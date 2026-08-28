//! Detect interactive password prompts in tool output and steer the agent
//! toward Ctrl+P / `$FILAR_SECRET_N` (see #329, #331).

/// Short guidance appended when a command fails because it needs a TTY password.
/// Kept to ~2 lines so it stays readable in the TUI command block (#331).
pub const PASSWORD_PROMPT_GUIDANCE: &str = "\
Password required (no interactive TTY). Ask the user for Ctrl+P → \
$FILAR_SECRET_N, then retry with e.g. `printf '%s\\n' \"$FILAR_SECRET_1\" | sudo -S …`. \
Do not embed the real password in the command.";

/// Heuristic: output looks like a password / sudo TTY failure.
pub fn looks_like_password_prompt(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "password:",
        "password for",
        "a terminal is required to read the password",
        "a password is required",
        "no password was provided",
        "sudo: a password is required",
        "sudo: no tty present",
        "sudo: no tty",
        "use the -s option to read from standard input",
        "configure an askpass helper",
        "authentication is required",
        "sorry, try again",
    ];
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// Guidance for the stdin conflict between `sudo -S` and a `<<EOF` heredoc
/// on the same command (#364). In POSIX `sh`, a heredoc attached to the last
/// pipeline segment **replaces** its stdin, so `sudo -S` reads the heredoc
/// body as password attempts instead of the piped secret.
pub const SUDO_HEREDOC_GUIDANCE: &str = "\
stdin conflict: a `<<EOF` heredoc on the same command as `sudo -S` replaces \
the secret pipe — sudo tried the heredoc lines as the password. Fix: write \
the content to a temp file first (no sudo), then \
`printf '%s\\n' \"$FILAR_SECRET_1\" | sudo -S cp /tmp/file <target>`.";

/// Heuristic: the command combines `sudo -S` with a heredoc, so the heredoc
/// steals `sudo -S`'s stdin (see [`SUDO_HEREDOC_GUIDANCE`], #364).
pub fn sudo_heredoc_stdin_conflict(command: &str) -> bool {
    let has_sudo = command
        .split_whitespace()
        .any(|t| t == "sudo" || t.ends_with("/sudo"));
    let has_dash_s = command
        .split_whitespace()
        .any(|t| t.starts_with('-') && !t.starts_with("--") && t.contains('S'));
    let has_heredoc = command.contains("<<");
    has_sudo && has_dash_s && has_heredoc
}

/// Append password guidance once when the output indicates a password/TTY failure.
pub fn enrich_password_prompt_message(base: &str) -> String {
    if base.contains(PASSWORD_PROMPT_GUIDANCE) {
        return base.to_string();
    }
    if !looks_like_password_prompt(base) {
        return base.to_string();
    }
    format!("{base}\n\n{PASSWORD_PROMPT_GUIDANCE}")
}

/// Like [`enrich_password_prompt_message`], but also appends
/// [`SUDO_HEREDOC_GUIDANCE`] when the failed command combined `sudo -S` with
/// a heredoc (#364). Only applied on password/TTY failures.
pub fn enrich_password_prompt_message_for_command(command: &str, base: &str) -> String {
    let enriched = enrich_password_prompt_message(base);
    if enriched == base {
        return enriched;
    }
    if sudo_heredoc_stdin_conflict(command) && !enriched.contains(SUDO_HEREDOC_GUIDANCE) {
        return format!("{enriched}\n\n{SUDO_HEREDOC_GUIDANCE}");
    }
    enriched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sudo_tty_messages() {
        assert!(looks_like_password_prompt(
            "sudo: a terminal is required to read the password; either use the -S option"
        ));
        assert!(looks_like_password_prompt("Password:"));
        assert!(looks_like_password_prompt("Sorry, try again."));
        assert!(!looks_like_password_prompt("ok done"));
    }

    #[test]
    fn enrich_appends_once() {
        let once = enrich_password_prompt_message("sudo: a password is required");
        assert!(once.contains(PASSWORD_PROMPT_GUIDANCE));
        assert!(once.contains("Ctrl+P"));
        assert!(once.contains("$FILAR_SECRET_N"));
        assert!(once.contains("sudo -S"));
        // Keep UI-friendly: guidance itself should stay short.
        assert!(
            PASSWORD_PROMPT_GUIDANCE.len() < 220,
            "guidance too long for TUI: {} chars",
            PASSWORD_PROMPT_GUIDANCE.len()
        );
        let twice = enrich_password_prompt_message(&once);
        assert_eq!(
            twice.matches(PASSWORD_PROMPT_GUIDANCE).count(),
            1,
            "guidance must not duplicate"
        );
    }

    #[test]
    fn enrich_skips_unrelated() {
        let out = enrich_password_prompt_message("ls: cannot access");
        assert_eq!(out, "ls: cannot access");
    }

    #[test]
    fn detects_sudo_heredoc_stdin_conflict() {
        let cmd = "printf '%s\\n' \"$FILAR_SECRET_1\" | sudo -S tee /tmp/f >/dev/null <<'EOF'\ncontent\nEOF";
        assert!(sudo_heredoc_stdin_conflict(cmd));
        assert!(sudo_heredoc_stdin_conflict("echo x | sudo -S sh <<'EOF'\ntrue\nEOF"));
        // No heredoc → no conflict.
        assert!(!sudo_heredoc_stdin_conflict(
            "printf '%s\\n' \"$FILAR_SECRET_1\" | sudo -S true"
        ));
        // Heredoc without sudo -S → no conflict.
        assert!(!sudo_heredoc_stdin_conflict("cat > /tmp/f <<'EOF'\nx\nEOF"));
        // Heredoc with bare sudo (no -S) → no conflict.
        assert!(!sudo_heredoc_stdin_conflict("sudo sh <<'EOF'\nx\nEOF"));
    }

    #[test]
    fn enrich_for_command_appends_heredoc_guidance_on_failure() {
        let cmd = "printf '%s\\n' \"$FILAR_SECRET_2\" | sudo -S tee /tmp/f <<'EOF'\n<x/>\nEOF";
        let out = enrich_password_prompt_message_for_command(cmd, "Password:Sorry, try again.\nsudo: 3 incorrect password attempts");
        assert!(out.contains(PASSWORD_PROMPT_GUIDANCE));
        assert!(out.contains(SUDO_HEREDOC_GUIDANCE));
        // Keep UI-friendly: combined guidance stays bounded.
        assert!(
            SUDO_HEREDOC_GUIDANCE.len() < 320,
            "guidance too long: {} chars",
            SUDO_HEREDOC_GUIDANCE.len()
        );
    }

    #[test]
    fn enrich_for_command_heredoc_only_on_password_failure() {
        // Successful/unrelated output → no guidance even with conflicting command.
        let cmd = "printf x | sudo -S tee /tmp/f <<'EOF'\ncontent\nEOF";
        let out = enrich_password_prompt_message_for_command(cmd, "ok done");
        assert_eq!(out, "ok done");
    }

    #[test]
    fn enrich_for_command_without_heredoc_keeps_base_guidance_only() {
        let out = enrich_password_prompt_message_for_command(
            "printf '%s\\n' \"$FILAR_SECRET_1\" | sudo -S true",
            "sudo: a password is required",
        );
        assert!(out.contains(PASSWORD_PROMPT_GUIDANCE));
        assert!(!out.contains(SUDO_HEREDOC_GUIDANCE));
    }
}
