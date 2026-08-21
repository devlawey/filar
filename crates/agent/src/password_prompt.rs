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
}
