//! Local transport: command execution via subprocess.
//!
//! Uses `tokio::process::Command` to execute commands. On Windows, commands
//! are run via PowerShell (`-NoProfile -NonInteractive -Command`). On Unix,
//! commands are run via `sh -c`.
//!
//! Shell state (cwd, env) does NOT persist between calls — each command runs
//! in a fresh process. The system prompt informs the agent of this.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::info;

use filar_core::{CoreError, Result};

use crate::{CommandResult, StreamEvent};

/// Default timeout for command execution (60 seconds).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// LocalExecutor
// ---------------------------------------------------------------------------

/// [`crate::CommandExecutor`] implementation backed by local subprocess execution.
///
/// On Windows, uses PowerShell. On Unix, uses `sh`.
/// Each command runs in a separate process — no persistent shell session.
/// Commands have a 60-second timeout to prevent hanging on interactive prompts.
pub struct LocalExecutor {
    cancel_notify: Arc<Notify>,
}

impl LocalExecutor {
    /// Create a new local executor.
    pub async fn new() -> Result<Self> {
        Self::with_shell(None).await
    }

    /// Create a local executor with a specific shell program.
    ///
    /// The `shell` parameter is accepted for API compatibility but ignored —
    /// the shell is determined automatically by platform.
    pub async fn with_shell(_shell: Option<&str>) -> Result<Self> {
        info!("local subprocess executor ready");
        Ok(Self {
            cancel_notify: Arc::new(Notify::new()),
        })
    }
}

#[async_trait::async_trait]
impl crate::CommandExecutor for LocalExecutor {
    async fn run(&self, command: &str) -> Result<CommandResult> {
        let start = Instant::now();

        // Build the command based on platform.
        #[cfg(windows)]
        let mut cmd = {
            let full = build_shell_command(command);
            let mut c = tokio::process::Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", &full]);
            c
        };
        #[cfg(unix)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", command]);
            c
        };

        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // Kill the child process if the future is dropped (cancel/timeout).
        cmd.kill_on_drop(true);

        // Wait for output, with timeout and cancel support.
        // When cancel/timeout fires, the output() future is dropped,
        // which kills the child (kill_on_drop = true).
        let output = tokio::select! {
            result = cmd.output() => {
                result.map_err(|e| CoreError::Other(format!("command failed: {e}")))?
            }
            _ = self.cancel_notify.notified() => {
                return Err(CoreError::Other("command cancelled by user".into()));
            }
            _ = tokio::time::sleep(DEFAULT_TIMEOUT) => {
                return Err(CoreError::Other(format!(
                    "command timed out after {} seconds",
                    DEFAULT_TIMEOUT.as_secs()
                )));
            }
        };

        let duration = start.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code();

        Ok(CommandResult {
            stdout,
            stderr,
            exit_code,
            duration,
        })
    }

    async fn run_streaming(&self, command: &str) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>> {
        let result = self.run(command).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            if !result.stdout.is_empty() {
                let _ = tx.send(StreamEvent::Stdout(result.stdout)).await;
            }
            if !result.stderr.is_empty() {
                let _ = tx.send(StreamEvent::Stderr(result.stderr)).await;
            }
            let _ = tx.send(StreamEvent::Exit(result.exit_code)).await;
        });
        Ok(rx)
    }

    async fn cancel(&self) -> Result<()> {
        self.cancel_notify.notify_one();
        Ok(())
    }
}

/// Build the shell command string for the current platform.
///
/// On Windows, prepends `chcp 65001 > $null;` to set the console code page
/// to UTF-8 and appends `2>&1` to redirect stderr through stdout so that
/// PowerShell error messages are also decoded correctly by
/// `String::from_utf8_lossy`.
fn build_shell_command(command: &str) -> String {
    #[cfg(windows)]
    {
        format!("chcp 65001 > $null; {command} 2>&1")
    }
    #[cfg(not(windows))]
    {
        command.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_shell_command_contains_user_command() {
        let result = build_shell_command("echo hello");
        assert!(result.contains("echo hello"), "must contain the original command");
    }

    #[test]
    #[cfg(windows)]
    fn build_shell_command_windows_has_chcp() {
        let result = build_shell_command("dir");
        assert!(
            result.starts_with("chcp 65001 > $null; "),
            "Windows command must start with chcp 65001 prefix, got: {result}"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn build_shell_command_unix_no_chcp() {
        let result = build_shell_command("ls");
        assert_eq!(result, "ls");
        assert!(!result.contains("chcp"));
    }
}
