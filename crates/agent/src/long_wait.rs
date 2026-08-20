//! Policy for long wall-clock waits in agent tool commands.
//!
//! Tool calls share a hard `[timeouts].command_secs` deadline. Patterns like
//! `sleep 120` or `Start-Sleep -Seconds 300` almost always hit that deadline
//! and teach the model nothing useful. This module detects those waits early
//! and returns guidance to use background + short poll instead.

/// Minimum sleep/wait duration (seconds) that is rejected as an agent tool call.
///
/// Short delays (a few seconds) can still be useful; anything at or above this
/// threshold should be background work + polling under the command timeout.
pub const LONG_WAIT_SECS_THRESHOLD: u64 = 30;

/// Guidance returned to the LLM (and shown in the TUI) when a long wait is
/// rejected or a command times out.
pub const LONG_WAIT_GUIDANCE: &str = "\
Do not use sleep/Start-Sleep (or similar wall-clock waits) under the command \
timeout. Start long jobs in the background and poll with short commands \
(POSIX: `nohup … >log 2>&1 & echo $!` then `tail`/`ps`; Windows: \
`Start-Process` / jobs, then check output). For live interactive progress, \
ask the user to use Ctrl+T (interactive terminal).";

/// If `command` is primarily a long wall-clock wait, return a tool-error
/// message the agent should follow. Otherwise `None` (command may run).
pub fn reject_long_wait(command: &str) -> Option<String> {
    let secs = longest_wait_secs(command)?;
    if secs < LONG_WAIT_SECS_THRESHOLD {
        return None;
    }
    Some(format!(
        "Error: refused to run a ~{secs}s wall-clock wait (threshold {LONG_WAIT_SECS_THRESHOLD}s). {LONG_WAIT_GUIDANCE}"
    ))
}

/// Append long-wait guidance when a transport/agent timeout error is returned.
pub fn enrich_timeout_message(base: &str) -> String {
    if base.contains(LONG_WAIT_GUIDANCE) {
        return base.to_string();
    }
    format!("{base} {LONG_WAIT_GUIDANCE}")
}

/// Largest sleep/Start-Sleep duration found in `command`, in seconds.
fn longest_wait_secs(command: &str) -> Option<u64> {
    let mut best: Option<u64> = None;
    for token_run in command.split(['\n', ';', '|']) {
        // Also split on `&&` / `||` without treating single `&` (background) specially.
        for part in token_run.split("&&").flat_map(|p| p.split("||")) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some(secs) = parse_posix_sleep(part).or_else(|| parse_powershell_sleep(part)) {
                best = Some(best.map_or(secs, |b| b.max(secs)));
            }
        }
    }
    best
}

fn parse_posix_sleep(part: &str) -> Option<u64> {
    // `sleep 120`, `sleep 2m`, `sleep 1h`, optional leading env/`command`
    let rest = strip_leading_command_words(part);
    let mut words = rest.split_whitespace();
    let cmd = words.next()?;
    if !cmd.eq_ignore_ascii_case("sleep") {
        return None;
    }
    let arg = words.next()?;
    parse_duration_arg(arg)
}

fn parse_powershell_sleep(part: &str) -> Option<u64> {
    // `Start-Sleep -Seconds 120`, `Start-Sleep -s 120`, `Start-Sleep 120`
    let rest = strip_leading_command_words(part);
    let lower = rest.to_ascii_lowercase();
    if !lower.starts_with("start-sleep") {
        return None;
    }
    let after = rest["start-sleep".len()..].trim_start();
    let mut words = after.split_whitespace();
    let first = words.next()?;
    if first.eq_ignore_ascii_case("-seconds")
        || first.eq_ignore_ascii_case("-s")
        || first.eq_ignore_ascii_case("-second")
    {
        return words.next().and_then(parse_duration_arg);
    }
    if first.eq_ignore_ascii_case("-milliseconds") || first.eq_ignore_ascii_case("-m") {
        let ms = words.next()?.parse::<u64>().ok()?;
        return Some(ms.div_ceil(1000).max(1));
    }
    parse_duration_arg(first)
}

/// Skip common prefixes like `sudo`, `command`, `time`.
fn strip_leading_command_words(part: &str) -> &str {
    let mut s = part.trim();
    loop {
        let (w, rest) = match s.split_once(char::is_whitespace) {
            Some((w, rest)) => (w, rest.trim_start()),
            None => return s,
        };
        let skip = matches!(
            w.to_ascii_lowercase().as_str(),
            "sudo" | "command" | "time" | "env"
        );
        if !skip {
            return s;
        }
        s = rest;
    }
}

fn parse_duration_arg(arg: &str) -> Option<u64> {
    let arg = arg.trim();
    if arg.is_empty() {
        return None;
    }
    // Pure integer seconds.
    if let Ok(n) = arg.parse::<u64>() {
        return Some(n);
    }
    // Suffix: 120s, 2m, 1h (GNU sleep style).
    let (num, mult) = match arg.as_bytes().last()? {
        b's' | b'S' => (&arg[..arg.len() - 1], 1u64),
        b'm' | b'M' => (&arg[..arg.len() - 1], 60),
        b'h' | b'H' => (&arg[..arg.len() - 1], 3600),
        b'd' | b'D' => (&arg[..arg.len() - 1], 86400),
        _ => return None,
    };
    let n: u64 = num.parse().ok()?;
    n.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_sleep_above_threshold() {
        let msg = reject_long_wait("sleep 120 && tail log").unwrap();
        assert!(msg.contains("120"));
        assert!(msg.contains("nohup") || msg.contains("background"));
    }

    #[test]
    fn allows_short_sleep() {
        assert!(reject_long_wait("sleep 5 && echo ok").is_none());
        assert!(reject_long_wait("sleep 29").is_none());
    }

    #[test]
    fn rejects_at_threshold() {
        assert!(reject_long_wait("sleep 30").is_some());
    }

    #[test]
    fn parses_gnu_suffixes() {
        assert_eq!(longest_wait_secs("sleep 2m"), Some(120));
        assert_eq!(longest_wait_secs("sleep 1h"), Some(3600));
    }

    #[test]
    fn parses_powershell_start_sleep() {
        assert_eq!(
            longest_wait_secs("Start-Sleep -Seconds 180"),
            Some(180)
        );
        assert!(reject_long_wait("Start-Sleep -s 60").is_some());
    }

    #[test]
    fn ignores_non_wait_commands() {
        assert!(reject_long_wait("ollama pull model").is_none());
        assert!(reject_long_wait("nohup ollama pull x >log 2>&1 &").is_none());
    }

    #[test]
    fn enrich_timeout_appends_guidance_once() {
        let once = enrich_timeout_message("Command timed out.");
        assert!(once.contains("Command timed out."));
        assert!(once.contains("Ctrl+T"));
        let twice = enrich_timeout_message(&once);
        assert_eq!(
            twice.matches("Ctrl+T").count(),
            1,
            "guidance must not duplicate"
        );
    }
}
