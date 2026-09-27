//! Working-directory helpers shared by local and SSH transports.
//!
//! Used to sync the agent executor with the interactive PTY (and the reverse)
//! without writing files on the remote host.

/// Maximum length of a cwd we will accept from OSC 7, `$PWD`, or `set_cwd`.
pub const MAX_CWD_LEN: usize = 1024;

/// Bytes written to a POSIX interactive shell to emit OSC 7 for the current pwd.
///
/// No files are created. The PTY is typically closed immediately after, so the
/// command does not stay in the user's session.
pub const OSC7_PWD_PROBE: &[u8] =
    b"printf '\\033]7;file://localhost%s\\007' \"$(pwd)\"\n";

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
/// `file://localhost/C:\dir` without typing anything into the session — no
/// probe to see, nothing written to disk. The payload is not
/// percent-encoded: spaces and non-ASCII arrive as they are, which the OSC 7
/// reader accepts.
pub fn cmd_osc7_prompt(existing: Option<&str>) -> String {
    let visible = existing.map(str::trim).filter(|p| !p.is_empty()).unwrap_or("$P$G");
    format!("$E]7;file://localhost/$P$E\\{visible}")
}

/// `-Command` for PowerShell (5.1 and 7) defining a prompt that emits OSC 7
/// with the current filesystem path, then shows `PS <path>> `.
///
/// `[char]27` rather than `` `e ``, which 5.1 does not know. A Unix path's
/// leading `/` is trimmed so the URI does not read `localhost//home`. It replaces a
/// prompt the user's profile defined — the profile runs first.
pub const POWERSHELL_OSC7_PROMPT: &str = "function global:prompt { \
$p = $ExecutionContext.SessionState.Path.CurrentLocation.ProviderPath; \
[Console]::Write([char]27 + ']7;file://localhost/' + $p.TrimStart('/') + [char]27 + '\\'); \
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(cmd_osc7_prompt(None), "$E]7;file://localhost/$P$E\\$P$G");
        assert_eq!(cmd_osc7_prompt(Some("  ")), "$E]7;file://localhost/$P$E\\$P$G");
        assert_eq!(
            cmd_osc7_prompt(Some("[$T] $P$G")),
            "$E]7;file://localhost/$P$E\\[$T] $P$G"
        );
    }

    #[test]
    fn the_powershell_prompt_is_one_line_and_5_1_compatible() {
        assert!(!POWERSHELL_OSC7_PROMPT.contains('\n'));
        assert!(POWERSHELL_OSC7_PROMPT.contains("[char]27"));
        assert!(!POWERSHELL_OSC7_PROMPT.contains("`e"), "PowerShell 5.1 has no `e");
        assert!(POWERSHELL_OSC7_PROMPT.contains("ProviderPath"));
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
