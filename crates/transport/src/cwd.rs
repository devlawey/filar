//! Working-directory helpers shared by local and SSH transports.
//!
//! Used to sync the agent executor with the interactive PTY (and the reverse)
//! without writing files on the remote host.

/// Maximum length of a cwd we will accept from OSC 7, `$PWD`, or `set_cwd`.
pub const MAX_CWD_LEN: usize = 1024;

/// Host of the OSC 7 URIs filar's own hooks emit: the path after it is the
/// raw directory, **not** percent-encoded, so the reader must not decode it —
/// a directory literally named `build%20final` stays that (#482 review).
/// Shells' own OSC 7 (`localhost` or a hostname) is decoded as usual.
pub const OSC7_RAW_HOST: &str = "filar-raw";

/// Bytes written to a POSIX interactive shell to emit OSC 7 for the current pwd.
///
/// No files are created. The PTY is typically closed immediately after, so the
/// command does not stay in the user's session. The path is raw, hence
/// [`OSC7_RAW_HOST`].
pub const OSC7_PWD_PROBE: &[u8] =
    b"printf '\\033]7;file://filar-raw%s\\007' \"$(pwd)\"\n";

/// Where an interactive terminal's foreground program is, learned without
/// typing anything into the terminal (#499).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundCwd {
    /// Working directory of the foreground program (the shell itself while
    /// it waits at its prompt).
    pub path: String,
    /// Whether the shell that owns the terminal is in the foreground, i.e.
    /// no program started from it is still running.
    pub at_prompt: bool,
}

/// Prefix of the one line [`PTY_CWD_COMMAND`] prints.
const PTY_CWD_MARK: &str = "filar-cwd:";

/// Command for a **second** SSH channel (no PTY) that tells where the
/// foreground program of the connection's terminal is (#499).
///
/// Nothing is typed into the user's shell, so nothing shows in the terminal
/// or in the shell history, and no file is written: the script only reads
/// `/proc`. It finds the one process that has a terminal and is a child of
/// this connection's `sshd` (the interactive shell), takes the terminal's
/// foreground process group from its `stat`, and prints that process's
/// directory as `filar-cwd:<1 if the shell is in the foreground, else
/// 0>:<path>`. A host without `/proc` (macOS, the BSDs) prints nothing, and
/// the caller keeps what it knew.
///
/// One line, no single quotes or backslashes inside: the login shell that
/// runs it may be bash, dash, zsh, fish or csh.
pub const PTY_CWD_COMMAND: &str = concat!(
    "sh -c 'a=$$; anc=\" \"; i=0; ",
    "while [ \"$a\" -gt 1 ] && [ $i -lt 4 ]; do ",
    "{ read -r s < /proc/$a/stat; } 2>/dev/null || break; ",
    "s=${s##*) }; set -- $s; a=$2; [ \"$a\" -gt 1 ] && anc=\"$anc$a \"; i=$((i+1)); done; ",
    "n=0; p=; f=; for d in /proc/[0-9]*; do ",
    "{ read -r s < $d/stat; } 2>/dev/null || continue; ",
    "s=${s##*) }; set -- $s; [ \"$5\" != 0 ] || continue; ",
    "case \"$anc\" in *\" $2 \"*) ;; *) continue ;; esac; ",
    "n=$((n+1)); p=${d#/proc/}; f=$6; done; ",
    "[ $n = 1 ] || exit 0; q=0; [ \"$f\" = \"$p\" ] && q=1; ",
    "c=$(readlink /proc/$f/cwd 2>/dev/null) || c=$(readlink /proc/$p/cwd 2>/dev/null) || exit 0; ",
    "printf \"filar-cwd:%s:%s\" $q \"$c\"'",
);

/// Read the answer of [`PTY_CWD_COMMAND`]. `None` for anything else — an
/// empty reply, a banner, a directory that was removed, an unsafe path.
pub fn parse_pty_cwd_reply(reply: &str) -> Option<ForegroundCwd> {
    let rest = &reply[reply.rfind(PTY_CWD_MARK)? + PTY_CWD_MARK.len()..];
    let (flag, path) = rest.split_once(':')?;
    let at_prompt = match flag {
        "1" => true,
        "0" => false,
        _ => return None,
    };
    let path = usable_proc_cwd(path)?;
    Some(ForegroundCwd { path, at_prompt })
}

/// A `/proc/<pid>/cwd` link target as a cwd, or `None` if it is not an
/// absolute, safe path of a directory that still exists.
pub fn usable_proc_cwd(link: &str) -> Option<String> {
    let path = link.strip_suffix('\n').unwrap_or(link);
    if !path.starts_with('/') || path.ends_with(" (deleted)") || !is_safe_cwd(path) {
        return None;
    }
    Some(path.to_string())
}

/// How an interactive shell can tell its working directory (#482).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFlavor {
    /// sh, bash, zsh, …: probed on demand with [`OSC7_PWD_PROBE`].
    Posix,
    /// Windows `cmd.exe`: reports OSC 7 from its prompt ([`cmd_osc7_prompt`]).
    Cmd,
    /// Windows PowerShell 5.1 or PowerShell 7: reports OSC 7 from its prompt
    /// function ([`POWERSHELL_OSC7_PROMPT`]).
    PowerShell,
}

/// The flavor of the shell started as `program` — by its file name,
/// case-insensitive, with or without `.exe`, never by the client's OS.
pub fn shell_flavor(program: &str) -> ShellFlavor {
    let name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    match name {
        "cmd" => ShellFlavor::Cmd,
        "powershell" | "pwsh" => ShellFlavor::PowerShell,
        _ => ShellFlavor::Posix,
    }
}

/// `PROMPT` for `cmd.exe` that emits OSC 7 with the current directory before
/// the visible prompt (`existing`, else cmd's default `$P$G`).
///
/// `$E` is ESC and `$P` the current drive and path, so every prompt reports
/// `file://filar-raw/C:\dir` without typing anything into the session — no
/// probe to see, nothing written to disk. The payload is not
/// percent-encoded: spaces and non-ASCII arrive as they are, which the OSC 7
/// reader accepts.
pub fn cmd_osc7_prompt(existing: Option<&str>) -> String {
    let visible = existing.map(str::trim).filter(|p| !p.is_empty()).unwrap_or("$P$G");
    format!("$E]7;file://{OSC7_RAW_HOST}/$P$E\\{visible}")
}

/// `-Command` for PowerShell (5.1 and 7) defining a prompt that emits OSC 7
/// with the current filesystem path, then shows `PS <path>> `.
///
/// `[char]27` rather than `` `e ``, which 5.1 does not know. A Unix path's
/// leading `/` is trimmed so the URI does not read `localhost//home`. It replaces a
/// prompt the user's profile defined — the profile runs first.
pub const POWERSHELL_OSC7_PROMPT: &str = "function global:prompt { \
$p = $ExecutionContext.SessionState.Path.CurrentLocation.ProviderPath; \
[Console]::Write([char]27 + ']7;file://filar-raw/' + $p.TrimStart('/') + [char]27 + '\\'); \
'PS ' + $p + '> ' }";

/// Reject empty, oversized, or newline/NUL-containing paths.
pub fn is_safe_cwd(path: &str) -> bool {
    if path.contains('\n') || path.contains('\r') || path.as_bytes().contains(&0) {
        return false;
    }
    let trimmed = path.trim();
    !trimmed.is_empty() && trimmed.len() <= MAX_CWD_LEN
}

/// POSIX single-quote a path for `cd`.
///
/// `\` is quoted (not treated as a bare-safe char) so dash/`printf` cannot
/// reinterpret escapes. Inside single quotes a backslash is literal.
pub fn posix_shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        value.to_string()
    } else {
        let escaped = value.replace('\'', "'\\''");
        format!("'{escaped}'")
    }
}

/// `cd <quoted-path>` plus newline, for writing to an interactive POSIX PTY.
pub fn posix_cd_input(path: &str) -> Option<String> {
    if !is_safe_cwd(path) {
        return None;
    }
    Some(format!("cd {}\n", posix_shell_quote(path.trim())))
}

/// `cd <quoted-path>` without newline, for the agent SSH shell.
pub fn posix_cd_command(path: &str) -> Option<String> {
    if !is_safe_cwd(path) {
        return None;
    }
    Some(format!("cd {}", posix_shell_quote(path.trim())))
}

/// Input that moves an interactive shell of `flavor` to `path`, Enter
/// included — typed into a hidden terminal the agent has moved on from (#493).
///
/// `None` for a path [`is_safe_cwd`] rejects, or one cmd.exe cannot quote
/// (`"`, which Windows paths never contain anyway). Windows shells get `\r`,
/// the byte their console reads as Enter.
pub fn cd_input_for(flavor: ShellFlavor, path: &str) -> Option<String> {
    if !is_safe_cwd(path) {
        return None;
    }
    let path = path.trim();
    match flavor {
        ShellFlavor::Posix => posix_cd_input(path),
        ShellFlavor::Cmd => {
            if path.contains('"') {
                return None;
            }
            Some(format!("cd /d \"{path}\"\r"))
        }
        ShellFlavor::PowerShell => {
            let escaped = path.replace('\'', "''");
            Some(format!("Set-Location -LiteralPath '{escaped}'\r"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pty_cwd_command_is_one_line_any_login_shell_can_pass_on() {
        assert!(!PTY_CWD_COMMAND.contains('\n'));
        assert!(!PTY_CWD_COMMAND.contains('\\'));
        assert_eq!(PTY_CWD_COMMAND.matches('\'').count(), 2, "only the outer quotes");
        assert!(PTY_CWD_COMMAND.starts_with("sh -c '") && PTY_CWD_COMMAND.ends_with('\''));
    }

    #[test]
    fn the_pty_cwd_reply_is_parsed() {
        assert_eq!(
            parse_pty_cwd_reply("filar-cwd:1:/var/log"),
            Some(ForegroundCwd { path: "/var/log".into(), at_prompt: true })
        );
        assert_eq!(
            parse_pty_cwd_reply("banner\nfilar-cwd:0:/srv/a b:c"),
            Some(ForegroundCwd { path: "/srv/a b:c".into(), at_prompt: false })
        );
    }

    #[test]
    fn a_reply_that_is_not_a_directory_is_ignored() {
        for reply in [
            "",
            "Welcome",
            "filar-cwd:",
            "filar-cwd:2:/tmp",
            "filar-cwd:1:relative",
            "filar-cwd:1:/tmp/gone (deleted)",
            "filar-cwd:1:/a\nb",
        ] {
            assert_eq!(parse_pty_cwd_reply(reply), None, "{reply:?}");
        }
    }

    #[test]
    fn the_shell_flavor_comes_from_the_program_name() {
        assert_eq!(shell_flavor("cmd.exe"), ShellFlavor::Cmd);
        assert_eq!(shell_flavor(r"C:\Windows\System32\CMD.EXE"), ShellFlavor::Cmd);
        assert_eq!(shell_flavor("powershell.exe"), ShellFlavor::PowerShell);
        assert_eq!(shell_flavor(r"C:\Program Files\PowerShell\7\pwsh.exe"), ShellFlavor::PowerShell);
        assert_eq!(shell_flavor("/usr/bin/pwsh"), ShellFlavor::PowerShell);
        assert_eq!(shell_flavor("/bin/bash"), ShellFlavor::Posix);
        assert_eq!(shell_flavor("sh"), ShellFlavor::Posix);
    }

    #[test]
    fn the_cmd_prompt_reports_osc7_and_keeps_the_users_prompt() {
        assert_eq!(cmd_osc7_prompt(None), "$E]7;file://filar-raw/$P$E\\$P$G");
        assert_eq!(cmd_osc7_prompt(Some("  ")), "$E]7;file://filar-raw/$P$E\\$P$G");
        assert_eq!(
            cmd_osc7_prompt(Some("[$T] $P$G")),
            "$E]7;file://filar-raw/$P$E\\[$T] $P$G"
        );
    }

    #[test]
    fn the_hooks_mark_their_raw_paths() {
        assert!(std::str::from_utf8(OSC7_PWD_PROBE).unwrap().contains("file://filar-raw%s"));
        assert!(cmd_osc7_prompt(None).contains("file://filar-raw/$P"));
        assert!(POWERSHELL_OSC7_PROMPT.contains("file://filar-raw/"));
    }

    #[test]
    fn the_powershell_prompt_is_one_line_and_5_1_compatible() {
        assert!(!POWERSHELL_OSC7_PROMPT.contains('\n'));
        assert!(POWERSHELL_OSC7_PROMPT.contains("[char]27"));
        assert!(!POWERSHELL_OSC7_PROMPT.contains("`e"), "PowerShell 5.1 has no `e");
        assert!(POWERSHELL_OSC7_PROMPT.contains("ProviderPath"));
    }

    #[test]
    fn cd_input_per_shell_flavor() {
        assert_eq!(
            cd_input_for(ShellFlavor::Posix, "/srv/a b").as_deref(),
            Some("cd '/srv/a b'\n")
        );
        assert_eq!(
            cd_input_for(ShellFlavor::Cmd, r"C:\Users\Мой Dir").as_deref(),
            Some("cd /d \"C:\\Users\\Мой Dir\"\r")
        );
        assert_eq!(
            cd_input_for(ShellFlavor::PowerShell, r"C:\it's").as_deref(),
            Some("Set-Location -LiteralPath 'C:\\it''s'\r")
        );
        assert!(cd_input_for(ShellFlavor::Cmd, "C:\\a\"b").is_none());
        assert!(cd_input_for(ShellFlavor::PowerShell, "C:\\a\nb").is_none());
        assert!(cd_input_for(ShellFlavor::Posix, "").is_none());
    }

    #[test]
    fn rejects_empty_and_control() {
        assert!(!is_safe_cwd(""));
        assert!(!is_safe_cwd("   "));
        assert!(!is_safe_cwd("/tmp\n/evil"));
        assert!(!is_safe_cwd("/tmp\r"));
        assert!(!is_safe_cwd("a\0b"));
    }

    #[test]
    fn quote_simple_path() {
        assert_eq!(posix_shell_quote("/tmp/work"), "/tmp/work");
    }

    #[test]
    fn quote_spaces_and_quotes() {
        assert_eq!(posix_shell_quote("/home/a b"), "'/home/a b'");
        assert_eq!(posix_shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn cd_input_none_when_unsafe() {
        assert!(posix_cd_input("").is_none());
        assert_eq!(posix_cd_input("/opt/app").as_deref(), Some("cd /opt/app\n"));
        assert_eq!(
            posix_cd_command("/opt/app").as_deref(),
            Some("cd /opt/app")
        );
    }
}
