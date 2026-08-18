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
