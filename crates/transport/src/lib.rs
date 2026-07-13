//! Transport crate: abstraction over command execution (SSH + optional local).
//!
//! This crate provides:
//! - [`CommandExecutor`] — a trait abstracting command execution so the agent
//!   layer is agnostic to whether commands run locally or over SSH.
//! - [`SshExecutor`] — SSH implementation using a persistent shell channel
//!   with marker-based boundary detection (Stage 2).
//! - [`LocalExecutor`] — local PTY implementation using `portable-pty` (Stage 3).
//!   Only available with the `local` feature (enabled by default).

pub mod error;
pub mod interactive;
#[cfg(feature = "local")]
pub mod local;
pub mod secret;
pub mod ssh;

use std::time::Duration;

use tokio::sync::mpsc;
use filar_core::Result;

// Re-export key types.
pub use error::is_connection_lost;
#[cfg(feature = "local")]
pub use interactive::LocalInteractive;
pub use interactive::{InteractiveTerminal, SshInteractive};
#[cfg(feature = "local")]
pub use local::LocalExecutor;
pub use secret::SecretSubstitutingExecutor;
pub use ssh::{SshExecutor, SshSession, SshTransportConfig};

/// Result of executing a command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    /// Merged stdout output (may include stderr if the channel merges them).
    pub stdout: String,
    /// Standard error output (empty if the transport merges streams).
    pub stderr: String,
    /// Process exit code. `None` if the command was killed or did not exit normally.
    pub exit_code: Option<i32>,
    /// Wall-clock duration the command ran for.
    pub duration: Duration,
}

/// A streaming event emitted during command execution.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of stdout data.
    Stdout(String),
    /// A chunk of stderr data.
    Stderr(String),
    /// Command finished with the given exit code (`None` if killed).
    Exit(Option<i32>),
}

/// Trait abstracting command execution so the agent layer is agnostic
/// to whether commands run locally or over SSH.
///
/// Implementations:
/// - [`SshExecutor`] — executes commands on a remote host via a persistent SSH channel.
/// - [`LocalExecutor`] — executes commands in a local PTY.
#[async_trait::async_trait]
pub trait CommandExecutor: Send + Sync {
    /// Run a command and wait for it to complete.
    async fn run(&self, command: &str) -> Result<CommandResult>;

    /// Run a command with streaming output. Returns a receiver that yields
    /// [`StreamEvent`]s as data arrives. The stream ends with an [`StreamEvent::Exit`].
    ///
    /// Default implementation calls [`run`](Self::run) and sends the result as events.
    async fn run_streaming(&self, command: &str) -> Result<mpsc::Receiver<StreamEvent>> {
        let result = self.run(command).await?;
        let (tx, rx) = mpsc::channel(16);
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

    /// Cancel the currently running command by sending Ctrl-C (SIGINT).
    /// If no command is running, this is a no-op.
    async fn cancel(&self) -> Result<()>;
}

// Re-export the async_trait macro so downstream crates don't need a direct dep.
pub use async_trait::async_trait;
