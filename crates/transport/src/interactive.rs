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
use russh::{ChannelMsg, ChannelWriteHalf, Disconnect};
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use filar_core::{CoreError, EnvSecretProvider, Result, SecretProvider, SshTarget};

use crate::ssh::{known_hosts_path, SshHandler};

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

    /// Whether the shell reports its working directory by itself, as OSC 7
    /// from every prompt (#482). Such a shell is not sent the POSIX `pwd`
    /// probe — cmd.exe and PowerShell could not run it anyway. Defaults to
    /// `false`: probe.
    fn reports_cwd_itself(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// LocalInteractive — local PTY (feature-gated: requires `local`)
// ---------------------------------------------------------------------------

/// [`InteractiveTerminal`] backed by a local PTY via `portable-pty`.
///
/// Spawns a shell in a pseudo-terminal and provides raw read/write/resize
/// access. Default shell: `$SHELL` on Unix/macOS (fallback `sh` if unset or
/// not a usable path); `cmd.exe` on Windows.
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
    /// The shell reports its cwd from its prompt (cmd.exe, PowerShell).
    reports_cwd: bool,
}

/// Resolve the default local interactive shell when none is passed explicitly.
///
/// On Unix: `$SHELL` if non-empty and the path is an existing file; otherwise
/// `"sh"`. On Windows: `"cmd.exe"`. Agent command execution (`LocalExecutor`)
/// still uses `sh -c` / PowerShell and is unchanged.
#[cfg(feature = "local")]
fn resolve_default_local_shell() -> String {
    #[cfg(unix)]
    {
        resolve_unix_interactive_shell(std::env::var("SHELL").ok().as_deref())
    }
    #[cfg(windows)]
    {
        "cmd.exe".to_string()
    }
}

/// Pick Unix interactive shell from an env-style value (for tests + default).
#[cfg(all(unix, feature = "local"))]
fn resolve_unix_interactive_shell(env_shell: Option<&str>) -> String {
    env_shell
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| std::path::Path::new(s).is_file())
        .unwrap_or("sh")
        .to_string()
}

#[cfg(feature = "local")]
impl LocalInteractive {
    /// Create a new local interactive terminal with default shell and size.
    pub async fn new() -> Result<Self> {
        Self::with_size(80, 24).await
    }

    /// Create a local interactive terminal with the given initial size.
    pub async fn with_size(cols: u16, rows: u16) -> Result<Self> {
        Self::with_shell_size_and_cwd(None, cols, rows, None).await
    }

    /// Create a local interactive PTY in `cwd` when set (tab cwd sync).
    pub async fn with_size_in(cols: u16, rows: u16, cwd: Option<&str>) -> Result<Self> {
        Self::with_shell_size_and_cwd(None, cols, rows, cwd).await
    }

    /// Create a local interactive terminal with a specific shell and size.
    ///
    /// If `shell` is `None`, uses [`resolve_default_local_shell`]: `$SHELL` on
    /// Unix/macOS (fallback `sh`), `cmd.exe` on Windows.
    pub async fn with_shell_and_size(
        shell: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        Self::with_shell_size_and_cwd(shell, cols, rows, None).await
    }

    /// Create a local interactive terminal with a specific shell, size, and cwd.
    pub async fn with_shell_size_and_cwd(
        shell: Option<&str>,
        cols: u16,
        rows: u16,
        cwd: Option<&str>,
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

        let default_shell = resolve_default_local_shell();
        let shell_prog = shell.unwrap_or(default_shell.as_str());

        let mut cmd = CommandBuilder::new(shell_prog);
        // Windows shells have no `pwd` probe: they report their directory
        // themselves, from the prompt, as OSC 7 (#482).
        let flavor = crate::shell_flavor(shell_prog);
        match flavor {
            crate::ShellFlavor::Cmd => {
                let existing = std::env::var("PROMPT").ok();
                cmd.env("PROMPT", crate::cmd_osc7_prompt(existing.as_deref()));
            }
            crate::ShellFlavor::PowerShell => {
                cmd.args(["-NoLogo", "-NoExit", "-Command", crate::POWERSHELL_OSC7_PROMPT]);
            }
            crate::ShellFlavor::Posix => {}
        }
        let dir = cwd
            .map(str::trim)
            .filter(|p| crate::is_safe_cwd(p))
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        cmd.cwd(dir);

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

        info!(cols, rows, shell = %shell_prog, "local interactive PTY shell ready");

        Ok(Self {
            rx: Arc::new(Mutex::new(rx)),
            writer: Arc::new(std::sync::Mutex::new(writer)),
            master: Arc::new(std::sync::Mutex::new(pair.master)),
            child: Arc::new(std::sync::Mutex::new(child)),
            reports_cwd: flavor != crate::ShellFlavor::Posix,
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

    fn reports_cwd_itself(&self) -> bool {
        self.reports_cwd
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
    ///
    /// Secrets are resolved through the default [`EnvSecretProvider`]
    /// (`SSH_PASSWORD` env var), preserving TUI/desktop behaviour.
    pub async fn connect(target: &SshTarget, cols: u16, rows: u16) -> Result<Self> {
        Self::connect_with_term(target, cols, rows, "xterm-256color").await
    }

    /// Connect with a specific terminal type string, resolving secrets through
    /// the default [`EnvSecretProvider`].
    pub async fn connect_with_term(
        target: &SshTarget,
        cols: u16,
        rows: u16,
        term: &str,
    ) -> Result<Self> {
        Self::connect_with_provider(target, cols, rows, term, &EnvSecretProvider::new()).await
    }

    /// Connect with a specific terminal type string and secret provider.
    ///
    /// External consumers that don't use environment variables can inject the
    /// SSH password via their own [`SecretProvider`] (`"SSH_PASSWORD"`) or set
    /// it explicitly on the target.
    pub async fn connect_with_provider(
        target: &SshTarget,
        cols: u16,
        rows: u16,
        term: &str,
        secrets: &dyn SecretProvider,
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
        crate::auth::authenticate(&mut session, target, secrets).await?;

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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[cfg(all(unix, feature = "local"))]
    use super::*;

    #[cfg(all(unix, feature = "local"))]
    #[test]
    fn unix_shell_prefers_valid_env_path() {
        // `/bin/sh` exists on essentially every Unix CI image.
        assert_eq!(
            resolve_unix_interactive_shell(Some("/bin/sh")),
            "/bin/sh"
        );
    }

    #[cfg(all(unix, feature = "local"))]
    #[test]
    fn unix_shell_falls_back_when_empty_or_missing() {
        assert_eq!(resolve_unix_interactive_shell(None), "sh");
        assert_eq!(resolve_unix_interactive_shell(Some("")), "sh");
        assert_eq!(resolve_unix_interactive_shell(Some("   ")), "sh");
        assert_eq!(
            resolve_unix_interactive_shell(Some("/no/such/filar-shell-xyz")),
            "sh"
        );
    }

    #[cfg(all(unix, feature = "local"))]
    #[tokio::test]
    #[ignore = "requires a local PTY and a usable shell"]
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

    /// Integration test (#482): a PowerShell PTY reports its directory as
    /// OSC 7 from the prompt — spaces and Cyrillic included — and again
    /// after a `Set-Location`, with nothing typed but that command.
    #[cfg(all(unix, feature = "local"))]
    #[tokio::test]
    #[ignore = "requires PowerShell: set FILAR_TEST_PWSH to the pwsh binary"]
    async fn powershell_reports_its_cwd_as_osc7() {
        let pwsh = std::env::var("FILAR_TEST_PWSH").expect("set FILAR_TEST_PWSH");
        let dir = std::env::temp_dir().join(format!("filar pwsh Проверка {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let osc7 = |p: &std::path::Path| {
            format!(
                "\x1b]7;file://filar-raw/{}\x1b\\",
                p.to_string_lossy().trim_start_matches('/')
            )
        };
        let in_dir = osc7(&dir);
        let in_parent = osc7(dir.parent().unwrap());

        let term = LocalInteractive::with_shell_size_and_cwd(
            Some(&pwsh),
            200,
            30,
            Some(&dir.to_string_lossy()),
        )
        .await
        .unwrap();
        // A stand-in for the terminal: PSReadLine asks for the cursor
        // position (DSR) and waits for the answer.
        let mut output = Vec::new();
        let mut typed = false;
        let seen = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while let Ok(Some(chunk)) = term.read_output().await {
                let dsr = chunk.windows(4).filter(|w| w == b"\x1b[6n").count();
                for _ in 0..dsr {
                    term.write_input(b"\x1b[1;1R").await.unwrap();
                }
                output.extend_from_slice(&chunk);
                let text = String::from_utf8_lossy(&output).into_owned();
                if !typed && text.contains(&in_dir) {
                    typed = true;
                    term.write_input(b"Set-Location ..\r").await.unwrap();
                }
                if typed && text.contains(&in_parent) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        let _ = term.close().await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen, "OSC 7 missing: {:?}", String::from_utf8_lossy(&output));
    }
}
