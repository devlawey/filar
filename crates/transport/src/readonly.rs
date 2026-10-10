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
/// (writes), `ifconfig`/`hostname` (configuration changes: `hostname
/// NAME`). Binaries that are worth reading through but can also change the
/// host — `systemctl`, `journalctl`, `dmesg`, `ip`, `dpkg`, `rpm`, `sshd` —
/// are not here either: they are [`GUARDED_FORMS`], allowed only in the
/// argument shapes spelled out there.
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
    if let Some(form) = GUARDED_FORMS.iter().find(|f| f.bin == base) {
        let args = without_redirections(segment.split_whitespace().skip(1));
        return check_form(form, &args).map_err(|why| {
            format!("\"{base}\" is only allowed in its read-only forms: {why}")
        });
    }
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

// ---------------------------------------------------------------------------
// Guarded forms: binaries that can change the host, allowed only as readers
// ---------------------------------------------------------------------------

/// What may stand where a guarded binary takes free words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Words {
    /// No free words at all.
    None,
    /// Any word that does not start with `-` (unit names, packages, paths).
    Any,
}

/// A binary that has writing or executing modes, allowed **only** in the
/// argument shapes listed here (#504).
///
/// Unlike [`FORBIDDEN_ARGS`] — a deny-list for binaries that are readers
/// but for one flag — this is an allow-list: every argument must be
/// recognised, and anything else refuses the whole command. A new flag of a
/// future version, an abbreviation, a cluster, a subcommand not named here
/// are all refused without anyone having to think of them.
struct GuardedForm {
    bin: &'static str,
    /// Options without a value, matched as whole tokens — no clusters, no
    /// abbreviations.
    flags: &'static [&'static str],
    /// Options with a value: a long one as `--name=VALUE`, a short one as
    /// `-x VALUE`. The value is one literal word.
    valued: &'static [&'static str],
    /// Subcommands (the first free word). Empty: the binary has none.
    subcommands: &'static [&'static str],
    /// Whether a subcommand must be given (`ip`), or the binary reads
    /// without one too (`systemctl --failed`).
    subcommand_required: bool,
    /// The free words after the subcommand (or all of them, without one).
    words: Words,
    /// At least one of these must be present — the option that puts the
    /// binary into its reading mode (`dpkg -l`). Empty: no such need.
    needs_one_of: &'static [&'static str],
}

const GUARDED_FORMS: &[GuardedForm] = &[
    // Unit state, never unit control: no start/stop/restart/enable/mask/
    // edit/set-property/daemon-reload/kill/isolate, no `-H`/`-M` (another
    // host or container), no `--root`.
    GuardedForm {
        bin: "systemctl",
        flags: &[
            "--failed", "--no-pager", "--all", "-a", "--full", "-l", "--no-legend", "--plain",
            "--quiet", "-q", "--recursive", "-r", "--reverse", "--with-dependencies",
        ],
        valued: &["--type", "-t", "--state", "--property", "-p", "--lines", "-n", "--output", "-o"],
        subcommands: &[
            "status", "is-active", "is-enabled", "is-failed", "is-system-running", "list-units",
            "list-unit-files", "list-timers", "list-sockets", "list-dependencies", "show", "cat",
            "get-default",
        ],
        subcommand_required: false,
        words: Words::Any,
        needs_one_of: &[],
    },
    // The journal, read only: no `--rotate`, `--vacuum-*`, `--flush`,
    // `--sync`, `--relinquish-var`, `--setup-keys`, `--update-catalog`; no
    // `-f` (never ends); no `-D`/`--file`/`--root`/`-M` (other journals).
    GuardedForm {
        bin: "journalctl",
        flags: &[
            "--no-pager", "-b", "--boot", "-k", "--dmesg", "-x", "--catalog", "-e", "--pager-end",
            "-r", "--reverse", "-a", "--all", "-q", "--quiet", "--utc", "--no-hostname",
            "--system", "--list-boots", "--disk-usage", "--no-full",
        ],
        valued: &[
            "--unit", "-u", "--lines", "-n", "--priority", "-p", "--since", "-S", "--until", "-U",
            "--output", "-o", "--grep", "-g", "--identifier", "-t", "--boot", "--facility",
        ],
        subcommands: &[],
        subcommand_required: false,
        words: Words::None,
        needs_one_of: &[],
    },
    // The kernel ring buffer, read only: no `-C`/`-c` (clear), `-n`/`-D`/`-E`
    // (console level), `-w`/`-W` (never ends).
    GuardedForm {
        bin: "dmesg",
        flags: &[
            "-T", "--ctime", "-H", "--human", "-k", "--kernel", "-u", "--userspace", "-x",
            "--decode", "-t", "--notime", "-e", "--reltime", "-r", "--raw", "--nopager", "-P",
        ],
        valued: &["--level", "-l", "--facility", "-f", "--since", "--until"],
        subcommands: &[],
        subcommand_required: false,
        words: Words::None,
        needs_one_of: &[],
    },
    // Network state: `ip OBJECT [show|list|get …]`. The object and the
    // command are whole words — `ip link s` is `set`, not `show`. No
    // `-batch`, `-force`, `-netns`; no `netns`, `tuntap`, `xfrm`, `monitor`.
    // The command is checked by `check_ip`.
    GuardedForm {
        bin: "ip",
        flags: &[
            "-4", "-6", "-br", "-brief", "-s", "-stats", "-statistics", "-d", "-details", "-o",
            "-oneline", "-j", "-json", "-p", "-pretty", "-h", "-human", "-c", "-color",
        ],
        valued: &[],
        subcommands: &[
            "a", "addr", "address", "l", "link", "r", "ro", "route", "n", "neigh", "neighbor",
            "neighbour", "rule", "maddr", "maddress",
        ],
        subcommand_required: true,
        words: Words::Any,
        needs_one_of: &[],
    },
    // Package state: no install, remove, purge, configure, no `--root`,
    // `--admindir`, `--force-*`.
    GuardedForm {
        bin: "dpkg",
        flags: &[
            "-l", "--list", "-s", "--status", "-L", "--listfiles", "-S", "--search", "-p",
            "--print-avail", "--get-selections", "--print-architecture", "--version", "--no-pager",
        ],
        valued: &[],
        subcommands: &[],
        subcommand_required: false,
        words: Words::Any,
        needs_one_of: &[
            "-l", "--list", "-s", "--status", "-L", "--listfiles", "-S", "--search", "-p",
            "--print-avail", "--get-selections", "--print-architecture", "--version",
        ],
    },
    GuardedForm {
        bin: "dpkg-query",
        flags: &[
            "-l", "--list", "-W", "--show", "-s", "--status", "-L", "--listfiles", "-S",
            "--search", "-p", "--print-avail", "--version", "--no-pager",
        ],
        valued: &[],
        subcommands: &[],
        subcommand_required: false,
        words: Words::Any,
        needs_one_of: &[
            "-l", "--list", "-W", "--show", "-s", "--status", "-L", "--listfiles", "-S",
            "--search", "-p", "--print-avail", "--version",
        ],
    },
    // `rpm` in query mode only; the mode letters are spelled out, so no
    // cluster can hide `-e`, `-i` (install without `-q`), `-U`, `-p` (reads a
    // package file or URL), and `--pipe`/`--eval`/`--root` are not listed.
    GuardedForm {
        bin: "rpm",
        flags: &[
            "-q", "--query", "-qa", "-qi", "-ql", "-qf", "-qc", "-qd", "-qR", "-qal", "-qil",
            "--all", "--info", "--list", "--file", "--requires", "--provides", "--whatprovides",
            "--whatrequires", "--changelog", "--last", "--version",
        ],
        valued: &[],
        subcommands: &[],
        subcommand_required: false,
        words: Words::Any,
        needs_one_of: &[
            "-q", "--query", "-qa", "-qi", "-ql", "-qf", "-qc", "-qd", "-qR", "-qal", "-qil",
            "--version",
        ],
    },
    // Version only: any other `sshd` argument starts a daemon, any other
    // `ssh` argument connects somewhere.
    GuardedForm {
        bin: "sshd",
        flags: &["-V"],
        valued: &[],
        subcommands: &[],
        subcommand_required: false,
        words: Words::None,
        needs_one_of: &["-V"],
    },
    GuardedForm {
        bin: "ssh",
        flags: &["-V"],
        valued: &[],
        subcommands: &[],
        subcommand_required: false,
        words: Words::None,
        needs_one_of: &["-V"],
    },
    // Compressed logs and changelogs to stdout; no options at all.
    GuardedForm {
        bin: "zcat",
        flags: &[],
        valued: &[],
        subcommands: &[],
        subcommand_required: false,
        words: Words::Any,
        needs_one_of: &[],
    },
];

/// The `ip` commands that only read. Whole words: iproute2 takes any prefix
/// of a command name, and `s` after `link` is `set`.
const IP_READ_COMMANDS: &[&str] = &["show", "list", "lst", "get"];

/// The arguments of a segment without its redirections (`2>&1`,
/// `2>/dev/null`, `> /dev/null`): [`check_redirections`] has already
/// allowed exactly those, and they are not arguments of the binary.
///
/// A redirection can be glued to an argument — the shell reads
/// `--rotate>/dev/null` as the argument `--rotate` plus a redirection — so
/// whatever stands before the `>` in a token is kept as an argument, unless
/// it is only digits, which is the file descriptor being redirected
/// (`2>&1`). Quotes and backslashes never get here
/// ([`check_reinterpretable_characters`]), so a `>` is always an operator.
fn without_redirections<'a>(tokens: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut args = Vec::new();
    let mut target_follows = false;
    for token in tokens {
        if std::mem::take(&mut target_follows) {
            continue;
        }
        let Some(at) = token.find('>') else {
            args.push(token);
            continue;
        };
        let before = &token[..at];
        if !before.is_empty() && !before.bytes().all(|b| b.is_ascii_digit()) {
            args.push(before);
        }
        target_follows = token.ends_with('>');
    }
    args
}

/// Validate the arguments of a guarded binary against its form: every
/// argument must be recognised. `Err` names the first one that is not.
fn check_form(form: &GuardedForm, args: &[&str]) -> std::result::Result<(), String> {
    let mut subcommand: Option<&str> = None;
    let mut words_after = 0usize;
    let mut satisfied = form.needs_one_of.is_empty();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        i += 1;
        if let Some(rest) = arg.strip_prefix('-') {
            // Options belong before the free words for `ip` (after the
            // object everything is a selector); elsewhere the position does
            // not change the meaning.
            if form.bin == "ip" && subcommand.is_some() {
                return Err(format!("`{arg}` after the object"));
            }
            if rest.is_empty() || arg == "--" {
                return Err(format!("`{arg}` is not accepted"));
            }
            if form.flags.contains(&arg) {
                satisfied |= form.needs_one_of.contains(&arg);
                // `journalctl -b -1`: the boot offset is an optional value
                // of its own, and the only one that starts with a dash.
                if form.bin == "journalctl" && arg == "-b" && args.get(i).is_some_and(|v| is_boot_offset(v)) {
                    i += 1;
                }
                continue;
            }
            // `--name=VALUE`
            if let Some((name, value)) = arg.split_once('=') {
                if name.starts_with("--") && form.valued.contains(&name) && !value.is_empty() {
                    continue;
                }
                return Err(format!("`{name}` is not an accepted option"));
            }
            // `-x VALUE` / `--name VALUE`: the value is the next word.
            if form.valued.contains(&arg) {
                match args.get(i) {
                    Some(value) if !value.starts_with('-') => {
                        i += 1;
                        continue;
                    }
                    _ => return Err(format!("`{arg}` needs a value")),
                }
            }
            return Err(format!("`{arg}` is not an accepted option"));
        }
        if !form.subcommands.is_empty() && subcommand.is_none() {
            if !form.subcommands.contains(&arg) {
                return Err(format!("`{arg}` is not a read-only subcommand"));
            }
            subcommand = Some(arg);
            continue;
        }
        if form.words == Words::None {
            return Err(format!("`{arg}` is not accepted"));
        }
        if form.bin == "ip" && words_after == 0 && !IP_READ_COMMANDS.contains(&arg) {
            return Err(format!("`{arg}` is not a read-only command (use show, list or get)"));
        }
        words_after += 1;
    }
    if form.subcommand_required && subcommand.is_none() {
        return Err("an object is required".into());
    }
    if !satisfied {
        return Err(format!("one of {} is required", form.needs_one_of.join(", ")));
    }
    Ok(())
}

/// A `journalctl -b` offset: `0`, `1`, `-1`, `-20`.
fn is_boot_offset(word: &str) -> bool {
    let digits = word.strip_prefix('-').unwrap_or(word);
    !digits.is_empty() && digits.len() <= 4 && digits.bytes().all(|b| b.is_ascii_digit())
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

    /// #504: diagnostics an administrator reads a fleet with.
    #[tokio::test]
    async fn guarded_binaries_run_in_their_read_only_forms() {
        let (exec, inner) = gated();
        let allowed = [
            "systemctl --failed",
            "systemctl --failed --no-pager --no-legend",
            "systemctl status nginx",
            "systemctl status nginx.service sshd --no-pager -n 20",
            "systemctl is-active nginx",
            "systemctl is-enabled nginx",
            "systemctl list-units --type=service --state=running",
            "systemctl list-unit-files -t service",
            "systemctl show nginx --property=ActiveState",
            "systemctl cat nginx",
            "journalctl --no-pager -n 50",
            "journalctl -u nginx --since=today --no-pager",
            "journalctl -k -b --no-pager -p err",
            "journalctl --boot=-1 --lines=20",
            "journalctl -b -1 --no-pager",
            "journalctl -b 0 -p err",
            "dmesg -T>/dev/null",
            "systemctl status nginx 2>/dev/null",
            "journalctl --disk-usage",
            "dmesg",
            "dmesg -T --level=err,warn",
            "dmesg -H | tail -n 20",
            "ip a",
            "ip addr",
            "ip -br addr show",
            "ip -4 addr show dev eth0",
            "ip link show",
            "ip -s link show eth0",
            "ip route",
            "ip route show table all",
            "ip route get 8.8.8.8",
            "ip neigh show",
            "ip rule list",
            "/usr/sbin/ip -j addr",
            "dpkg -l",
            "dpkg -l openssh-server",
            "dpkg -s openssh-server",
            "dpkg -S /usr/sbin/sshd",
            "dpkg-query -W openssh-server",
            "rpm -q openssh-server",
            "rpm -qa",
            "rpm -qi openssh-server",
            "rpm -qa --last",
            "sshd -V",
            "sshd -V 2>&1",
            "dmesg -T 2>/dev/null",
            "journalctl --no-pager -n 5 2> /dev/null",
            "/usr/sbin/sshd -V",
            "ssh -V",
            "zcat /usr/share/doc/openssh-server/changelog.gz",
            "zcat /var/log/syslog.2.gz | grep -i error | tail -n 5",
        ];
        for cmd in allowed {
            exec.run(cmd).await.unwrap_or_else(|e| panic!("{cmd:?} must be allowed: {e}"));
        }
        assert_eq!(inner.calls(), allowed.len());
    }

    /// #504: the same binaries in any form that changes the host, reaches
    /// another one, runs something or never ends.
    #[tokio::test]
    async fn guarded_binaries_are_refused_in_every_other_form() {
        let (exec, inner) = gated();
        for cmd in [
            // systemctl: control, other hosts, unknown words.
            "systemctl restart nginx",
            "systemctl stop nginx",
            "systemctl start nginx",
            "systemctl enable nginx",
            "systemctl disable --now nginx",
            "systemctl mask nginx",
            "systemctl daemon-reload",
            "systemctl edit nginx",
            "systemctl set-property nginx CPUWeight=1",
            "systemctl kill nginx",
            "systemctl isolate rescue.target",
            "systemctl reboot",
            "systemctl poweroff",
            "systemctl -H root@other status nginx",
            "systemctl --host=other status",
            "systemctl -M box status",
            "systemctl --root=/mnt status",
            "systemctl nginx",
            "systemctl --failed restart nginx",
            "systemctl status nginx --now",
            "systemctl stat nginx",
            // journalctl: maintenance, follow, other journals.
            "journalctl --rotate",
            "journalctl --vacuum-time=1s",
            "journalctl --vacuum-size=1M",
            "journalctl --flush",
            "journalctl --sync",
            "journalctl --relinquish-var",
            "journalctl --setup-keys",
            "journalctl --update-catalog",
            "journalctl -f",
            "journalctl --follow",
            "journalctl -D /mnt/journal",
            "journalctl --file=/tmp/x.journal",
            "journalctl --root=/mnt",
            "journalctl -fu nginx",
            "journalctl /dev/sda",
            // dmesg: clearing, console level, follow.
            "dmesg -C",
            "dmesg --clear",
            "dmesg -c",
            "dmesg --read-clear",
            "dmesg -n 1",
            "dmesg --console-level=1",
            "dmesg -D",
            "dmesg -E",
            "dmesg -w",
            "dmesg -Tc",
            "dmesg extra",
            // ip: changes, abbreviations that mean `set`, batch, netns.
            "ip",
            "ip link set eth0 down",
            "ip link s eth0 down",
            "ip link se eth0 down",
            "ip addr add 10.0.0.1/24 dev eth0",
            "ip addr del 10.0.0.1/24 dev eth0",
            "ip addr flush dev eth0",
            "ip a f dev eth0",
            "ip route add default via 10.0.0.1",
            "ip route del default",
            "ip route replace default via 10.0.0.1",
            "ip route flush cache",
            "ip neigh flush all",
            "ip rule add from all lookup 1",
            "ip -batch /tmp/cmds",
            "ip -b /tmp/cmds",
            "ip -force -batch /tmp/cmds",
            "ip -n ns1 addr show",
            "ip netns exec ns1 ls",
            "ip netns list",
            "ip tuntap add dev tun0 mode tun",
            "ip monitor",
            "ip addr show -batch /tmp/x",
            // packages: anything but queries.
            "dpkg -i /tmp/x.deb",
            "dpkg --install /tmp/x.deb",
            "dpkg -r openssh-server",
            "dpkg -P openssh-server",
            "dpkg --purge openssh-server",
            "dpkg --configure -a",
            "dpkg -l --root=/mnt",
            "dpkg --force-all -l",
            "dpkg openssh-server",
            "dpkg-query --admindir=/tmp -l",
            "rpm -i /tmp/x.rpm",
            "rpm -U /tmp/x.rpm",
            "rpm -e openssh-server",
            "rpm -qp /tmp/x.rpm",
            "rpm -q --pipe sh x",
            "rpm -q -e openssh-server",
            "rpm --all",
            "rpm -q --root=/mnt x",
            "rpm --eval x",
            "rpm openssh-server",
            // ssh/sshd: only the version.
            "sshd",
            "sshd -D",
            "sshd -V -D",
            "sshd -V 2>&1 -D",
            "sshd -V > /tmp/x",
            "sshd 2>&1",
            "sshd -p 2222",
            "sshd -t",
            "ssh",
            "ssh other-host",
            "ssh -V other-host",
            "ssh -V -o ProxyCommand=x other",
            // zcat: no options.
            "zcat -f /etc/passwd",
            "zcat --force x",
        ] {
            assert_refused(&exec, &inner, cmd).await;
        }
    }

    /// #504 review: a redirection glued to an argument does not hide it —
    /// the shell runs `--rotate>/dev/null` as `--rotate`.
    #[tokio::test]
    async fn an_argument_glued_to_a_redirection_is_still_checked() {
        let (exec, inner) = gated();
        for cmd in [
            "journalctl --rotate>/dev/null",
            "journalctl --rotate> /dev/null",
            "journalctl --rotate>/dev/null 2>&1",
            "journalctl --vacuum-time=1s>/dev/null",
            "dmesg -C>/dev/null",
            "dmesg -c2>/dev/null",
            "sshd -D>/dev/null",
            "sshd -V -D2>&1",
            "systemctl restart>/dev/null nginx",
            "systemctl stop nginx>/dev/null",
            "ip link set>/dev/null eth0 down",
            "ip addr flush>/dev/null dev eth0",
            "dpkg -P>/dev/null openssh-server",
            "rpm -e>/dev/null openssh-server",
            "zcat -f>/dev/null /etc/passwd",
            "journalctl -b -1 --rotate",
            "journalctl -b --rotate",
            "journalctl -b -f",
        ] {
            assert_refused(&exec, &inner, cmd).await;
        }
        assert_eq!(without_redirections("-T 2>&1 >/dev/null -x".split_whitespace()), ["-T", "-x"]);
        assert_eq!(without_redirections("a> /dev/null b 12>&1".split_whitespace()), ["a", "b"]);
    }

    /// #504: a guarded binary never slips through behind an allowed one.
    #[tokio::test]
    async fn a_guarded_form_is_checked_in_every_segment() {
        let (exec, inner) = gated();
        assert_refused(&exec, &inner, "systemctl --failed; systemctl restart nginx").await;
        assert_refused(&exec, &inner, "ip a && ip link set eth0 down").await;
        assert_refused(&exec, &inner, "ls | dmesg -C").await;
        exec.run("systemctl --failed --no-pager | grep -c failed").await.unwrap();
        assert_eq!(inner.calls(), 1);
    }

    #[test]
    fn a_refusal_names_what_was_not_accepted() {
        let err = check_read_only("systemctl restart nginx").unwrap_err();
        assert!(err.contains("only allowed in its read-only forms"), "{err}");
        assert!(err.contains("`restart`"), "{err}");
        let err = check_read_only("ip link s eth0 down").unwrap_err();
        assert!(err.contains("show, list or get"), "{err}");
    }

    /// No binary is both a plain reader and a guarded one: the plain list
    /// would let every argument through.
    #[test]
    fn guarded_binaries_are_not_on_the_plain_allowlist() {
        for form in GUARDED_FORMS {
            assert!(!ALLOWED_COMMANDS.contains(&form.bin), "{}", form.bin);
            for needed in form.needs_one_of {
                assert!(form.flags.contains(needed), "{}: {needed} is not a flag", form.bin);
            }
        }
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
