//! SSH transport: persistent shell channel over `russh`.
//!
//! This module implements the core "killer feature" — a single persistent
//! SSH shell channel through which commands are executed with marker-based
//! boundary detection and exit-code extraction. **Zero files are created
//! on the remote machine.**

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client::{self, Handle, Msg};
use russh::keys::*;
use russh::{Channel, ChannelMsg, Disconnect};
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{debug, info, warn};
use uuid::Uuid;

use filar_core::{CoreError, HostKeyPolicy, Result, SshAuth, SshTarget};

use crate::CommandResult;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Unique prefix for marker lines. Chosen to be extremely unlikely to appear
/// in normal command output.
const MARKER_PREFIX: &str = "__FILAR_";

/// Default keepalive interval — send a keepalive if nothing is received from
/// the server for this long. Chosen well below typical NAT/idle-connection
/// timeouts so a resting session never dies.
const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// Default number of unanswered keepalives before the connection is considered
/// dead. `20s * 3 ≈ 60s` grace before russh tears the session down.
const DEFAULT_KEEPALIVE_MAX: usize = 3;

// ---------------------------------------------------------------------------
// Transport config
// ---------------------------------------------------------------------------

/// Tunables for the SSH transport.
///
/// Defaults keep an idle session alive effectively indefinitely (as long as
/// the network is up) and enable a single silent reconnect+retry when a command
/// is issued on a connection that died while resting.
#[derive(Debug, Clone)]
pub struct SshTransportConfig {
    /// Send a keepalive if nothing is received from the server for this long.
    /// `None` disables keepalive entirely.
    pub keepalive_interval: Option<Duration>,
    /// Number of unanswered keepalives after which the connection is closed.
    pub keepalive_max: usize,
    /// When `true`, a command that fails because the connection was lost
    /// *before dispatch* triggers one silent reconnect and a single retry.
    /// A command that may already have executed is never auto-retried.
    pub auto_reconnect: bool,
}

impl Default for SshTransportConfig {
    fn default() -> Self {
        Self {
            keepalive_interval: Some(DEFAULT_KEEPALIVE_INTERVAL),
            keepalive_max: DEFAULT_KEEPALIVE_MAX,
            auto_reconnect: true,
        }
    }
}

// ---------------------------------------------------------------------------
// SSH client handler
// ---------------------------------------------------------------------------

/// Handler for SSH session events.
///
/// Implements TOFU (trust on first use) host key verification via a
/// `known_hosts` file at `~/.config/filar/known_hosts`.
pub(crate) struct SshHandler {
    /// Remote host (for known_hosts lookup).
    pub(crate) host: String,
    /// Remote port.
    pub(crate) port: u16,
    /// Host key verification policy.
    pub(crate) policy: HostKeyPolicy,
    /// Path to the known_hosts file.
    pub(crate) known_hosts_path: PathBuf,
}

impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        let fingerprint = server_public_key.fingerprint(ssh_key::HashAlg::Sha256);
        let fp_str = fingerprint.to_string();
        let host_port = format!("{}:{}", self.host, self.port);

        let entries = match parse_known_hosts(&self.known_hosts_path) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warn!(
                    host = %host_port,
                    error = %e,
                    "failed to read known_hosts, rejecting host key"
                );
                return Ok(false);
            }
        };

        match check_host_key(&entries, &host_port, &fp_str) {
            HostKeyCheck::Match => {
                debug!(host = %host_port, "host key verified (matches known_hosts)");
                Ok(true)
            }
            HostKeyCheck::Mismatch => {
                warn!(
                    host = %host_port,
                    "HOST KEY MISMATCH — possible MITM, rejecting"
                );
                Ok(false)
            }
            HostKeyCheck::New => match self.policy {
                HostKeyPolicy::Strict => {
                    warn!(
                        host = %host_port,
                        "unknown host key (strict mode — rejecting)"
                    );
                    Ok(false)
                }
                HostKeyPolicy::Tofu => {
                    match append_known_hosts_entry(
                        &self.known_hosts_path,
                        &host_port,
                        &fp_str,
                    ) {
                        Ok(()) => {
                            info!(host = %host_port, "host key added (TOFU)");
                            Ok(true)
                        }
                        Err(e) => {
                            warn!(
                                host = %host_port,
                                error = %e,
                                "failed to write known_hosts entry, rejecting host key"
                            );
                            Ok(false)
                        }
                    }
                }
                HostKeyPolicy::AcceptNew => {
                    info!(
                        host = %host_port,
                        "host key accepted (accept-new, not recorded)"
                    );
                    Ok(true)
                }
            },
        }
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
    /// Set to `true` when teardown is intentional (`close()` or being replaced
    /// on reconnect). The reader task reads this to decide whether a channel
    /// closure is expected (INFO) or unexpected (WARN).
    shutdown: Arc<AtomicBool>,
}

impl SshSession {
    /// Connect to an SSH target with the default transport config.
    pub async fn connect(target: &SshTarget) -> Result<Self> {
        Self::connect_with_config(target, &SshTransportConfig::default()).await
    }

    /// Connect to an SSH target, authenticate, open a shell channel, and
    /// synchronise the initial output.
    ///
    /// `cfg` controls keepalive; keepalive is what keeps an idle session from
    /// being reaped by the server/NAT after a few minutes of no traffic.
    pub async fn connect_with_config(
        target: &SshTarget,
        cfg: &SshTransportConfig,
    ) -> Result<Self> {
        let config = Arc::new(client::Config {
            // Keep the inactivity guard well above the keepalive interval so a
            // healthy session (keepalives answered) never trips it.
            inactivity_timeout: Some(Duration::from_secs(300)),
            keepalive_interval: cfg.keepalive_interval,
            keepalive_max: cfg.keepalive_max,
            ..<_>::default()
        });

        let addr = (target.host.as_str(), target.port);
        info!(host = %target.host, port = target.port, user = %target.user, "connecting via SSH");

        let handler = SshHandler {
            host: target.host.clone(),
            port: target.port,
            policy: target.host_key_policy,
            known_hosts_path: known_hosts_path(),
        };
        let mut session = client::connect(config, addr, handler)
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

        let shutdown = Arc::new(AtomicBool::new(false));
        tokio::spawn(reader_task(channel, cmd_rx, event_tx, shutdown.clone()));

        info!("SSH shell channel ready and synchronised");

        Ok(Self {
            cmd_tx,
            run_lock: Mutex::new(()),
            event_rx: Mutex::new(event_rx),
            session: Mutex::new(session),
            req_counter: AtomicU64::new(0),
            shutdown,
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
        let marker_id = format!("req_{:08x}_{}", req_id, Uuid::new_v4().simple());
        let marker_tag = format!("{}{}", MARKER_PREFIX, marker_id);

        // Build the full payload: <command>\n printf marker\n
        let payload = format!(
            "{}\nprintf '\\n{}__%d__\\n' \"$?\"\n",
            command, marker_tag
        );

        let start = Instant::now();

        // Lock the event receiver. `cancel()` never touches this mutex.
        let mut event_rx = self.event_rx.lock().await;

        // Drain any stale events from a previous command BEFORE sending
        // the new payload — otherwise fast commands' output could be
        // discarded as "stale".
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(10), event_rx.recv()).await
        {
            // Log only metadata — raw output may contain secrets.
            let (kind, len) = match &event {
                ChannelEvent::Data(text) => ("stdout", text.len()),
                ChannelEvent::Stderr(text) => ("stderr", text.len()),
                ChannelEvent::Closed => ("closed", 0),
            };
            debug!(kind, len, "draining stale event");
        }

        // Send the payload to the reader task (which owns the channel).
        //
        // A failure here means the reader task is already gone (the channel
        // died — typically an idle session reaped by the server/NAT). Crucially,
        // the command bytes have NOT reached the wire yet, so this is the one
        // point where an automatic reconnect+retry is safe — signalled via the
        // dedicated `ConnectionLost` variant.
        self.cmd_tx
            .send(ChannelCmd::Write(payload.into_bytes()))
            .await
            .map_err(|_| CoreError::ConnectionLost("ssh channel task closed".into()))?;

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

    /// Mark this session as intentionally shutting down.
    ///
    /// Signals the reader task that an imminent channel closure is expected, so
    /// it logs at INFO rather than WARN. Used both by [`close`](Self::close) and
    /// when the session is replaced during a reconnect.
    pub(crate) fn mark_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Disconnect the SSH session.
    pub async fn close(&self) -> Result<()> {
        self.mark_shutdown();
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
    shutdown: Arc<AtomicBool>,
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
                    Some(ChannelMsg::ExtendedData { ref data, ext: 1 }) => {
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
                        // Expected closure (we are tearing the session down) is
                        // routine — log at INFO. An unexpected closure (idle
                        // reap, network drop) is worth a WARN and, via the chat
                        // log mirror, is surfaced to the user.
                        if shutdown.load(Ordering::Relaxed) {
                            info!("reader: channel closed (shutdown)");
                        } else {
                            warn!("reader: channel closed unexpectedly (EOF/Close/None)");
                        }
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
///
/// The session lives behind an [`RwLock`] so it can be swapped out on
/// reconnect without disturbing in-flight readers. `run()`/`cancel()` take a
/// *read* guard (they run concurrently — `cancel()` must work while `run()` is
/// awaiting), while a reconnect briefly takes the *write* guard to install a
/// fresh session.
pub struct SshExecutor {
    session: RwLock<SshSession>,
    /// Target + credentials, retained so a dropped connection can be re-dialled
    /// with the same parameters. Held in memory only (never logged).
    target: SshTarget,
    config: SshTransportConfig,
}

impl SshExecutor {
    /// Create a new SSH executor by connecting to the given target with the
    /// default transport config.
    pub async fn connect(target: &SshTarget) -> Result<Self> {
        Self::connect_with_config(target, SshTransportConfig::default()).await
    }

    /// Create a new SSH executor with an explicit transport config.
    pub async fn connect_with_config(
        target: &SshTarget,
        config: SshTransportConfig,
    ) -> Result<Self> {
        let session = SshSession::connect_with_config(target, &config).await?;
        Ok(Self {
            session: RwLock::new(session),
            target: target.clone(),
            config,
        })
    }

    /// Close the underlying session.
    pub async fn close(&self) -> Result<()> {
        self.session.read().await.close().await
    }
}

#[async_trait::async_trait]
impl crate::CommandExecutor for SshExecutor {
    async fn run(&self, command: &str) -> Result<CommandResult> {
        // First attempt on the current session.
        let first = { self.session.read().await.run(command).await };

        // Reconnect only when the connection was lost *before* the command was
        // dispatched (the `ConnectionLost` variant). A command that may have
        // started executing surfaces as a different error and is never retried
        // automatically — this preserves the "no silent re-execution" invariant.
        let should_reconnect =
            self.config.auto_reconnect && matches!(first, Err(CoreError::ConnectionLost(_)));
        if !should_reconnect {
            return first;
        }

        info!("ssh connection lost before dispatch — attempting one silent reconnect");

        // Install a fresh session under the write guard, then drop the guard
        // before retrying so `cancel()` can still proceed during the retry.
        {
            let mut guard = self.session.write().await;
            match SshSession::connect_with_config(&self.target, &self.config).await {
                Ok(fresh) => {
                    let old = std::mem::replace(&mut *guard, fresh);
                    // The old session is already dead here; mark it so its
                    // reader task (if any lingers) treats closure as expected.
                    old.mark_shutdown();
                }
                Err(reconnect_err) => {
                    warn!(error = %reconnect_err, "ssh reconnect failed");
                    // Return the original connection-lost error, not the
                    // reconnect failure — the user's command never ran.
                    return first;
                }
            }
        }

        // Surfaced to the chat via the WARN→System log mirror (issue #57).
        warn!(
            "reconnected to {}:{}",
            self.target.host, self.target.port
        );

        // Single retry on the fresh session.
        self.session.read().await.run(command).await
    }

    async fn cancel(&self) -> Result<()> {
        self.session.read().await.cancel().await
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

/// Default known_hosts path: `~/.config/filar/known_hosts`.
pub(crate) fn known_hosts_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(&home)
        .join(".config")
        .join("filar")
        .join("known_hosts")
}

/// Parse known_hosts file contents into a `host:port → fingerprint` map.
fn parse_known_hosts_contents(contents: &str) -> HashMap<String, String> {
    let mut entries = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        if let (Some(host), Some(fp)) = (parts.next(), parts.next()) {
            entries.insert(host.trim().to_string(), fp.trim().to_string());
        }
    }
    entries
}

/// Read and parse the known_hosts file.
///
/// Returns `Ok(empty_map)` when the file doesn't exist (first connection).
/// Returns `Err` for any other I/O error — callers should reject the host key
/// (fail closed) rather than silently downgrading verification.
fn parse_known_hosts(path: &Path) -> std::io::Result<HashMap<String, String>> {
    std::fs::read_to_string(path)
        .map(|contents| parse_known_hosts_contents(&contents))
}

/// Append a new entry to the known_hosts file. Creates the file (and parent
/// directories) if it doesn't exist.
fn append_known_hosts_entry(
    path: &Path,
    host_port: &str,
    fingerprint: &str,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let exists = path.exists();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if !exists {
        writeln!(file, "# filar known_hosts — do not edit manually")?;
    }
    writeln!(file, "{host_port} {fingerprint}")?;
    Ok(())
}

/// Result of comparing a server key against known_hosts.
#[derive(Debug, PartialEq, Eq)]
enum HostKeyCheck {
    /// Key matches the known entry.
    Match,
    /// Key doesn't match — possible MITM.
    Mismatch,
    /// No entry for this host — first connection.
    New,
}

/// Check a fingerprint against known_hosts entries.
fn check_host_key(
    entries: &HashMap<String, String>,
    host_port: &str,
    fingerprint: &str,
) -> HostKeyCheck {
    match entries.get(host_port) {
        Some(known_fp) if known_fp == fingerprint => HostKeyCheck::Match,
        Some(_) => HostKeyCheck::Mismatch,
        None => HostKeyCheck::New,
    }
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
                                // Strip the synthetic trailing newline added by the
                                // printf marker's leading '\n'. This removes
                                // exactly one '\n' — not the command's own output.
                                let output = output
                                    .strip_suffix('\n')
                                    .map(str::to_string)
                                    .unwrap_or(output);

                                // Drain any trailing stderr that may still be
                                // in the event pipeline. Without this, late
                                // stderr could contaminate the next run's result.
                                while let Ok(Some(ChannelEvent::Stderr(text))) =
                                    tokio::time::timeout(
                                        Duration::from_millis(50),
                                        event_rx.recv(),
                                    )
                                    .await
                                {
                                    stderr_buf.push_str(&text);
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
    /// Set SSH_PASSWORD env var, e.g. `set SSH_PASSWORD=testpassword`.
    #[tokio::test]
    #[ignore = "requires Docker sshd container on port 2222"]
    async fn ssh_run_basic() {
        let target = SshTarget {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "testuser".into(),
            auth: SshAuth::Password { password: None },
            host_key_policy: HostKeyPolicy::Tofu,
        };
        std::env::var("SSH_PASSWORD")
            .expect("set SSH_PASSWORD to run ignored SSH integration tests");

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
            host_key_policy: HostKeyPolicy::Tofu,
        };
        std::env::var("SSH_PASSWORD")
            .expect("set SSH_PASSWORD to run ignored SSH integration tests");

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
            host_key_policy: HostKeyPolicy::Tofu,
        };
        std::env::var("SSH_PASSWORD")
            .expect("set SSH_PASSWORD to run ignored SSH integration tests");

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
            host_key_policy: HostKeyPolicy::Tofu,
        };
        std::env::var("SSH_PASSWORD")
            .expect("set SSH_PASSWORD to run ignored SSH integration tests");

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
            host_key_policy: HostKeyPolicy::Tofu,
        };
        std::env::var("SSH_PASSWORD")
            .expect("set SSH_PASSWORD to run ignored SSH integration tests");

        let session = Arc::new(SshSession::connect(&target).await.unwrap());

        // Spawn the long-running command in a separate task.
        let session_clone = session.clone();
        let run_task = tokio::spawn(async move {
            session_clone.run("sleep 30").await
        });

        // Give the command a moment to start, then cancel.
        tokio::time::sleep(Duration::from_secs(1)).await;
        tokio::time::timeout(Duration::from_secs(2), session.cancel())
            .await
            .expect("cancel() blocked")
            .unwrap();

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
        if let Ok(Ok(r)) = result {
            assert_ne!(
                r.exit_code,
                Some(0),
                "expected non-zero exit code after cancel, got: {:?}",
                r.exit_code
            );
        }

        // Verify the shell is still usable after cancel.
        let result = session.run("echo ok").await.unwrap();
        assert_eq!(result.stdout.trim(), "ok");
        assert_eq!(result.exit_code, Some(0));

        session.close().await.unwrap();
    }

    // ── known_hosts unit tests ───────────────────────────────────────

    #[test]
    fn known_hosts_parse_contents() {
        let contents = "# filar known_hosts\n127.0.0.1:2222 SHA256:abc123\n10.0.0.5:22 SHA256:def456\n\n# comment line\n";
        let entries = parse_known_hosts_contents(contents);
        assert_eq!(
            entries.get("127.0.0.1:2222"),
            Some(&"SHA256:abc123".to_string())
        );
        assert_eq!(
            entries.get("10.0.0.5:22"),
            Some(&"SHA256:def456".to_string())
        );
        assert!(!entries.contains_key("unknown:22"));
    }

    #[test]
    fn known_hosts_append_and_read() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "filar_test_known_hosts_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        append_known_hosts_entry(&path, "host1:22", "SHA256:aaa").unwrap();
        append_known_hosts_entry(&path, "host2:22", "SHA256:bbb").unwrap();

        let entries = parse_known_hosts(&path).unwrap();
        assert_eq!(
            entries.get("host1:22"),
            Some(&"SHA256:aaa".to_string())
        );
        assert_eq!(
            entries.get("host2:22"),
            Some(&"SHA256:bbb".to_string())
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn host_key_check_match() {
        let mut entries = HashMap::new();
        entries.insert("127.0.0.1:2222".to_string(), "SHA256:abc".to_string());
        assert_eq!(
            check_host_key(&entries, "127.0.0.1:2222", "SHA256:abc"),
            HostKeyCheck::Match
        );
    }

    #[test]
    fn host_key_check_mismatch() {
        let mut entries = HashMap::new();
        entries.insert("127.0.0.1:2222".to_string(), "SHA256:abc".to_string());
        assert_eq!(
            check_host_key(&entries, "127.0.0.1:2222", "SHA256:xyz"),
            HostKeyCheck::Mismatch
        );
    }

    #[test]
    fn host_key_check_new() {
        let entries: HashMap<String, String> = HashMap::new();
        assert_eq!(
            check_host_key(&entries, "127.0.0.1:2222", "SHA256:abc"),
            HostKeyCheck::New
        );
    }

    // ── transport config ─────────────────────────────────────────────

    #[test]
    fn transport_config_defaults() {
        let cfg = SshTransportConfig::default();
        assert_eq!(cfg.keepalive_interval, Some(Duration::from_secs(20)));
        assert_eq!(cfg.keepalive_max, 3);
        assert!(cfg.auto_reconnect);
    }

    #[test]
    fn connection_lost_is_classified() {
        // Pre-dispatch failures use the typed variant so the executor knows a
        // silent retry is safe; `is_connection_lost` recognises it.
        let e = CoreError::ConnectionLost("ssh channel task closed".into());
        assert!(crate::is_connection_lost(&e));
        assert!(matches!(e, CoreError::ConnectionLost(_)));
    }

    // ── reconnect integration tests (require Docker sshd) ────────────
    //
    // These drive `SshExecutor` against a real container and toggle it with
    // `docker stop`/`docker start`. The container name defaults to
    // `filar-sshd` and can be overridden with `FILAR_TEST_SSHD_CONTAINER`.

    #[cfg(test)]
    fn test_target() -> SshTarget {
        SshTarget {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "testuser".into(),
            auth: SshAuth::Password { password: None },
            host_key_policy: HostKeyPolicy::Tofu,
        }
    }

    #[cfg(test)]
    fn docker(args: &[&str]) {
        let container = std::env::var("FILAR_TEST_SSHD_CONTAINER")
            .unwrap_or_else(|_| "filar-sshd".to_string());
        let mut full = Vec::with_capacity(args.len() + 1);
        full.extend_from_slice(args);
        full.push(container.as_str());
        let status = std::process::Command::new("docker")
            .args(&full)
            .status()
            .expect("failed to invoke docker");
        assert!(status.success(), "docker {args:?} failed");
    }

    /// Killing the container between commands yields a clear error on the next
    /// command; restarting it and issuing another command reconnects silently
    /// and succeeds. Verifies the `reconnected` notice is emitted (visible in
    /// logs / chat via the WARN→System mirror — checked manually).
    #[tokio::test]
    #[ignore = "requires Docker sshd container on port 2222"]
    async fn ssh_reconnect_after_container_restart() {
        std::env::var("SSH_PASSWORD")
            .expect("set SSH_PASSWORD to run ignored SSH integration tests");

        use crate::CommandExecutor;
        let exec = SshExecutor::connect(&test_target()).await.unwrap();

        // Baseline: works.
        let r = exec.run("echo alive").await.unwrap();
        assert_eq!(r.stdout.trim(), "alive");

        // Kill the container — the session dies.
        docker(&["stop"]);
        // Give russh a moment to notice the dropped socket so the next command
        // hits the pre-dispatch (ConnectionLost) path. Auto-reconnect will try
        // once, fail (container down), and surface a clear error.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let down = exec.run("echo nope").await;
        assert!(down.is_err(), "expected an error while container is down");

        // Bring it back and let sshd come up.
        docker(&["start"]);
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Next command should reconnect silently and succeed.
        let r = exec.run("echo back").await.unwrap();
        assert_eq!(r.stdout.trim(), "back");

        exec.close().await.unwrap();
    }

    /// A command that has already been dispatched must never be auto-retried.
    /// Killing the container *while* a long command runs surfaces an error
    /// rather than silently re-running it after reconnect.
    #[tokio::test]
    #[ignore = "requires Docker sshd container on port 2222"]
    async fn ssh_dispatched_command_not_retried() {
        std::env::var("SSH_PASSWORD")
            .expect("set SSH_PASSWORD to run ignored SSH integration tests");

        use crate::CommandExecutor;
        let exec = Arc::new(SshExecutor::connect(&test_target()).await.unwrap());

        let exec2 = exec.clone();
        let handle = tokio::spawn(async move {
            // Long-running command: it is dispatched before the container dies.
            exec2.run("sleep 30 && echo done").await
        });

        tokio::time::sleep(Duration::from_secs(2)).await;
        docker(&["restart"]);

        let result = handle.await.unwrap();
        // The dispatched command must error out (channel closed after dispatch),
        // NOT silently reconnect and re-run `sleep 30 && echo done`.
        assert!(
            result.is_err() || result.as_ref().unwrap().exit_code != Some(0),
            "dispatched command should not have completed successfully after reconnect: {result:?}"
        );

        exec.close().await.unwrap();
    }
}
