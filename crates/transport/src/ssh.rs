//! SSH transport: persistent shell channel over `russh`.
//!
//! This module implements the core "killer feature" — a single persistent
//! SSH shell channel through which commands are executed with marker-based
//! boundary detection and exit-code extraction. **Zero files are created
//! on the remote machine.**

use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client::{self, Handle, Msg};
use russh::keys::*;
use russh::{Channel, ChannelMsg, Disconnect};
use tokio::sync::Mutex;
use tracing::{info, warn};
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
// SshSession — persistent shell channel
// ---------------------------------------------------------------------------

/// Inner state protected by a mutex.
struct SshSessionInner {
    /// Handle to the SSH session (used for disconnect).
    #[allow(dead_code)]
    session: Handle<SshHandler>,
    /// The persistent shell channel.
    channel: Channel<Msg>,
    /// Monotonic counter for request IDs.
    req_counter: u64,
}

/// An SSH session maintaining a single persistent shell channel.
///
/// Commands are executed via [`SshSession::run`], which sends the command
/// followed by a marker `printf` and reads output until the marker is found.
/// Shell state (cwd, env) is preserved between commands because the channel
/// is never closed.
pub struct SshSession {
    inner: Mutex<SshSessionInner>,
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
        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| CoreError::Other(format!("failed to open channel: {e}")))?;

        // Request a shell (no PTY — we want clean output without echo).
        channel
            .request_shell(true)
            .await
            .map_err(|e| CoreError::Other(format!("failed to request shell: {e}")))?;

        let mut inner = SshSessionInner {
            session,
            channel,
            req_counter: 0,
        };

        // ── Sync: drain initial shell output ───────────────────────────
        // Send a unique sync marker and wait for it to appear.
        let sync_id = format!("sync_{}", Uuid::new_v4().simple());
        let sync_cmd = format!(
            "printf '\\n{}{}\\n'\n",
            MARKER_PREFIX, sync_id
        );
        inner
            .channel
            .data(sync_cmd.as_bytes())
            .await
            .map_err(|e| CoreError::Other(format!("failed to send sync marker: {e}")))?;

        // Read until we find the sync marker.
        let sync_marker = format!("{}{}", MARKER_PREFIX, sync_id);
        drain_until_marker(&mut inner.channel, &sync_marker, Duration::from_secs(10))
            .await?;

        info!("SSH shell channel ready and synchronised");

        Ok(Self {
            inner: Mutex::new(inner),
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
        let mut inner = self.inner.lock().await;
        let req_id = inner.req_counter;
        inner.req_counter += 1;
    
        let marker_id = format!("req_{:08x}", req_id);
        let marker_tag = format!("{}{}", MARKER_PREFIX, marker_id);
    
        // Build the full payload: <command>\n printf marker\n
        let payload = format!(
            "{}\nprintf '\\n{}__%d__\\n' \"$?\"\n",
            command, marker_tag
        );
    
        let start = Instant::now();
        inner
            .channel
            .data(payload.as_bytes())
            .await
            .map_err(|e| CoreError::Other(format!("failed to send command: {e}")))?;
    
        // Read until we find the marker.
        match drain_until_marker_with_exit(
            &mut inner.channel,
            &marker_tag,
            Duration::from_secs(120),
        )
        .await
        {
            Ok((output, exit_code)) => {
                let duration = start.elapsed();
                Ok(CommandResult {
                    stdout: output,
                    stderr: String::new(),
                    exit_code,
                    duration,
                })
            }
            Err(e) => {
                // Command timed out or failed. Send Ctrl-C to interrupt
                // any running process, then resync the shell.
                warn!(error = %e, "command timed out, sending Ctrl-C and resyncing");
                let ctrl_c: &[u8] = b"\x03";
                let _ = inner.channel.data(ctrl_c).await; // Ctrl-C
                // Drain any pending output and resync.
                let sync_id = format!("sync_{}", Uuid::new_v4().simple());
                let sync_cmd = format!(
                    "printf '\\n{}{}\\n'\n",
                    MARKER_PREFIX, sync_id
                );
                let _ = inner.channel.data(sync_cmd.as_bytes()).await;
                let sync_marker = format!("{}{}", MARKER_PREFIX, sync_id);
                let _ = drain_until_marker(
                    &mut inner.channel,
                    &sync_marker,
                    Duration::from_secs(10),
                )
                .await;
                Err(e)
            }
        }
    }

    /// Disconnect the SSH session.
    pub async fn close(&self) -> Result<()> {
        let inner = self.inner.lock().await;
        let _ = inner
            .session
            .disconnect(Disconnect::ByApplication, "", "English")
            .await;
        info!("SSH session closed");
        Ok(())
    }
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
        // Send Ctrl-C (0x03) to the SSH channel.
        let inner = self.session.inner.lock().await;
        inner
            .channel
            .data(&b"\x03"[..])
            .await
            .map_err(|e| CoreError::Other(format!("failed to send Ctrl-C: {e}")))?;
        Ok(())
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
            ChannelMsg::Eof | ChannelMsg::Close => {
                return Err(CoreError::Other(
                    "channel closed before marker received".into(),
                ));
            }
            _ => {}
        }
    }
}

/// Read from the channel until a marker of the form `<marker_tag>__<exit_code>__`
/// is found. Returns the output before the marker and the parsed exit code.
async fn drain_until_marker_with_exit(
    channel: &mut Channel<Msg>,
    marker_tag: &str,
    timeout: Duration,
) -> Result<(String, Option<i32>)> {
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let msg = tokio::time::timeout_at(deadline, channel.wait())
            .await
            .map_err(|_| CoreError::Other(format!("timeout waiting for command marker `{marker_tag}`")))?
            .ok_or_else(|| CoreError::Other("channel closed before marker received".into()))?;

        match msg {
            ChannelMsg::Data { ref data } => {
                buf.push_str(&String::from_utf8_lossy(data.as_ref()));

                // Look for the marker line: <marker_tag>__<digits>__
                if let Some(pos) = buf.find(marker_tag) {
                    // Found the marker tag — try to parse the exit code.
                    let after_tag = &buf[pos + marker_tag.len()..];
                    // Expected format: __<digits>__
                    if let Some(rest) = after_tag.strip_prefix("__") {
                        if let Some(end) = rest.find("__") {
                            let code_str = &rest[..end];
                            if let Ok(code) = code_str.trim().parse::<i32>() {
                                // Output is everything before the marker line.
                                // Find the start of the marker line (go back to the previous newline).
                                let line_start = buf[..pos].rfind('\n').map(|p| p + 1).unwrap_or(0);
                                let output = buf[..line_start].to_string();
                                return Ok((output, Some(code)));
                            }
                        }
                    }
                    // Marker tag found but couldn't parse exit code — keep reading.
                }
            }
            ChannelMsg::Eof | ChannelMsg::Close => {
                return Err(CoreError::Other(
                    "channel closed before command marker received".into(),
                ));
            }
            _ => {}
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
}
