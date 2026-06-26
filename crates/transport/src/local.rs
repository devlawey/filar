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
            let mut c = tokio::process::Command::new("powershell");
            c.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                command,
            ]);
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
