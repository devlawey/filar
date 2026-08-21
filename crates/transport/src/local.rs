//! Local transport: command execution via subprocess.
//!
//! Uses `tokio::process::Command` to execute commands. On Windows, commands
//! are run via PowerShell (`-NoProfile -NonInteractive -Command`). On Unix,
//! commands are run via `sh -c`.
//!
//! Shell state (env) does NOT persist between calls — each command runs
//! in a fresh process. Working directory can persist via [`LocalExecutor::set_cwd`]
//! (interactive ↔ agent sync). The system prompt still warns that `cd` inside
//! a command does not stick.
//!
//! On Unix, agent children are started in a new session (`setsid`) so they
//! lose the controlling TTY. That prevents tools like `sudo` from painting
//! `Password:` over the filar TUI (#329). Interactive Ctrl+T PTY is separate
//! and is not affected.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::info;

use filar_core::{CoreError, Result, DEFAULT_COMMAND_TIMEOUT_SECS};

use crate::{CommandResult, StreamEvent};

/// Default timeout for command execution (5 minutes).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(DEFAULT_COMMAND_TIMEOUT_SECS);

// ---------------------------------------------------------------------------
// LocalExecutor
// ---------------------------------------------------------------------------

/// [`crate::CommandExecutor`] implementation backed by local subprocess execution.
///
/// On Windows, uses PowerShell. On Unix, uses `sh`.
/// Each command runs in a separate process — env does not persist.
/// Working directory persists when set via [`CommandExecutor::set_cwd`].
/// Commands have a 5-minute timeout by default to prevent hanging on
/// interactive prompts; override with [`LocalExecutor::with_timeout`].
pub struct LocalExecutor {
    cancel_notify: Arc<Notify>,
    timeout: Duration,
    cwd: Mutex<Option<PathBuf>>,
}

impl LocalExecutor {
    /// Create a new local executor with the default command timeout.
    pub async fn new() -> Result<Self> {
        Self::with_timeout(DEFAULT_TIMEOUT).await
    }

    /// Create a local executor with an explicit command timeout.
    pub async fn with_timeout(timeout: Duration) -> Result<Self> {
        info!(timeout_secs = timeout.as_secs(), "local subprocess executor ready");
        Ok(Self {
            cancel_notify: Arc::new(Notify::new()),
            timeout,
            cwd: Mutex::new(None),
        })
    }

    /// Create a local executor with a specific shell program.
    ///
    /// The `shell` parameter is accepted for API compatibility but ignored —
    /// the shell is determined automatically by platform.
    pub async fn with_shell(_shell: Option<&str>) -> Result<Self> {
        Self::with_timeout(DEFAULT_TIMEOUT).await
    }
}

/// Detach the child from the parent's controlling terminal (Unix).
///
/// If `setsid` fails, `pre_exec` returns an error and the command is not
/// started — running without TTY isolation would again allow password prompts
/// to overwrite the TUI (#329).
#[cfg(unix)]
fn detach_from_controlling_tty(cmd: &mut tokio::process::Command) {
    // SAFETY: pre_exec runs in the child after fork, before exec. Only
    // async-signal-safe calls are allowed; setsid(2) is async-signal-safe.
    // `tokio::process::Command::pre_exec` wraps std's CommandExt.
    unsafe {
        cmd.pre_exec(|| {
            extern "C" {
                fn setsid() -> i32;
            }
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
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
        #[cfg(unix)]
        detach_from_controlling_tty(&mut cmd);
        if let Ok(guard) = self.cwd.lock() {
            if let Some(ref dir) = *guard {
                cmd.current_dir(dir);
            }
        }

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
            _ = tokio::time::sleep(self.timeout) => {
                return Err(CoreError::Other(format!(
                    "command timed out after {} seconds",
                    self.timeout.as_secs()
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
            cwd: self.cwd.lock().ok().and_then(|g| {
                g.as_ref().map(|p| p.to_string_lossy().into_owned())
            }),
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

    async fn set_cwd(&self, path: &str) -> Result<()> {
        if !crate::is_safe_cwd(path) {
            return Err(CoreError::Other("invalid cwd".into()));
        }
        let mut guard = self.cwd.lock().map_err(|_| {
            CoreError::Other("cwd lock poisoned".into())
        })?;
        *guard = Some(PathBuf::from(path.trim()));
        Ok(())
    }

    async fn current_cwd(&self) -> Option<String> {
        self.cwd
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|p| p.to_string_lossy().into_owned()))
    }
}

/// Build the shell command string for the current platform.
///
/// On Windows, sets `[Console]::OutputEncoding` to UTF-8 so PowerShell writes
/// its own output (cmdlet output, error messages) as UTF-8 bytes, and appends
/// `2>&1` to redirect stderr through stdout. Unlike `chcp 65001`, this does
/// not change the console active code page (`SetConsoleOutputCP`), which .NET
/// caches at startup and ignores later — and which could trigger font switch /
/// resize events on the parent console (#246).
fn build_shell_command(command: &str) -> String {
    #[cfg(windows)]
    {
        format!("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new(); {command} 2>&1")
    }
    #[cfg(not(windows))]
    {
        command.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandExecutor;

    #[test]
    fn default_timeout_is_five_minutes() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(300));
    }

    #[test]
    fn build_shell_command_contains_user_command() {
        let result = build_shell_command("echo hello");
        assert!(result.contains("echo hello"), "must contain the original command");
    }

    #[test]
    #[cfg(windows)]
    fn build_shell_command_windows_has_output_encoding() {
        let result = build_shell_command("dir");
        assert!(
            result.starts_with("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new(); "),
            "Windows command must start with OutputEncoding prefix, got: {result}"
        );
    }

    #[test]
    #[cfg(windows)]
    fn build_shell_command_windows_has_stderr_redirect() {
        let result = build_shell_command("dir");
        assert!(
            result.starts_with("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new(); ")
                && result.ends_with(" 2>&1"),
            "Windows command must have OutputEncoding prefix and 2>&1 suffix, got: {result}"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn build_shell_command_unix_no_prefix() {
        let result = build_shell_command("ls");
        assert_eq!(result, "ls");
        assert!(!result.contains("OutputEncoding"));
    }

    #[tokio::test]
    async fn set_cwd_is_used_by_subsequent_run() {
        let exec = LocalExecutor::new().await.unwrap();
        let marker = format!("filar_cwd_{}", std::process::id());
        let dir = std::env::temp_dir().join(&marker);
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.to_string_lossy().into_owned();
        exec.set_cwd(&dir_str).await.unwrap();
        assert_eq!(exec.current_cwd().await.as_deref(), Some(dir_str.as_str()));
        #[cfg(windows)]
        let cmd = "(Get-Location).Path";
        #[cfg(unix)]
        let cmd = "pwd";
        let result = exec.run(cmd).await.unwrap();
        assert!(
            result.stdout.contains(&marker),
            "cwd output {:?} should contain {marker}",
            result.stdout
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn set_cwd_rejects_newline() {
        let exec = LocalExecutor::new().await.unwrap();
        assert!(exec.set_cwd("/tmp\n/etc").await.is_err());
    }

    /// Agent local commands must not keep a controlling TTY (#329).
    ///
    /// After `setsid`, `ps -o tty=` for this process reports `??` / blank /
    /// `?` rather than a real tty name like `ttys001`.
    #[tokio::test]
    #[cfg(unix)]
    async fn unix_agent_child_has_no_controlling_tty() {
        let exec = LocalExecutor::with_timeout(Duration::from_secs(10))
            .await
            .unwrap();
        let result = exec.run("ps -o tty= -p $$").await.unwrap();
        let tty = result.stdout.trim();
        assert!(
            tty.is_empty() || tty == "?" || tty == "??" || tty == "-",
            "expected no controlling tty for agent child, got {tty:?} (stdout={:?} stderr={:?})",
            result.stdout,
            result.stderr
        );
    }
}
