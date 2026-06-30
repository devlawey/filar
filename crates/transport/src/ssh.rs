//! SSH transport: persistent shell channel over `russh`.
//!
//! This module implements the core "killer feature" — a single persistent
//! SSH shell channel through which commands are executed with marker-based
//! boundary detection and exit-code extraction. **Zero files are created
//! on the remote machine.**

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client::{self, Handle, Msg};
use russh::keys::*;
use russh::{Channel, ChannelMsg, Disconnect};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};
use uuid::Uuid;

use filar_core::{CoreError, Result, SshAuth, SshTarget};

use crate::CommandResult;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Unique prefix for marker lines. Chosen to be extremely unlikely to appear
/// in normal command output.
const MARKER_PREFIX: &str = "__FILAR_";

// ---------------------------------------------------------------------------
// SSH client handler
// ---------------------------------------------------------------------------

/// Handler for SSH session events.
///
/// Currently accepts all host keys (with a warning). TODO: verify against
/// `known_hosts`.
pub(crate) struct SshHandler;

impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        let fingerprint = server_public_key.fingerprint(ssh_key::HashAlg::Sha256);
        warn!(%fingerprint, "accepting unverified host key (known_hosts check not yet implemented)");
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Reader-task types
// ---------------------------------------------------------------------------

/// Commands sent **to** the channel-owning reader task.
#[derive(Debug)]
enum ChannelCmd {
    /// Write raw bytes to the SSH channel (payload, sync marker, etc.).
    Write(Vec<u8>),
    /// Send Ctrl-C (0x03) to interrupt the running command.
    Interrupt,
}

/// Events received **from** the channel-owning reader task.
#[derive(Debug)]
enum ChannelEvent {
    /// stdout data (`ChannelMsg::Data`).
    Data(String),
    /// stderr data (`ChannelMsg::ExtendedData` with `ext == 1`).
    Stderr(String),
    /// Channel was closed (EOF, Close, or `None` from `channel.wait()`).
    Closed,
}

// ---------------------------------------------------------------------------
// SshSession — persistent shell channel with reader-task
// ---------------------------------------------------------------------------

/// An SSH session maintaining a single persistent shell channel.
///
/// Commands are executed via [`SshSession::run`], which sends the command
/// followed by a marker `printf` and reads output until the marker is found.
/// Shell state (cwd, env) is preserved between commands because the channel
/// is never closed.
///
/// **Architecture:** A long-lived reader task owns the `Channel<Msg>` and is
/// the sole reader/writer. `run()` sends commands and reads events through
/// `mpsc` channels, while `cancel()` sends an `Interrupt` command that does
/// **not** compete for any lock held by `run()`.
pub struct SshSession {
    /// Sender to the reader task (write data, send interrupts).
    cmd_tx: mpsc::Sender<ChannelCmd>,
    /// Lightweight per-command mutex — serialises commands but is NOT
    /// needed by `cancel()`.
    run_lock: Mutex<()>,
    /// Event receiver from the reader task, wrapped in a mutex so `run()`
    /// can call `recv()` (which needs `&mut`). `cancel()` never touches this.
    event_rx: Mutex<mpsc::UnboundedReceiver<ChannelEvent>>,
    /// SSH session handle (for disconnect in `close()`). `Handle` contains a
    /// `JoinHandle` which is `Send` but not `Sync`, so we wrap in `Mutex`.
    session: Mutex<Handle<SshHandler>>,
    /// Monotonic counter for request IDs.
    req_counter: AtomicU64,
}

impl SshSession {
    /// Connect to an SSH target, authenticate, open a shell channel, and
    /// synchronise the initial output.
    pub async fn connect(target: &SshTarget) -> Result<Self> {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(300)),
            ..<_>::default()
        });

        let addr = (target.host.as_str(), target.port);
        info!(host = %target.host, port = target.port, user = %target.user, "connecting via SSH");

        let mut session = client::connect(config, addr, SshHandler)
            .await
            .map_err(|e| CoreError::Other(format!("SSH connect failed: {e}")))?;

        // ── Authenticate ───────────────────────────────────────────────
        match &target.auth {
            SshAuth::Key { path } => {
                let key_path = path
                    .clone()
                    .unwrap_or_else(|| {
                        dirs_or_default()
                    });
                let key_pair = load_secret_key(&key_path, None)
                    .map_err(|e| CoreError::Other(format!("failed to load SSH key {:?}: {e}", key_path)))?;

                let hash = session
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|e| CoreError::Other(format!("RSA hash negotiation failed: {e}")))?
                    .flatten();

                let auth_res = session
                    .authenticate_publickey(
                        &target.user,
                        PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash),
                    )
                    .await
                    .map_err(|e| CoreError::Other(format!("publickey auth failed: {e}")))?;

                if !auth_res.success() {
                    return Err(CoreError::Other("publickey authentication rejected".into()));
                }
                info!("SSH authenticated via key");
            }

            SshAuth::Password { password } => {
                let password = password.clone()
                    .or_else(|| std::env::var("SSH_PASSWORD").ok())
                    .ok_or_else(|| CoreError::Secret("no password provided (set SSH_PASSWORD env var or enter in GUI)".into()))?;

                let auth_res = session
                    .authenticate_password(&target.user, &password)
                    .await
                    .map_err(|e| CoreError::Other(format!("password auth failed: {e}")))?;

                if !auth_res.success() {
                    return Err(CoreError::Other("password authentication rejected".into()));
                }
                info!("SSH authenticated via password");
            }

            SshAuth::Agent => {
                return Err(CoreError::Other("SSH agent authentication not yet implemented".into()));
            }
        }

        // ── Open persistent shell channel ──────────────────────────────
        let mut channel = session
            .channel_open_session()
            .await
            .map_err(|e| CoreError::Other(format!("failed to open channel: {e}")))?;

        // Request a shell (no PTY — we want clean output without echo).
        channel
            .request_shell(true)
            .await
            .map_err(|e| CoreError::Other(format!("failed to request shell: {e}")))?;

        // ── Sync: drain initial shell output ───────────────────────────
        // Send a unique sync marker and wait for it to appear. This is done
        // directly on the channel BEFORE spawning the reader task.
        let sync_id = format!("sync_{}", Uuid::new_v4().simple());
        let sync_cmd = format!(
            "printf '\\n{}{}\\n'\n",
            MARKER_PREFIX, sync_id
        );
        channel
            .data(sync_cmd.as_bytes())
            .await
            .map_err(|e| CoreError::Other(format!("failed to send sync marker: {e}")))?;

        let sync_marker = format!("{}{}", MARKER_PREFIX, sync_id);
        drain_until_marker(&mut channel, &sync_marker, Duration::from_secs(10))
            .await?;

        // ── Spawn reader task ──────────────────────────────────────────
        // The task is the sole owner of `Channel<Msg>` from this point on.
        let (cmd_tx, cmd_rx) = mpsc::channel::<ChannelCmd>(16);
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ChannelEvent>();

        tokio::spawn(reader_task(channel, cmd_rx, event_tx));

        info!("SSH shell channel ready and synchronised");

        Ok(Self {
            cmd_tx,
            run_lock: Mutex::new(()),
            event_rx: Mutex::new(event_rx),
            session: Mutex::new(session),
            req_counter: AtomicU64::new(0),
        })
    }

    /// Execute a command on the remote shell and return its output.
    ///
    /// The command is sent to the persistent shell channel followed by a
    /// marker `printf` that emits a unique line with the exit code. Output
    /// is collected until the marker is found.
    ///
    /// If the command times out, Ctrl-C is sent to the shell and a resync
    /// marker is used to restore the channel to a known state.
    pub async fn run(&self, command: &str) -> Result<CommandResult> {
        // Serialize commands — only one `run()` at a time.
        let _guard = self.run_lock.lock().await;

        let req_id = self.req_counter.fetch_add(1, Ordering::Relaxed);
        let marker_id = format!("req_{:08x}", req_id);
        let marker_tag = format!("{}{}", MARKER_PREFIX, marker_id);

        // Build the full payload: <command>\n printf marker\n
        let payload = format!(
            "{}\nprintf '\\n{}__%d__\\n' \"$?\"\n",
            command, marker_tag
        );

        let start = Instant::now();

        // Send the payload to the reader task (which owns the channel).
        self.cmd_tx
            .send(ChannelCmd::Write(payload.into_bytes()))
            .await
            .map_err(|_| CoreError::Other("channel task closed".into()))?;

        // Lock the event receiver. `cancel()` never touches this mutex.
        let mut event_rx = self.event_rx.lock().await;

        // Drain any stale events from a previous command.
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(10), event_rx.recv()).await
        {
            debug!(?event, "draining stale event");
        }

        // Read events until we find the marker — without holding any lock
        // that `cancel()` needs.
        match recv_until_marker(&mut event_rx, &marker_tag, Duration::from_secs(120)).await {
            Ok((stdout, stderr, exit_code)) => {
                let duration = start.elapsed();
                Ok(CommandResult {
                    stdout,
                    stderr,
                    exit_code,
                    duration,
                })
            }
            Err(e) => {
                // Command timed out or failed. Send Ctrl-C to interrupt
                // any running process, then resync the shell.
                warn!(error = %e, "command timed out, sending Ctrl-C and resyncing");
                let _ = self.cmd_tx.send(ChannelCmd::Interrupt).await;
                // Drain any remaining output and resync.
                let sync_id = format!("sync_{}", Uuid::new_v4().simple());
                let sync_cmd = format!(
                    "printf '\\n{}{}\\n'\n",
                    MARKER_PREFIX, sync_id
                );
                let _ = self
                    .cmd_tx
                    .send(ChannelCmd::Write(sync_cmd.into_bytes()))
                    .await;
                let sync_marker = format!("{}{}", MARKER_PREFIX, sync_id);
                let _ = recv_until_marker_simple(&mut event_rx, &sync_marker, Duration::from_secs(10))
                    .await;
                Err(e)
            }
        }
    }

    /// Send Ctrl-C to interrupt the currently running command.
    ///
    /// This sends an `Interrupt` command to the reader task via `cmd_tx`.
    /// It does **not** acquire any lock that `run()` holds, so it works
    /// even while a command is executing.
    pub async fn cancel(&self) -> Result<()> {
        self.cmd_tx
            .send(ChannelCmd::Interrupt)
            .await
            .map_err(|_| CoreError::Other("failed to send Ctrl-C: channel task closed".into()))?;
        Ok(())
    }

    /// Disconnect the SSH session.
    pub async fn close(&self) -> Result<()> {
        let session = self.session.lock().await;
        let _ = session
            .disconnect(Disconnect::ByApplication, "", "English")
            .await;
        info!("SSH session closed");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reader task — sole owner of `Channel<Msg>`
// ---------------------------------------------------------------------------

/// Long-lived task that owns the SSH `Channel` and is the sole reader/writer.
///
/// It accepts commands (`Write`, `Interrupt`) via `cmd_rx` and forwards
/// channel events (`Data`, `Stderr`, `Closed`) via `event_tx`.
async fn reader_task(
    mut channel: Channel<Msg>,
    mut cmd_rx: mpsc::Receiver<ChannelCmd>,
    event_tx: mpsc::UnboundedSender<ChannelEvent>,
) {
    debug!("SSH reader task started");
    loop {
        tokio::select! {
            // ── Incoming commands ──────────────────────────────────────
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(ChannelCmd::Write(data)) => {
                        if let Err(e) = channel.data(&data[..]).await {
                            warn!(error = %e, "reader: failed to write to channel");
                            let _ = event_tx.send(ChannelEvent::Closed);
                            break;
                        }
                    }
                    Some(ChannelCmd::Interrupt) => {
                        debug!("reader: sending Ctrl-C (0x03)");
                        if let Err(e) = channel.data(&b"\x03"[..]).await {
                            warn!(error = %e, "reader: failed to send Ctrl-C");
                            let _ = event_tx.send(ChannelEvent::Closed);
                            break;
                        }
                    }
                    None => {
                        // cmd_tx dropped — SshSession is shutting down.
                        debug!("reader: cmd_rx closed, exiting");
                        break;
                    }
                }
            }
            // ── Channel events ─────────────────────────────────────────
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { ref data }) => {
                        let text = String::from_utf8_lossy(data.as_ref()).to_string();
                        if event_tx.send(ChannelEvent::Data(text)).is_err() {
                            // event_rx dropped — no one is listening.
                            debug!("reader: event_tx closed, exiting");
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { ref data, ext }) if ext == 1 => {
                        debug!(len = data.len(), "stderr data received from SSH channel");
                        let text = String::from_utf8_lossy(data.as_ref()).to_string();
                        if event_tx.send(ChannelEvent::Stderr(text)).is_err() {
                            debug!("reader: event_tx closed, exiting");
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { .. }) => {
                        // Other extended data — ignore.
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                        warn!("reader: channel closed (EOF/Close/None)");
                        let _ = event_tx.send(ChannelEvent::Closed);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    debug!("SSH reader task exited");
}

// ---------------------------------------------------------------------------
// SshExecutor — CommandExecutor impl
// ---------------------------------------------------------------------------

/// [`CommandExecutor`] implementation backed by an SSH session.
pub struct SshExecutor {
    session: SshSession,
}

impl SshExecutor {
    /// Create a new SSH executor by connecting to the given target.
    pub async fn connect(target: &SshTarget) -> Result<Self> {
        let session = SshSession::connect(target).await?;
        Ok(Self { session })
    }

    /// Close the underlying session.
    pub async fn close(&self) -> Result<()> {
        self.session.close().await
    }
}

#[async_trait::async_trait]
impl crate::CommandExecutor for SshExecutor {
    async fn run(&self, command: &str) -> Result<CommandResult> {
        self.session.run(command).await
    }

    async fn cancel(&self) -> Result<()> {
        self.session.cancel().await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Default SSH key path: `~/.ssh/id_ed25519` (falls back to `id_rsa`).
pub(crate) fn dirs_or_default() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    let ed25519 = std::path::PathBuf::from(&home).join(".ssh/id_ed25519");
    if ed25519.exists() {
        return ed25519;
    }
    std::path::PathBuf::from(&home).join(".ssh/id_rsa")
}

/// Read from the channel until `marker` is found in the accumulated output.
/// Returns the output *before* the marker (discarded for sync).
///
/// This is used directly on the `Channel` in `connect()` **before** the
/// reader task is spawned.
async fn drain_until_marker(
    channel: &mut Channel<Msg>,
    marker: &str,
    timeout: Duration,
) -> Result<()> {
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let msg = tokio::time::timeout_at(deadline, channel.wait())
            .await
            .map_err(|_| CoreError::Other(format!("timeout waiting for marker `{marker}`")))?
            .ok_or_else(|| CoreError::Other("channel closed before marker received".into()))?;

        match msg {
            ChannelMsg::Data { ref data } => {
                buf.push_str(&String::from_utf8_lossy(data.as_ref()));
                if let Some(pos) = buf.find(marker) {
                    // Found the marker — drain everything up to and including it.
                    buf.drain(..=pos + marker.len());
                    return Ok(());
                }
            }
            ChannelMsg::ExtendedData { .. } => {
                // Extended data (e.g. stderr) — drain silently during sync.
                // Sync markers are sent via stdout, so we don't need to collect this.
            }
            ChannelMsg::Eof | ChannelMsg::Close => {
                return Err(CoreError::Other(
                    "channel closed before marker received".into(),
                ));
            }
            _ => {}
        }
    }
}

/// Read events from the reader task until a marker of the form
/// `<marker_tag>__<exit_code>__` is found. Returns the output before the
/// marker and the parsed exit code.
///
/// Unlike `drain_until_marker`, this reads from an `mpsc::UnboundedReceiver`
/// and does NOT touch the SSH channel directly — the reader task does that.
async fn recv_until_marker(
    event_rx: &mut mpsc::UnboundedReceiver<ChannelEvent>,
    marker_tag: &str,
    timeout: Duration,
) -> Result<(String, String, Option<i32>)> {
    let mut buf = String::new();
    let mut stderr_buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let event = tokio::time::timeout_at(deadline, event_rx.recv())
            .await
            .map_err(|_| CoreError::Other(format!("timeout waiting for command marker `{marker_tag}`")))?
            .ok_or_else(|| CoreError::Other("channel closed before marker received".into()))?;

        match event {
            ChannelEvent::Data(text) => {
                buf.push_str(&text);

                // Look for the marker line: <marker_tag>__<digits>__
                if let Some(pos) = buf.find(marker_tag) {
                    let after_tag = &buf[pos + marker_tag.len()..];
                    if let Some(rest) = after_tag.strip_prefix("__") {
                        if let Some(end) = rest.find("__") {
                            let code_str = &rest[..end];
                            if let Ok(code) = code_str.trim().parse::<i32>() {
                                let line_start =
                                    buf[..pos].rfind('\n').map(|p| p + 1).unwrap_or(0);
                                let output = buf[..line_start].to_string();

                                // Drain any trailing stderr that may still be
                                // in the event pipeline. Without this, late
                                // stderr could contaminate the next run's result.
                                loop {
                                    match tokio::time::timeout(
                                        Duration::from_millis(50),
                                        event_rx.recv(),
                                    )
                                    .await
                                    {
                                        Ok(Some(ChannelEvent::Stderr(text))) => {
                                            stderr_buf.push_str(&text);
                                        }
                                        _ => break,
                                    }
                                }
                                return Ok((output, stderr_buf, Some(code)));
                            }
                        }
                    }
                    // Marker tag found but couldn't parse exit code — keep reading.
                }
            }
            ChannelEvent::Stderr(text) => {
                stderr_buf.push_str(&text);
            }
            ChannelEvent::Closed => {
                return Err(CoreError::Other(
                    "channel closed before command marker received".into(),
                ));
            }
        }
    }
}

/// Read events from the reader task until `marker` is found. Used for
/// resync after a timeout/cancel — output is discarded.
async fn recv_until_marker_simple(
    event_rx: &mut mpsc::UnboundedReceiver<ChannelEvent>,
    marker: &str,
    timeout: Duration,
) -> Result<()> {
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let event = tokio::time::timeout_at(deadline, event_rx.recv())
            .await
            .map_err(|_| CoreError::Other(format!("timeout waiting for marker `{marker}`")))?
            .ok_or_else(|| CoreError::Other("channel closed before marker received".into()))?;

        match event {
            ChannelEvent::Data(text) => {
                buf.push_str(&text);
                if buf.find(marker).is_some() {
                    return Ok(());
                }
            }
            ChannelEvent::Stderr(_) => {
                // Ignore stderr during sync.
            }
            ChannelEvent::Closed => {
                return Err(CoreError::Other(
                    "channel closed before marker received".into(),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_format() {
        let marker_id = "req_00000001";
        let marker_tag = format!("{}{}", MARKER_PREFIX, marker_id);
        assert_eq!(marker_tag, "__FILAR_req_00000001");
    }

    #[test]
    fn payload_format() {
        let command = "echo hello";
        let marker_tag = "__FILAR_req_00000001";
        let payload = format!(
            "{}\nprintf '\\n{}__%d__\\n' \"$?\"\n",
            command, marker_tag
        );
        assert!(payload.contains("echo hello"));
        assert!(payload.contains("__FILAR_req_00000001__%d__"));
    }

    /// Integration test — requires a running SSH server.
    /// Start one with: docker run -d -p 2222:22 --name filar-sshd filar-sshd
    #[tokio::test]
    #[ignore = "requires Docker sshd container on port 2222"]
    async fn ssh_run_basic() {
        let target = SshTarget {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "testuser".into(),
            auth: SshAuth::Password { password: None },
        };
        std::env::set_var("SSH_PASSWORD", "testpassword");

        let session = SshSession::connect(&target).await.unwrap();
        let result = session.run("echo hi").await.unwrap();
        assert_eq!(result.stdout.trim(), "hi");
        assert_eq!(result.exit_code, Some(0));

        let result = session.run("false").await.unwrap();
        assert_eq!(result.exit_code, Some(1));

        session.close().await.unwrap();
    }

    /// Integration test — verifies shell state preservation.
    #[tokio::test]
    #[ignore = "requires Docker sshd container on port 2222"]
    async fn ssh_state_preservation() {
        let target = SshTarget {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "testuser".into(),
            auth: SshAuth::Password { password: None },
        };
        std::env::set_var("SSH_PASSWORD", "testpassword");

        let session = SshSession::connect(&target).await.unwrap();

        // cd /tmp then pwd → should be /tmp
        session.run("cd /tmp").await.unwrap();
        let result = session.run("pwd").await.unwrap();
        assert_eq!(result.stdout.trim(), "/tmp");

        session.close().await.unwrap();
    }

    /// Integration test — verifies zero-install (no files left on remote).
    #[tokio::test]
    #[ignore = "requires Docker sshd container on port 2222"]
    async fn ssh_zero_install() {
        let target = SshTarget {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "testuser".into(),
            auth: SshAuth::Password { password: None },
        };
        std::env::set_var("SSH_PASSWORD", "testpassword");

        let session = SshSession::connect(&target).await.unwrap();

        // Count files in home before
        let before = session.run("find ~ -type f | wc -l").await.unwrap();

        // Run some commands
        session.run("echo hello").await.unwrap();
        session.run("ls /tmp").await.unwrap();
        session.run("whoami").await.unwrap();

        // Count files in home after
        let after = session.run("find ~ -type f | wc -l").await.unwrap();

        assert_eq!(before.stdout.trim(), after.stdout.trim(),
            "zero-install violated: file count changed");

        session.close().await.unwrap();
    }

    /// Integration test — verifies stderr capture from a failing command.
    #[tokio::test]
    #[ignore = "requires Docker sshd container on port 2222"]
    async fn ssh_stderr_capture() {
        let target = SshTarget {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "testuser".into(),
            auth: SshAuth::Password { password: None },
        };
        std::env::set_var("SSH_PASSWORD", "testpassword");

        let session = SshSession::connect(&target).await.unwrap();

        // `ls` on a non-existent path writes to stderr and exits non-zero.
        let result = session.run("ls /nonexistent_path_xyz").await.unwrap();

        let code = result.exit_code.expect("expected command to report an exit code");
        assert_ne!(code, 0, "expected non-zero exit code");
        assert!(
            !result.stderr.is_empty(),
            "stderr should not be empty for a failing command"
        );
        assert!(
            result.stderr.contains("No such file or directory"),
            "stderr should contain error message, got: {:?}",
            result.stderr
        );

        session.close().await.unwrap();
    }

    /// Integration test — verifies that cancel() interrupts a long-running
    /// command before its natural completion.
    ///
    /// Expected behaviour:
    /// 1. `run("sleep 30")` is started in a background task.
    /// 2. After 1 second, `cancel()` is called.
    /// 3. `run()` should return quickly (well under 30 seconds) — either
    ///    with an error (timeout) or with a non-zero exit code (SIGINT = 130).
    /// 4. The shell remains usable: a subsequent `run("echo ok")` returns
    ///    `ok` with exit code 0 (thanks to resync).
    #[tokio::test]
    #[ignore = "requires Docker sshd container on port 2222"]
    async fn ssh_cancel_interrupts_long_command() {
        let target = SshTarget {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "testuser".into(),
            auth: SshAuth::Password { password: None },
        };
        std::env::set_var("SSH_PASSWORD", "testpassword");

        let session = Arc::new(SshSession::connect(&target).await.unwrap());

        // Spawn the long-running command in a separate task.
        let session_clone = session.clone();
        let run_task = tokio::spawn(async move {
            session_clone.run("sleep 30").await
        });

        // Give the command a moment to start, then cancel.
        tokio::time::sleep(Duration::from_secs(1)).await;
        session.cancel().await.unwrap();

        // The command should return quickly (well under 30 seconds).
        let start = Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(15), run_task)
            .await
            .expect("cancel did not interrupt the command in time");
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(15),
            "cancel took too long: {elapsed:?}"
        );

        // The result should be either an error (timeout/channel) or a
        // non-zero exit code (SIGINT = 130). A `Some(0)` means the command
        // was NOT interrupted — that's a failure.
        match result {
            Ok(Ok(r)) => {
                assert_ne!(
                    r.exit_code,
                    Some(0),
                    "expected non-zero exit code after cancel, got: {:?}",
                    r.exit_code
                );
            }
            // Error from run() (timeout) or JoinError — both acceptable.
            Ok(Err(_)) | Err(_) => {}
        }

        // Verify the shell is still usable after cancel.
        let result = session.run("echo ok").await.unwrap();
        assert_eq!(result.stdout.trim(), "ok");
        assert_eq!(result.exit_code, Some(0));

        session.close().await.unwrap();
    }
}
