//! Transport crate: abstraction over command execution (SSH + optional local).
//!
//! This crate provides:
//! - [`CommandExecutor`] — a trait abstracting command execution so the agent
//!   layer is agnostic to whether commands run locally or over SSH.
//! - [`SshExecutor`] — SSH implementation using a persistent shell channel
//!   with marker-based boundary detection (Stage 2).
//! - [`LocalExecutor`] — local PTY implementation using `portable-pty` (Stage 3).
//!   Only available with the `local` feature (enabled by default).
//! - [`FileTransfer`] — filesystem of one side (local disk or SSH host) for the
//!   file panels and the copy engine; [`LocalFileTransfer`] is the local one,
//!   [`SftpFileTransfer`] the SSH one.
//! - [`copy()`] — copy engine between two [`FileTransfer`] sides.

mod auth;
#[cfg(feature = "kube")]
pub mod cluster;
pub mod copy;
pub mod cwd;
pub mod error;
pub mod file_transfer;
pub mod interactive;
pub mod key;
#[cfg(feature = "local")]
pub mod local;
pub mod readonly;
pub mod secret;
pub mod sftp;
pub mod ssh;
mod shell_protocol;

use std::time::Duration;

use filar_core::Result;
use tokio::sync::mpsc;

// Re-export key types.
#[cfg(feature = "kube")]
pub use cluster::{
    ClusterApi, ConfigMapInfo, ContainerInfo, ContainerMetrics, EndpointInfo, EventInfo,
    IngressInfo, KubeClusterApi, KubeExecConfig, KubeExecEvent, KubeExecutor, KubeInteractive, NamespaceInfo, NodeCondition, NodeInfo, PodInfo, PodLogs,
    ResourceMetrics, RolloutRevision, ServiceInfo, ServicePortInfo, TerminationInfo, WorkloadInfo,
    WorkloadKind, DEFAULT_CLUSTER_LIST_LIMIT, DEFAULT_POD_SHELL, SHELL_RECREATED_MESSAGE,
};
pub use copy::{
    copy, CopyEvent, CopyObserver, CopyReport, CopySource, OverwriteDecision, PART_SUFFIX,
};
pub use cwd::{
    cd_input_for, cmd_osc7_prompt, is_safe_cwd, parse_pty_cwd_reply, posix_cd_command,
    posix_cd_input, posix_shell_quote, shell_flavor, usable_proc_cwd, ForegroundCwd, ShellFlavor,
    OSC7_PWD_PROBE, OSC7_RAW_HOST, POWERSHELL_OSC7_PROMPT, PTY_CWD_COMMAND,
};
pub use error::is_connection_lost;
pub use file_transfer::{
    DirEntry, EntryKind, FileTransfer, LocalFileTransfer, Metadata, PathStyle, TransferReader,
    TransferWriter,
};
#[cfg(feature = "local")]
pub use interactive::LocalInteractive;
pub use interactive::{InteractiveTerminal, SshInteractive};
pub use key::{
    fill_key_passphrase, key_passphrase_matches, key_protection, resolve_key_path, KeyPassphrase,
    KeyProtection,
};
#[cfg(feature = "local")]
pub use local::LocalExecutor;
pub use readonly::ReadOnlyExecutor;
pub use secret::SecretSubstitutingExecutor;
pub use sftp::SftpFileTransfer;
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
    /// Working directory after the command, when the transport can report it
    /// (SSH marker includes `$PWD`; local reports it after each command, #493).
    pub cwd: Option<String>,
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

    /// Point subsequent [`run`](Self::run) calls at `path` (tab cwd sync).
    ///
    /// Transport bookkeeping for Ctrl+T / OSC 7, not an LLM tool call.
    /// Default is a no-op. `path` must pass [`is_safe_cwd`].
    async fn set_cwd(&self, path: &str) -> Result<()> {
        let _ = path;
        Ok(())
    }

    /// Last known working directory for this executor, if tracked.
    async fn current_cwd(&self) -> Option<String> {
        None
    }
}

// Re-export the async_trait macro so downstream crates don't need a direct dep.
pub use async_trait::async_trait;
