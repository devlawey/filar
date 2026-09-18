//! `ReadOnlyExecutor` — hard transport-level gate that refuses every command
//! outside a built-in allowlist (#419).
//!
//! The allowlist is compiled into the binary and deliberately not
//! configurable: it is a security boundary, not a convenience default.
//! A command is forwarded to the inner executor only when **every** simple
//! command segment starts with an allowlisted binary **and** the command
//! contains no construct that could smuggle in another one — command
//! substitution, process substitution, heredocs, input redirection,
//! background execution — and no output redirection to anything but
//! `/dev/null` or a bare file descriptor.
//!
//! The gate validates *literal text only*: characters the remote shell
//! would re-lex or expand are refused outright, because these checks see
//! the command **before** the host shell does — `$X`, `${X}`, `$'…'`
//! (parameter and ANSI-C expansion), `\` (escape removal), `'`/`"` (quote
//! removal), `{`/`}` (brace expansion) and `*`/`?`/`[`/`]` (pathname
//! expansion) can each turn an argument that passed the checks into a
//! different one remotely (`file ${X:---compile}` and `file --compil{e,}`
//! both become `file --compile`). `~` is tolerated: it expands to a path,
//! never to a flag.
//!
//! Refusal happens *before* the command reaches the wrapped executor, so
//! nothing is ever sent to the host.
//!
//! This is the second, independent layer under the confirmation gate in
//! `filar-agent`'s `security.rs`: that gate decides whether the *user* is
//! asked; this one decides that a read-only host may never see a write,
//! regardless of who or what issued the command. The interactive terminal
//! (Ctrl+T) does not pass through `CommandExecutor` — it is the user's own
//! direct input and is intentionally out of scope here.
//!
//! Over-strictness is intentional (fail-closed): a quoted `;` is refused
//! rather than interpreted, and fd duplication must be spelled compactly
//! (`2>&1`) — a spaced `>& 1` is not recognised and is refused.

use std::sync::Arc;

use tokio::sync::mpsc;

use filar_core::{CoreError, Result};

use crate::{CommandExecutor, CommandResult, StreamEvent};

/// Binaries a read-only session may execute. A bare name matches; a leading
/// `/bin/`, `/usr/bin/`, `/usr/local/bin/`, `/sbin/`, `/usr/sbin/` or
/// `/usr/local/sbin/` is tolerated, any other path is not.
///
/// Every entry is a pure reader: no file writes, no process execution, no
/// signal delivery under any of its flags. The few readers that have an
/// opt-in write/execute flag (`sort -o`/`--compress-program`, `date -s`,
/// `file -C`) are covered by [`FORBIDDEN_ARGS`] in every spelling getopt
/// accepts: clusters (`sort -ro`), attached values (`sort -oFILE`) and
/// abbreviated long options (`sort --out=FILE`).
///
/// Deliberately absent, with intent: `find` (`-delete`, `-exec`), `sed`/`awk`
/// (`-i`, `system()`), `env`/`nice`/`timeout`/`xargs` (execute code),
/// `sudo`/`su`/`doas` (privilege escalation), `tee`/`cp`/`mv`/`rm`/`touch`
/// (writes), `ip`/`ifconfig`/`hostname` (configuration changes: `ip link
/// set`, `hostname NAME`).
pub const ALLOWED_COMMANDS: &[&str] = &[
    "base64", "cat", "cksum", "cmp", "comm", "cut", "date", "df", "diff", "dig", "du", "echo",
    "egrep", "fgrep", "file", "free", "grep", "groups", "head", "host", "id", "ls", "lsblk",
    "lscpu", "lspci", "lsusb", "md5sum", "netstat", "nl", "nslookup", "od", "ping", "ping6",
    "printenv", "printf", "ps", "pwd", "sha1sum", "sha256sum", "sha512sum", "sort", "ss", "stat",
    "strings", "tac", "tail", "tr", "uname", "uniq", "uptime", "wc", "which", "who", "whoami",
];

/// Arguments that turn an otherwise read-only binary into a writer or an
/// execution vector: `(binary, long option names, short option letters)`,
/// checked against every argument of the segment.
///
/// Long options are matched by *name* (the part before `=`), and the token
/// is refused when the name or the stored option is an abbreviation of the
/// other — GNU getopt accepts unambiguous abbreviations (`sort --out=`),
/// so the match cannot be one-sided. Short options are matched per letter,
/// not per exact token: in a cluster (`sort -ro`) the letter is a real
/// option, getopt treats the rest as its argument, and the attached form
/// (`sort -oFILE`) is covered by the same scan. Both rules over-refuse a
/// few exotic-but-valid spellings (`sort -k2o`); fail-closed.
const FORBIDDEN_ARGS: &[(&str, &[&str], &[char])] = &[
    // `sort -o FILE` / `--output` write; `--compress-program` executes a helper.
    ("sort", &["--output", "--compress-program"], &['o']),
    // `date -s` / `--set` sets the system clock.
    ("date", &["--set"], &['s']),
    // `file -C` / `--compile` writes a compiled magic database.
    ("file", &["--compile"], &['C']),
];

/// Standard binary directories whose prefix is stripped before the allowlist
/// check, so `/usr/bin/ls` is accepted like `ls`.
const BIN_PREFIXES: &[&str] = &[
    "/bin/",
    "/usr/bin/",
    "/usr/local/bin/",
    "/sbin/",
    "/usr/sbin/",
    "/usr/local/sbin/",
];

/// Validate `command` against the read-only allowlist.
///
/// `Ok(())` means the command may be forwarded to the host verbatim;
/// `Err(reason)` is a human-readable explanation of the first violation.
pub fn check_read_only(command: &str) -> std::result::Result<(), String> {
    let cmd = command.trim();
    if cmd.is_empty() {
        return Err("empty command".into());
    }

    // Substitution constructs can hide anything — refuse outright, before
    // any segmentation, and never try to interpret what is inside.
    if cmd.contains('`') {
        return Err("backtick command substitution is not allowed".into());
    }
    if cmd.contains("$(") {
        return Err("$() command substitution is not allowed".into());
    }
    if cmd.contains("<(") || cmd.contains(">(") {
        return Err("process substitution is not allowed".into());
    }
    check_reinterpretable_characters(cmd)?;

    check_ampersands(cmd)?;
    check_redirections(cmd)?;

    let mut segments_checked = 0usize;
    for segment in segments(cmd) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        segments_checked += 1;
        check_segment(segment)?;
    }
    if segments_checked == 0 {
        return Err("empty command".into());
    }
    Ok(())
}

/// Refuse every character the remote shell would re-lex or expand (see the
/// module docs). These checks run on literal text, so any of these
/// characters would make them meaningless: after validation the host shell
/// can still turn a checked argument into a write flag. `~` is not listed —
/// tilde expansion yields a path, never a flag.
fn check_reinterpretable_characters(cmd: &str) -> std::result::Result<(), String> {
    for c in ['$', '\\', '\'', '"', '{', '}', '*', '?', '[', ']'] {
        if cmd.contains(c) {
            return Err(format!(
                "shell metacharacter `{c}` is not allowed in read-only commands (literal text only)"
            ));
        }
    }
    Ok(())
}

/// Split on `;`, `&&`, `||`, `|` and newlines — the shell's command
/// separators. Every resulting segment is validated independently.
fn segments(cmd: &str) -> impl Iterator<Item = &str> {
    cmd.split([';', '\n'])
        .flat_map(|s| s.split("&&"))
        .flat_map(|s| s.split("||"))
        .flat_map(|s| s.split('|'))
}

/// Refuse `&` unless it is `&&` (a separator) or the fd-duplication form
/// `>&N` — e.g. `2>&1`.
fn check_ampersands(cmd: &str) -> std::result::Result<(), String> {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if bytes.get(i + 1) == Some(&b'&') {
                i += 2;
                continue;
            }
            let after_gt = i > 0 && bytes[i - 1] == b'>';
            let digit_follows = bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit());
            if after_gt {
                if digit_follows {
                    i += 1;
                    continue;
                }
                // `>& 1` (spaced) is not a portable spelling across shells;
                // only the compact `2>&1` form is recognised by the gate.
                return Err(
                    "`>&` must be followed directly by a file descriptor digit (`2>&1`)".into(),
                );
            }
            return Err("background execution (&) is not allowed".into());
        }
        i += 1;
    }
    Ok(())
}

/// Refuse `<` entirely (input redirection, heredocs) and `>` unless it is
/// fd duplication (`>&N`) or targets `/dev/null`.
fn check_redirections(cmd: &str) -> std::result::Result<(), String> {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                if cmd[i..].starts_with("<<") {
                    return Err("heredocs are not allowed".into());
                }
                return Err("input redirection is not allowed".into());
            }
            b'>' => {
                let rest = &cmd[i + 1..];
                if rest.starts_with('>') {
                    return Err("append redirection (>>) is not allowed".into());
                }
                if let Some(after_amp) = rest.strip_prefix('&') {
                    if !after_amp.starts_with(|c: char| c.is_ascii_digit()) {
                        return Err("`>&` must target a file descriptor".into());
                    }
                } else {
                    // `/dev/null` must be the whole target: `/dev/nullx` is a
                    // different (writable) file and must not slip through a
                    // prefix check.
                    let target = rest.trim_start();
                    let null_ok = target.strip_prefix("/dev/null").is_some_and(|tail| {
                        tail.is_empty()
                            || tail.starts_with(|c: char| {
                                c.is_whitespace() || c == ';' || c == '|' || c == '&'
                            })
                    });
                    if !null_ok {
                        return Err("output redirection is only allowed to /dev/null".into());
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

/// Validate one simple command segment: its first token must be an
/// allowlisted binary, and its arguments must not carry a write/execute flag.
fn check_segment(segment: &str) -> std::result::Result<(), String> {
    let token = segment
        .split_whitespace()
        .next()
        .ok_or_else(|| "empty command".to_string())?;
    let base = strip_bin_prefix(token);
    if !ALLOWED_COMMANDS.contains(&base) {
        return Err(format!("\"{token}\" is not in the read-only allowlist"));
    }
    if let Some((_, long_opts, short_chars)) =
        FORBIDDEN_ARGS.iter().find(|(bin, _, _)| *bin == base)
    {
        for arg in segment.split_whitespace().skip(1) {
            if let Some(bad) = forbidden_flag(arg, long_opts, short_chars) {
                return Err(format!("\"{base} {bad}\" is not read-only"));
            }
        }
    }
    Ok(())
}

/// Detect a write/execute flag in one argument of a guarded binary, in the
/// spellings getopt accepts (see [`FORBIDDEN_ARGS`]): long options are
/// compared by name with the abbreviation rule, short options by letter
/// anywhere in a cluster (or in an attached value, `-oFILE`).
fn forbidden_flag(arg: &str, long_opts: &[&str], short_chars: &[char]) -> Option<String> {
    if let Some(body) = arg.strip_prefix("--") {
        // A bare `--` ends option parsing; what follows is an operand.
        if body.is_empty() {
            return None;
        }
        let name = body.split('=').next().unwrap_or(body);
        if name.is_empty() {
            return None;
        }
        return long_opts
            .iter()
            .find(|opt| {
                let bare = opt.trim_start_matches('-');
                bare.starts_with(name) || name.starts_with(bare)
            })
            .map(|opt| (*opt).to_string());
    }
    if arg.starts_with('-') && arg.len() > 1 {
        if let Some(c) = short_chars.iter().find(|c| arg[1..].contains(**c)) {
            return Some(format!("-{c}"));
        }
    }
    None
}

/// Strip one of the standard binary-directory prefixes, if present.
/// The remainder is matched against the allowlist verbatim, so any
/// remaining path (`/usr/bin/../evil/ls`, `./ls`) fails the check.
fn strip_bin_prefix(token: &str) -> &str {
    for prefix in BIN_PREFIXES {
        if let Some(rest) = token.strip_prefix(prefix) {
            return rest;
        }
    }
    token
}

// ---------------------------------------------------------------------------
// ReadOnlyExecutor
// ---------------------------------------------------------------------------

/// [`CommandExecutor`] wrapper that only lets allowlisted commands through
/// to the inner executor (see the module docs for the rules).
///
/// Forbidden commands are refused *before* the inner executor is called —
/// they never reach the host.
///
/// Wrapping contract (security): this gate belongs **under** the secret
/// layer — `SecretSubstitutingExecutor` wraps it, never the other way
/// round. Two things rely on that order: the gate validates the text as
/// it will go to the wire (secrets already substituted), and the refusal
/// message — which echoes the command — passes back out through the
/// secret layer, which scrubs secret values from error strings. Keep it.
pub struct ReadOnlyExecutor {
    inner: Arc<dyn CommandExecutor>,
}

impl ReadOnlyExecutor {
    /// Create a `ReadOnlyExecutor` wrapping `inner`.
    pub fn new(inner: Arc<dyn CommandExecutor>) -> Self {
        Self { inner }
    }
}

/// Build the refusal error for a command that failed [`check_read_only`].
///
/// The command text is echoed by design (the user must see what was not
/// sent); the secret layer above scrubs substituted values out of error
/// strings — see the wrapping contract on [`ReadOnlyExecutor`].
fn refuse(command: &str, reason: &str) -> CoreError {
    CoreError::Other(format!(
        "read-only policy: {reason} — command not sent to the host: {}",
        command.trim()
    ))
}

#[async_trait::async_trait]
impl CommandExecutor for ReadOnlyExecutor {
    async fn run(&self, command: &str) -> Result<CommandResult> {
        check_read_only(command).map_err(|reason| refuse(command, &reason))?;
        self.inner.run(command).await
    }

    async fn run_streaming(&self, command: &str) -> Result<mpsc::Receiver<StreamEvent>> {
        // Forward to the inner *streaming* path (the trait default would
        // flatten the stream through `run` and lose incremental output).
        check_read_only(command).map_err(|reason| refuse(command, &reason))?;
        self.inner.run_streaming(command).await
    }

    async fn cancel(&self) -> Result<()> {
        self.inner.cancel().await
    }

    async fn set_cwd(&self, path: &str) -> Result<()> {
        // Transport bookkeeping for Ctrl+T / OSC 7, not a shell command.
        self.inner.set_cwd(path).await
    }

    async fn current_cwd(&self) -> Option<String> {
        self.inner.current_cwd().await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Mock executor recording what the gate forwarded to it.
    struct CountingExecutor {
        calls: AtomicUsize,
        streaming_calls: AtomicUsize,
        cancelled: AtomicBool,
        last_command: Mutex<Option<String>>,
        cwd: Mutex<Option<String>>,
    }

    impl CountingExecutor {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                streaming_calls: AtomicUsize::new(0),
                cancelled: AtomicBool::new(false),
                last_command: Mutex::new(None),
                cwd: Mutex::new(None),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn streaming_calls(&self) -> usize {
            self.streaming_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl CommandExecutor for CountingExecutor {
        async fn run(&self, command: &str) -> Result<CommandResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_command.lock().unwrap() = Some(command.to_string());
            Ok(CommandResult {
                stdout: format!("ran: {command}"),
                stderr: String::new(),
                exit_code: Some(0),
                duration: std::time::Duration::from_millis(1),
                cwd: None,
            })
        }

        async fn run_streaming(&self, command: &str) -> Result<mpsc::Receiver<StreamEvent>> {
            // Deliberately not delegating to `run()` — the gate must forward
            // to the inner streaming path, so counting them separately
            // catches a regression to the trait default.
            self.streaming_calls.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel(4);
            let cmd = command.to_string();
            tokio::spawn(async move {
                let _ = tx.send(StreamEvent::Stdout(cmd)).await;
                let _ = tx.send(StreamEvent::Exit(Some(0))).await;
            });
            Ok(rx)
        }

        async fn cancel(&self) -> Result<()> {
            self.cancelled.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn set_cwd(&self, path: &str) -> Result<()> {
            *self.cwd.lock().unwrap() = Some(path.to_string());
            Ok(())
        }

        async fn current_cwd(&self) -> Option<String> {
            self.cwd.lock().unwrap().clone()
        }
    }

    fn gated() -> (ReadOnlyExecutor, Arc<CountingExecutor>) {
        let inner = Arc::new(CountingExecutor::new());
        let exec = ReadOnlyExecutor::new(inner.clone());
        (exec, inner)
    }

    /// Assert the command is refused by policy and the inner executor never
    /// sees a call.
    async fn assert_refused(exec: &ReadOnlyExecutor, inner: &Arc<CountingExecutor>, command: &str) {
        let before = inner.calls();
        let err = exec.run(command).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("read-only policy"),
            "expected a policy refusal for {command:?}, got: {msg}"
        );
        assert_eq!(
            inner.calls(),
            before,
            "refused command {command:?} must not reach the inner executor"
        );
    }

    #[tokio::test]
    async fn allowed_command_is_forwarded_and_executes() {
        let (exec, inner) = gated();
        let result = exec.run("ls -la /var/log").await.unwrap();
        assert_eq!(inner.calls(), 1);
        assert_eq!(
            inner.last_command.lock().unwrap().as_deref(),
            Some("ls -la /var/log")
        );
        assert_eq!(result.exit_code, Some(0));
    }

    #[tokio::test]
    async fn forbidden_command_is_refused_before_send() {
        let (exec, inner) = gated();
        assert_refused(&exec, &inner, "rm -rf /var/tmp/x").await;
        assert_refused(&exec, &inner, "sudo ls").await;
        assert_refused(&exec, &inner, "sh -c 'rm -rf /'").await;
        assert_refused(&exec, &inner, "env ls").await;
        assert_refused(&exec, &inner, "xargs ls").await;
        assert_refused(&exec, &inner, "tee /tmp/x").await;
        assert_refused(&exec, &inner, "find / -name x -delete").await;
    }

    #[tokio::test]
    async fn separator_bypasses_are_all_refused() {
        let (exec, inner) = gated();
        for cmd in [
            "ls; rm -rf /tmp/x",
            "ls && rm -rf /tmp/x",
            "ls || rm -rf /tmp/x",
            "ls | rm -rf /tmp/x",
            "ls\nrm -rf /tmp/x",
            "cat /etc/passwd & rm -rf /tmp/x",
            "ls && ls && rm -rf /tmp/x",
        ] {
            assert_refused(&exec, &inner, cmd).await;
        }
    }

    #[tokio::test]
    async fn allowed_pipeline_is_forwarded() {
        let (exec, inner) = gated();
        exec.run("ls -la | grep -v ssh | wc -l").await.unwrap();
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test]
    async fn substitution_bypasses_are_refused() {
        let (exec, inner) = gated();
        for cmd in [
            "echo $(rm -rf /tmp/x)",
            "echo `rm -rf /tmp/x`",
            "cat <(rm -rf /tmp/x)",
            "echo $(whoami)",
            "ls `uname`",
        ] {
            assert_refused(&exec, &inner, cmd).await;
        }
    }

    #[tokio::test]
    async fn shell_expansion_is_refused_everywhere() {
        let (exec, inner) = gated();
        assert_refused(&exec, &inner, "$CMD ls").await;
        assert_refused(&exec, &inner, "\"ls\"").await;
        assert_refused(&exec, &inner, "FOO=1 ls").await;
        // The gate runs *before* the host shell: variables in arguments are
        // expanded remotely, so a literal check means nothing — a checked
        // `${X:---compile}` becomes `--compile` on the other side.
        assert_refused(&exec, &inner, "ls $HOME/dir").await;
        assert_refused(&exec, &inner, "grep -r pattern ${HOME}/dir").await;
        assert_refused(&exec, &inner, "file ${X:---compile}").await;
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn reinterpreted_characters_are_refused() {
        let (exec, inner) = gated();
        // Quote removal, escape processing, brace and pathname expansion all
        // happen on the host *after* this gate, so they are refused outright.
        for cmd in [
            "file $'--compile'",
            "file \"--compile\"",
            "file --compil\\e",
            "file --compil{e,}",
            "sort {-o,/tmp/x} file",
            "ls /var/log/*.log",
            "cat /etc/hosts?",
            "grep -E cho[rm]x /etc/hosts",
            "echo back\\slash",
        ] {
            assert_refused(&exec, &inner, cmd).await;
        }
    }

    #[tokio::test]
    async fn redirection_rules() {
        let (exec, inner) = gated();
        for cmd in [
            "ls > /tmp/x",
            "ls >> /tmp/x",
            "ls >/tmp/x",
            "ls >/dev/nullx",
            "ls >/dev/zero",
            "cat < /etc/passwd",
            "cat <<EOF",
            "ls &",
            "ls &> /tmp/x",
            "echo hi >& /tmp/x",
            "ls >& 1",
        ] {
            assert_refused(&exec, &inner, cmd).await;
        }
        // /dev/null and fd duplication stay allowed — they are not writes.
        exec.run("ls 2>/dev/null").await.unwrap();
        exec.run("ls 2>&1").await.unwrap();
        exec.run("ls >/dev/null").await.unwrap();
        assert_eq!(inner.calls(), 3);
    }

    #[tokio::test]
    async fn bin_path_prefix_is_tolerated_but_not_arbitrary_paths() {
        let (exec, inner) = gated();
        exec.run("/bin/ls /tmp").await.unwrap();
        exec.run("/usr/bin/ls /tmp").await.unwrap();
        exec.run("/usr/local/bin/ls /tmp").await.unwrap();
        assert_eq!(inner.calls(), 3);
        assert_refused(&exec, &inner, "/tmp/ls").await;
        assert_refused(&exec, &inner, "./ls").await;
        assert_refused(&exec, &inner, "/usr/bin/../sbin/rm").await;
    }

    #[tokio::test]
    async fn write_flags_on_readers_are_refused() {
        let (exec, inner) = gated();
        assert_refused(&exec, &inner, "sort -o /tmp/x file").await;
        assert_refused(&exec, &inner, "sort --output=/tmp/x file").await;
        assert_refused(&exec, &inner, "sort --compress-program=evil file").await;
        // Getopt spellings that would defeat an exact-token check: a short
        // cluster, the attached-value form, and abbreviated long options.
        assert_refused(&exec, &inner, "sort -ro /tmp/x file").await;
        assert_refused(&exec, &inner, "sort -o/tmp/x file").await;
        assert_refused(&exec, &inner, "sort --out=/tmp/x file").await;
        assert_refused(&exec, &inner, "date -s 12:00").await;
        assert_refused(&exec, &inner, "date -us 12:00").await;
        assert_refused(&exec, &inner, "date -s2026-12-31").await;
        assert_refused(&exec, &inner, "date --se=2026-12-31").await;
        assert_refused(&exec, &inner, "file -C").await;
        assert_refused(&exec, &inner, "file -Cb").await;
        exec.run("sort file").await.unwrap();
        exec.run("sort -k2 -n file").await.unwrap();
        exec.run("date +%H:%M").await.unwrap();
        exec.run("date -u").await.unwrap();
        exec.run("file /etc/passwd").await.unwrap();
        exec.run("file -b /etc/hosts").await.unwrap();
        assert_eq!(inner.calls(), 6);
    }

    #[tokio::test]
    async fn empty_and_structure_only_commands_are_refused() {
        let (exec, inner) = gated();
        assert_refused(&exec, &inner, "").await;
        assert_refused(&exec, &inner, "   ").await;
        assert_refused(&exec, &inner, ";").await;
        assert_refused(&exec, &inner, "&&").await;
    }

    #[tokio::test]
    async fn trailing_separator_is_tolerated() {
        let (exec, inner) = gated();
        exec.run("ls;").await.unwrap();
        exec.run("ls &&").await.unwrap();
        assert_eq!(inner.calls(), 2);
    }

    #[tokio::test]
    async fn quotes_are_refused_outright() {
        let (exec, inner) = gated();
        // Quotes are refused by the metacharacter rule (quote removal could
        // change an argument), and a quoted separator would additionally be
        // split like a real one — over-strict by design either way.
        assert_refused(&exec, &inner, "grep \"a;b\" file").await;
        assert_refused(&exec, &inner, "echo 'a|b'").await;
    }

    #[tokio::test]
    async fn run_streaming_refuses_before_inner() {
        let (exec, inner) = gated();
        let err = exec.run_streaming("rm -rf /tmp/x").await.unwrap_err();
        assert!(err.to_string().contains("read-only policy"));
        assert_eq!(inner.streaming_calls(), 0);
        assert_eq!(inner.calls(), 0);
    }

    #[tokio::test]
    async fn run_streaming_allowed_forwards_to_streaming_path() {
        let (exec, inner) = gated();
        let mut rx = exec.run_streaming("tail -n 10 /var/log/syslog").await.unwrap();
        assert_eq!(inner.streaming_calls(), 1, "must use the inner streaming path");
        match rx.recv().await {
            Some(StreamEvent::Stdout(chunk)) => assert_eq!(chunk, "tail -n 10 /var/log/syslog"),
            other => panic!("expected stdout chunk, got {other:?}"),
        }
        assert!(matches!(rx.recv().await, Some(StreamEvent::Exit(Some(0)))));
    }

    #[tokio::test]
    async fn cancel_set_cwd_and_current_cwd_forward() {
        let (exec, inner) = gated();
        exec.set_cwd("/srv/app").await.unwrap();
        assert_eq!(exec.current_cwd().await.as_deref(), Some("/srv/app"));
        exec.cancel().await.unwrap();
        assert!(inner.cancelled.load(Ordering::SeqCst));
    }

    /// Real (non-mock) execution through the local executor: allowed
    /// commands actually run; a write is refused before execution.
    #[cfg(feature = "local")]
    #[tokio::test]
    async fn real_local_executor_allows_reads_and_refuses_writes() {
        let local: Arc<dyn CommandExecutor> =
            Arc::new(crate::LocalExecutor::new().await.unwrap());
        let exec = ReadOnlyExecutor::new(local);

        let result = exec.run("echo filar-readonly-ok").await.unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("filar-readonly-ok"));

        let err = exec.run("rm -rf /tmp/filar-readonly-never").await.unwrap_err();
        assert!(err.to_string().contains("read-only policy"));
    }
}
