//! Interactive terminal transport: raw bidirectional terminal access.
//!
//! Unlike [`crate::CommandExecutor`] (which uses a marker-based protocol for
//! structured command execution), the [`InteractiveTerminal`] trait provides
//! **raw** byte-stream access to a PTY or SSH channel. This is used by the
//! interactive terminal mode (Stage 7) where the user gets a full terminal
//! emulator backed by `alacritty_terminal`.
//!
//! Implementations:
//! - [`LocalInteractive`] — spawns a shell in a local PTY via `portable-pty`.
//! - [`SshInteractive`] — connects via SSH, requests a PTY + shell, and
//!   provides raw read/write/resize over the channel.

#[cfg(feature = "local")]
use std::io::{Read, Write};
use std::sync::Arc;

#[cfg(feature = "local")]
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use russh::client::{self, Handle, Msg};
use russh::keys::*;
use russh::{ChannelMsg, ChannelWriteHalf, Disconnect};
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use filar_core::{CoreError, Result, SshAuth, SshTarget};

use crate::ssh::{dirs_or_default, known_hosts_path, SshHandler};

// ---------------------------------------------------------------------------
// InteractiveTerminal trait
// ---------------------------------------------------------------------------

/// Trait abstracting raw interactive terminal access (local PTY or SSH).
///
/// Unlike [`CommandExecutor`](crate::CommandExecutor), this trait provides
/// raw bidirectional byte-stream access suitable for driving a full terminal
/// emulator. Output bytes are read and fed into a terminal model (e.g.
/// `alacritty_terminal::Term`); input bytes are forwarded from keyboard events.
#[async_trait::async_trait]
pub trait InteractiveTerminal: Send + Sync {
    /// Read a chunk of output bytes from the terminal.
    ///
    /// Returns `Ok(Some(bytes))` when data is available, `Ok(None)` on EOF
    /// (the terminal/PTY has closed).
    async fn read_output(&self) -> Result<Option<Vec<u8>>>;

    /// Write input bytes to the terminal (keyboard input forwarded to PTY/SSH).
    async fn write_input(&self, data: &[u8]) -> Result<()>;

    /// Resize the terminal to the given number of columns and rows.
    async fn resize(&self, cols: u16, rows: u16) -> Result<()>;

    /// Close the terminal session.
    async fn close(&self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// LocalInteractive — local PTY (feature-gated: requires `local`)
// ---------------------------------------------------------------------------

/// [`InteractiveTerminal`] backed by a local PTY via `portable-pty`.
///
/// Spawns a shell (`sh` on Unix, `cmd.exe` on Windows) in a pseudo-terminal
/// and provides raw read/write/resize access.
#[cfg(feature = "local")]
pub struct LocalInteractive {
    /// Receiver for output bytes (fed by a reader thread).
    rx: Arc<Mutex<mpsc::UnboundedReceiver<Vec<u8>>>>,
    /// Writer to the PTY master (for sending input).
    writer: Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    /// PTY master (for resize).
    master: Arc<std::sync::Mutex<Box<dyn MasterPty + Send>>>,
    /// Child process handle (kept alive).
    #[allow(dead_code)]
    child: Arc<std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
}

#[cfg(feature = "local")]
impl LocalInteractive {
    /// Create a new local interactive terminal with default shell and size.
    pub async fn new() -> Result<Self> {
        Self::with_size(80, 24).await
    }

    /// Create a local interactive terminal with the given initial size.
    pub async fn with_size(cols: u16, rows: u16) -> Result<Self> {
        Self::with_shell_and_size(None, cols, rows).await
    }

    /// Create a local interactive terminal with a specific shell and size.
    ///
    /// If `shell` is `None`, defaults to `sh` on Unix, `cmd.exe` on Windows.
    pub async fn with_shell_and_size(
        shell: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| CoreError::Other(format!("failed to open PTY: {e}")))?;

        // Determine shell program.
        #[cfg(unix)]
        let shell_prog = shell.unwrap_or("sh");
        #[cfg(windows)]
        let shell_prog = shell.unwrap_or("cmd.exe");

        let mut cmd = CommandBuilder::new(shell_prog);
        cmd.cwd(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));

        // Spawn the shell.
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| CoreError::Other(format!("failed to spawn shell: {e}")))?;

        // Drop the slave so that EOF is properly detected when the child exits.
        drop(pair.slave);

        // Take the writer and reader from the master.
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| CoreError::Other(format!("failed to take PTY writer: {e}")))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| CoreError::Other(format!("failed to take PTY reader: {e}")))?;

        // Spawn a blocking reader thread that forwards chunks to a channel.
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        info!(cols, rows, "local interactive PTY shell ready");

        Ok(Self {
            rx: Arc::new(Mutex::new(rx)),
            writer: Arc::new(std::sync::Mutex::new(writer)),
            master: Arc::new(std::sync::Mutex::new(pair.master)),
            child: Arc::new(std::sync::Mutex::new(child)),
        })
    }
}

#[async_trait::async_trait]
#[cfg(feature = "local")]
impl InteractiveTerminal for LocalInteractive {
    async fn read_output(&self) -> Result<Option<Vec<u8>>> {
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some(bytes) => Ok(Some(bytes)),
            None => Ok(None), // channel closed = EOF
        }
    }

    async fn write_input(&self, data: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer
            .write_all(data)
            .map_err(|e| CoreError::Other(format!("failed to write to PTY: {e}")))?;
        let _ = writer.flush();
        Ok(())
    }

    async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let master = self.master.lock().unwrap();
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| CoreError::Other(format!("failed to resize PTY: {e}")))?;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        // Closing is handled by dropping the handles.
        // Kill the child process to ensure cleanup.
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SshInteractive — SSH with PTY
// ---------------------------------------------------------------------------

/// [`InteractiveTerminal`] backed by an SSH session with a PTY.
///
/// Connects via `russh`, requests a PTY and shell on the remote host, and
/// provides raw read/write/resize access over the SSH channel.
pub struct SshInteractive {
    /// Receiver for output bytes (fed by a background read task).
    rx: Arc<Mutex<mpsc::UnboundedReceiver<Vec<u8>>>>,
    /// Write half of the SSH channel (for input and resize).
    write_half: Arc<ChannelWriteHalf<Msg>>,
    /// SSH session handle (kept alive to maintain the connection).
    #[allow(dead_code)]
    session: Handle<SshHandler>,
}

impl SshInteractive {
    /// Connect to an SSH target, request a PTY + shell, and return an
    /// interactive terminal.
    pub async fn connect(target: &SshTarget, cols: u16, rows: u16) -> Result<Self> {
        Self::connect_with_term(target, cols, rows, "xterm-256color").await
    }

    /// Connect with a specific terminal type string.
    pub async fn connect_with_term(
        target: &SshTarget,
        cols: u16,
        rows: u16,
        term: &str,
    ) -> Result<Self> {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(300)),
            ..<_>::default()
        });

        let addr = (target.host.as_str(), target.port);
        info!(host = %target.host, port = target.port, user = %target.user, "connecting interactive SSH");

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
        authenticate(&mut session, target).await?;

        // ── Open channel and request PTY + shell ───────────────────────
        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| CoreError::Other(format!("failed to open channel: {e}")))?;

        // Request a PTY so that full-screen apps (vim, htop) work.
        channel
            .request_pty(true, term, cols as u32, rows as u32, 0, 0, &[])
            .await
            .map_err(|e| CoreError::Other(format!("failed to request PTY: {e}")))?;

        channel
            .request_shell(true)
            .await
            .map_err(|e| CoreError::Other(format!("failed to request shell: {e}")))?;

        // Split the channel into read and write halves.
        let (read_half, write_half) = channel.split();

        // Spawn a background task to read output and forward via channel.
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            let mut read_half = read_half;
            loop {
                match read_half.wait().await {
                    Some(ChannelMsg::Data { ref data }) => {
                        if tx.send(data.as_ref().to_vec()).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                        // stderr — also forward to the terminal model.
                        if tx.send(data.as_ref().to_vec()).is_err() {
                            break;
                        }
                    }
                    Some(_) => {} // ignore other channel messages
                    None => break, // channel closed
                }
            }
        });

        info!(cols, rows, %term, "SSH interactive PTY shell ready");

        Ok(Self {
            rx: Arc::new(Mutex::new(rx)),
            write_half: Arc::new(write_half),
            session,
        })
    }
}

#[async_trait::async_trait]
impl InteractiveTerminal for SshInteractive {
    async fn read_output(&self) -> Result<Option<Vec<u8>>> {
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some(bytes) => Ok(Some(bytes)),
            None => Ok(None), // channel closed = EOF
        }
    }

    async fn write_input(&self, data: &[u8]) -> Result<()> {
        self.write_half
            .data(data)
            .await
            .map_err(|e| CoreError::Other(format!("failed to write to SSH channel: {e}")))?;
        Ok(())
    }

    async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.write_half
            .window_change(cols as u32, rows as u32, 0, 0)
            .await
            .map_err(|e| CoreError::Other(format!("failed to resize SSH PTY: {e}")))?;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let _ = self
            .session
            .disconnect(Disconnect::ByApplication, "", "English")
            .await;
        info!("SSH interactive session closed");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Shared authentication helper
// ---------------------------------------------------------------------------

/// Authenticate an SSH session using the target's configured auth method.
async fn authenticate(session: &mut Handle<SshHandler>, target: &SshTarget) -> Result<()> {
    match &target.auth {
        SshAuth::Key { path } => {
            let key_path = path.clone().unwrap_or_else(dirs_or_default);
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
                .ok_or_else(|| {
                    CoreError::Secret("no password provided (set SSH_PASSWORD env var or enter in GUI)".into())
                })?;

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
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[cfg(all(unix, feature = "local"))]
    use super::*;

    #[cfg(all(unix, feature = "local"))]
    #[tokio::test]
    #[ignore = "requires sh on Unix"]
    async fn local_interactive_echo() {
        let term = LocalInteractive::with_size(80, 24).await.unwrap();

        // Send "echo hello\n" and read output.
        term.write_input(b"echo hello\n").await.unwrap();

        // Read output until we see "hello".
        let mut output = Vec::new();
        for _ in 0..20 {
            if let Some(chunk) = term.read_output().await.unwrap() {
                output.extend_from_slice(&chunk);
                if output.windows(5).any(|w| w == b"hello") {
                    break;
                }
            } else {
                break;
            }
        }
        assert!(output.windows(5).any(|w| w == b"hello"));

        term.close().await.unwrap();
    }
}
