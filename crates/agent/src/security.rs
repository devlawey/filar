//! Security layer: command confirmation, destructive pattern detection,
//! and read-only command allowlist.
//!
//! The agent never executes a command without confirmation in the default
//! `Always` mode. Destructive commands are flagged with a warning, and the
//! confirmer can use this information to show extra warnings to the user.

use filar_core::{CommandConfirmMode, Result};

use crate::tools::ToolKind;

// ---------------------------------------------------------------------------
// CommandConfirmer trait
// ---------------------------------------------------------------------------

/// Trait for asking the user to confirm a command before execution.
///
/// Implementations:
/// - [`CliConfirmer`] — simple y/n prompt on stdin (Stage 5).
/// - (Future) TUI confirmer — ratatui dialog (Stage 6).
#[async_trait::async_trait]
pub trait CommandConfirmer: Send + Sync {
    /// Ask the user to confirm execution of `command`.
    ///
    /// Returns `Ok(true)` if the user approves, `Ok(false)` if they deny.
    async fn confirm(&self, command: &str, explanation: &str, destructive: bool) -> Result<bool>;
}

// ---------------------------------------------------------------------------
// Destructive pattern detection
// ---------------------------------------------------------------------------

/// Patterns that are considered destructive and warrant an extra warning.
///
/// This is not an exhaustive list — it covers the most dangerous common
/// patterns. The check is intentionally conservative (false positives are
/// acceptable; false negatives are not).
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "rm -r -f",
    "rm -f -r",
    "rmdir",
    "mkfs",
    "dd if=",
    "dd of=",
    "shutdown",
    "reboot",
    "halt",
    "poweroff",
    "init 0",
    "init 6",
    "> /dev/sd",
    "> /dev/nvme",
    "> /dev/hd",
    "> /dev/vd",
    ":(){:|:&};:",
    "chmod -R 777",
    "chmod 777",
    "chown -R",
    "iptables -F",
    "ip6tables -F",
    "systemctl stop",
    "systemctl disable",
    "kill -9",
    "killall",
    "pkill",
    "userdel",
    "usermod -L",
    "passwd -l",
    "umount",
    "mount -o remount",
];

/// Check whether a command matches any destructive pattern.
pub fn is_destructive(command: &str) -> bool {
    let lower = command.to_lowercase();
    DESTRUCTIVE_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Check whether a command writes to a system path (potential destruction).
///
/// Only checks the **immediate target** of each redirect operator (`>` or `>>`),
/// not system paths that appear elsewhere in the command. This prevents false
/// positives like `grep x > /tmp/a; cat /etc/passwd` where `/etc/passwd` is a
/// read target, not a write target.
fn writes_to_system_path(command: &str) -> bool {
    let lower = command.to_lowercase();
    // Remove non-redirect patterns that contain > (=>, ->, >=).
    let cleaned = lower.replace("=>", "").replace("->", "").replace(">=", "");

    let system_paths = ["/etc/", "/boot/", "/sys/", "/proc/", "/dev/", "/usr/", "/lib/"];

    // Split by shell operators to process each sub-command independently.
    for sub in cleaned.split([';', '|', '&']) {
        let sub = sub.trim();
        // Use char_indices() for byte-safe offsets — sub[...] slicing requires
        // byte positions, not character positions. Non-ASCII text before '>'
        // would otherwise cause a panic or incorrect extraction.
        let indices: Vec<(usize, char)> = sub.char_indices().collect();
        let mut i = 0;
        while i < indices.len() {
            if indices[i].1 == '>' {
                // Skip second '>' for '>>' append operator.
                let mut target_start = i + 1;
                if target_start < indices.len() && indices[target_start].1 == '>' {
                    target_start += 1;
                }
                // Skip whitespace after redirect operator.
                while target_start < indices.len() && indices[target_start].1.is_whitespace() {
                    target_start += 1;
                }
                // Extract the target token (up to next whitespace or end).
                let mut target_end = target_start;
                while target_end < indices.len() && !indices[target_end].1.is_whitespace() {
                    target_end += 1;
                }
                if target_end > target_start {
                    // Use byte offsets for slicing.
                    let byte_start = indices[target_start].0;
                    let byte_end = if target_end < indices.len() {
                        indices[target_end].0
                    } else {
                        sub.len()
                    };
                    let target = &sub[byte_start..byte_end];
                    // Strip surrounding shell quotes so that
                    // `echo foo >"/etc/passwd"` is still caught.
                    let target = target.trim_matches(|c| c == '"' || c == '\'');
                    // /dev/null is a null device, not a real system path write.
                    if target != "/dev/null" {
                        if system_paths.iter().any(|p| target.starts_with(p)) {
                            return true;
                        }
                    }
                }
                i = target_end;
            } else {
                i += 1;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Write/modify detection (denylist approach)
// ---------------------------------------------------------------------------

/// Patterns that indicate a command writes, modifies, or deletes something.
/// If ANY of these are present, the command requires confirmation.
const WRITE_PATTERNS: &[&str] = &[
    // Delete / remove
    "rm ", "rmdir ", "unlink ",
    // Copy / move / rename
    "cp ", "mv ", "install ",
    // Create
    "mkdir ", "touch ", "ln ",
    // Permissions
    "chmod ", "chown ", "chgrp ", "setfacl ",
    // Package management
    "apt ", "apt-get ", "yum ", "dnf ", "pip ", "pip3 ", "npm ", "yarn ",
    "cargo ", "go install", "gem ",
    // Service management — only write actions, not status/show/list
    "systemctl start", "systemctl stop", "systemctl restart",
    "systemctl reload", "systemctl enable", "systemctl disable",
    "systemctl mask", "systemctl unmask", "systemctl daemon-reload",
    // Disk / format
    "dd ", "mkfs", "fdisk ", "parted ",
    // Download to file
    "curl -o", "curl -O", "wget ",
    // In-place edit
    "sed -i", "perl -i",
    // Write to file
    "tee ",
    // Mount operations — only active mount, not listing
    "mount -t", "mount -o", "mount /",
    "umount ",
    // User management
    "useradd ", "userdel ", "usermod ", "passwd ", "groupadd ", "groupdel ",
    // Firewall
    "iptables ", "ip6tables ", "ufw ", "firewall-cmd",
    // Power
    "shutdown", "reboot", "halt", "poweroff", "init 0", "init 6",
    // Process kill
    "kill ", "killall", "pkill",
    // Network config — only changes, not reads
    "ip link set", "ip addr add", "ip addr del", "ip route add", "ip route del",
    "ip link set",
];

/// Check whether a command modifies the system (writes, deletes, installs, etc.).
pub fn is_write_command(command: &str) -> bool {
    let lower = command.to_lowercase();

    // Check for redirect operators (> or >>), but ignore harmless redirects
    // to /dev/null and stderr/stdout duplication (2>&1, 1>&2).
    let redirect_cleaned = lower
        .replace("2>/dev/null", "")
        .replace("1>/dev/null", "")
        .replace(">/dev/null", "")
        .replace("&>/dev/null", "")
        .replace("2>&1", "")
        .replace("1>&2", "");
    // Remove non-redirect patterns that contain > (=>, ->, >=).
    let cleaned = redirect_cleaned.replace("=>", "").replace("->", "").replace(">=", "");
    if cleaned.contains('>') {
        return true;
    }

    // Split command by shell operators to check each sub-command.
    // This prevents false positives like "at " matching "cat ".
    let sub_commands = lower
        .split([';', '|', '&'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());

    for sub in sub_commands {
        // Remove leading "sudo " if present.
        let sub = sub.strip_prefix("sudo ").unwrap_or(sub);

        for pattern in WRITE_PATTERNS {
            // Match if the sub-command starts with the pattern.
            if sub.starts_with(pattern) || sub == pattern.trim() {
                return true;
            }
        }
    }

    false
}

/// Check whether a command starts with a known read-only command.
pub fn is_readonly(command: &str) -> bool {
    !is_write_command(command)
}

// ---------------------------------------------------------------------------
// Confirmation decision logic
// ---------------------------------------------------------------------------

/// The result of a confirmation check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmDecision {
    /// The command can be executed without asking the user.
    AutoApproved,
    /// The user must be asked for confirmation.
    NeedsConfirmation,
    /// The command is blocked (too dangerous).
    Blocked(String),
}

/// Decide what to do with a command based on the confirm mode and security checks.
///
/// - `Never` → always auto-approve (dangerous).
/// - `Allowlist` → auto-approve read-only commands, confirm everything else.
/// - `Always` → confirm everything.
///
/// Destructive commands always require confirmation (even in `Never` mode they
/// get a warning, but are still auto-approved — the user chose `Never`).
pub fn check_command(command: &str, mode: CommandConfirmMode) -> ConfirmDecision {
    let destructive = is_destructive(command) || writes_to_system_path(command);

    match mode {
        CommandConfirmMode::Never => {
            if destructive {
                // Still auto-approve, but the confirmer can log a warning.
                ConfirmDecision::AutoApproved
            } else {
                ConfirmDecision::AutoApproved
            }
        }
        CommandConfirmMode::Allowlist => {
            if is_readonly(command) && !destructive {
                ConfirmDecision::AutoApproved
            } else if destructive {
                ConfirmDecision::NeedsConfirmation
            } else {
                ConfirmDecision::NeedsConfirmation
            }
        }
        CommandConfirmMode::Always => ConfirmDecision::NeedsConfirmation,
    }
}

/// Determine if a tool kind requires confirmation given the confirm mode.
pub fn tool_needs_confirmation(kind: ToolKind, command: &str, mode: CommandConfirmMode) -> ConfirmDecision {
    match kind {
        ToolKind::RunCommand => check_command(command, mode),
        // read_file and list_dir are wrappers around cat/ls — check the
        // generated command, but they're typically read-only.
        ToolKind::ReadFile | ToolKind::ListDir => {
            if is_readonly(command) {
                match mode {
                    CommandConfirmMode::Never => ConfirmDecision::AutoApproved,
                    CommandConfirmMode::Allowlist => ConfirmDecision::AutoApproved,
                    CommandConfirmMode::Always => ConfirmDecision::NeedsConfirmation,
                }
            } else {
                check_command(command, mode)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CliConfirmer — simple stdin-based confirmer
// ---------------------------------------------------------------------------

/// A simple command-line confirmer that reads y/n from stdin.
pub struct CliConfirmer;

impl CliConfirmer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CliConfirmer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl CommandConfirmer for CliConfirmer {
    async fn confirm(&self, command: &str, explanation: &str, destructive: bool) -> Result<bool> {
        // Use blocking stdin in a spawn_blocking to avoid blocking the async runtime.
        let command = command.to_string();
        let explanation = explanation.to_string();
        tokio::task::spawn_blocking(move || {
            use std::io::{self, BufRead, Write};

            let mut stdout = io::stdout();
            let stdin = io::stdin();

            writeln!(stdout, "\n┌─ Command proposed by agent:").ok();
            if !explanation.is_empty() {
                writeln!(stdout, "│ Explanation: {explanation}").ok();
            }
            if destructive {
                writeln!(stdout, "│ ⚠ WARNING: This command may be destructive!").ok();
            }
            writeln!(stdout, "│ Command: {command}").ok();
            write!(stdout, "└─ Approve? [y/N] ").ok();
            stdout.flush().ok();

            let mut input = String::new();
            stdin.lock().read_line(&mut input).ok();

            let approved = input.trim().eq_ignore_ascii_case("y")
                || input.trim().eq_ignore_ascii_case("yes");

            if approved {
                writeln!(stdout, "  → Approved").ok();
            } else {
                writeln!(stdout, "  → Denied").ok();
            }

            Ok(approved)
        })
        .await
        .map_err(|e| filar_core::CoreError::Other(format!("confirmer task failed: {e}")))?
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_destructive_rm_rf() {
        assert!(is_destructive("rm -rf /"));
        assert!(is_destructive("sudo rm -rf /tmp/*"));
        assert!(is_destructive("RM -RF /home"));
    }

    #[test]
    fn detect_destructive_mkfs() {
        assert!(is_destructive("mkfs.ext4 /dev/sda1"));
    }

    #[test]
    fn detect_destructive_dd() {
        assert!(is_destructive("dd if=/dev/zero of=/dev/sda"));
    }

    #[test]
    fn not_destructive_ls() {
        assert!(!is_destructive("ls -la /tmp"));
        assert!(!is_destructive("cat /etc/hostname"));
        assert!(!is_destructive("echo hello"));
    }

    #[test]
    fn detect_system_redirect() {
        assert!(writes_to_system_path("echo foo > /etc/passwd"));
        assert!(writes_to_system_path("echo bar >> /boot/grub.cfg"));
        assert!(!writes_to_system_path("echo foo > /tmp/test"));
        // /dev/null is a null device, not a real system path write.
        assert!(!writes_to_system_path("echo foo > /dev/null"));
        assert!(!writes_to_system_path("echo foo 2>/dev/null"));
        // System path in a *read* sub-command must not trigger.
        assert!(!writes_to_system_path("grep x > /tmp/a; cat /etc/passwd"));
        // /dev/sda is a real device — should be flagged.
        assert!(writes_to_system_path("dd if=/dev/zero > /dev/sda"));
        // Append to system path.
        assert!(writes_to_system_path("echo bad >> /etc/passwd"));
        // Quoted redirect targets must still be caught.
        assert!(writes_to_system_path("echo foo >\"/etc/passwd\""));
        assert!(writes_to_system_path("echo foo >>'/etc/passwd'"));
        // Non-ASCII before '>' must not panic (byte-safe indexing).
        assert!(writes_to_system_path("echo привет > /etc/passwd"));
        assert!(!writes_to_system_path("echo привет > /tmp/test"));
    }

    #[test]
    fn readonly_commands() {
        assert!(is_readonly("ls -la /tmp"));
        assert!(is_readonly("cat /etc/hostname"));
        assert!(is_readonly("ps aux"));
        assert!(is_readonly("grep foo bar.txt"));
        assert!(is_readonly("pwd"));
        assert!(is_readonly("echo hello"));
        assert!(is_readonly("hostname"));
        assert!(is_readonly("ip -4 addr show | grep inet | awk '{print $2}'"));
        // Redirects to /dev/null are read-only
        assert!(is_readonly("cat /etc/os-release 2>/dev/null || cat /etc/redhat-release 2>/dev/null || echo unknown OS"));
        assert!(is_readonly("ls -la /tmp 2>/dev/null"));
        assert!(is_readonly("find / -name foo 2>/dev/null"));
    }

    #[test]
    fn not_readonly_commands() {
        assert!(!is_readonly("rm /tmp/file"));
        assert!(!is_readonly("chmod 755 /tmp"));
        assert!(!is_readonly("curl -o file http://example.com"));
        assert!(!is_readonly("echo foo > /tmp/test"));
        assert!(!is_readonly("mkdir /tmp/newdir"));
        assert!(!is_readonly("systemctl restart nginx"));
        assert!(!is_readonly("apt install htop"));
    }

    #[test]
    fn check_always_mode() {
        assert_eq!(
            check_command("ls", CommandConfirmMode::Always),
            ConfirmDecision::NeedsConfirmation
        );
        assert_eq!(
            check_command("rm -rf /", CommandConfirmMode::Always),
            ConfirmDecision::NeedsConfirmation
        );
    }

    #[test]
    fn check_never_mode() {
        assert_eq!(
            check_command("ls", CommandConfirmMode::Never),
            ConfirmDecision::AutoApproved
        );
        assert_eq!(
            check_command("rm -rf /", CommandConfirmMode::Never),
            ConfirmDecision::AutoApproved
        );
    }

    #[test]
    fn check_allowlist_mode() {
        // Read-only → auto-approved
        assert_eq!(
            check_command("ls -la", CommandConfirmMode::Allowlist),
            ConfirmDecision::AutoApproved
        );
        assert_eq!(
            check_command("cat /etc/hostname", CommandConfirmMode::Allowlist),
            ConfirmDecision::AutoApproved
        );
        // Write command → needs confirmation
        assert_eq!(
            check_command("curl -o file http://example.com", CommandConfirmMode::Allowlist),
            ConfirmDecision::NeedsConfirmation
        );
        // Destructive → needs confirmation
        assert_eq!(
            check_command("rm -rf /tmp", CommandConfirmMode::Allowlist),
            ConfirmDecision::NeedsConfirmation
        );
    }

    #[test]
    fn tool_needs_confirmation_read_file() {
        // read_file generates "cat <path>" — read-only
        let decision = tool_needs_confirmation(
            ToolKind::ReadFile,
            "cat /etc/hostname",
            CommandConfirmMode::Allowlist,
        );
        assert_eq!(decision, ConfirmDecision::AutoApproved);

        // In Always mode, still needs confirmation
        let decision = tool_needs_confirmation(
            ToolKind::ReadFile,
            "cat /etc/hostname",
            CommandConfirmMode::Always,
        );
        assert_eq!(decision, ConfirmDecision::NeedsConfirmation);
    }
}
