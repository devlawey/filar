//! Local transport: command execution via subprocess.
//!
//! Uses `tokio::process::Command` to execute commands. On Windows, commands
//! are run via PowerShell (`-NoProfile -NonInteractive -Command`). On Unix,
//! commands are run via `sh -c`.
//!
//! Each command runs in a fresh process, so env and shell variables do NOT
//! persist between calls. The working directory DOES persist: after every
//! command the shell reports its final directory through a one-off marker
//! line on stdout (stripped from the output), and the next command starts
//! there (#493). [`CommandExecutor::set_cwd`] overrides it (interactive ↔
//! agent sync).
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
/// Working directory persists: a `cd` in one command applies to the next
/// (reported by the shell after each command, #493), and
/// [`CommandExecutor::set_cwd`] overrides it.
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
        let marker = cwd_marker();

        // Build the command based on platform.
        #[cfg(windows)]
        let mut cmd = {
            let full = build_shell_command(command, &marker);
            let mut c = tokio::process::Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", &full]);
            c
        };
        #[cfg(unix)]
        let mut cmd = {
            let full = build_shell_command(command, &marker);
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", &full]);
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

        let child = cmd
            .spawn()
            .map_err(|e| CoreError::Other(format!("command failed: {e}")))?;
        #[cfg(unix)]
        let pid = child.id();

        // Wait for output, with timeout and cancel support.
        // When cancel/timeout fires, the wait future is dropped, which kills
        // the shell (kill_on_drop = true); on Unix the rest of its process
        // group goes too — the shell no longer `exec`s a lone command, so
        // the command itself is a child of the shell (#493).
        let output = tokio::select! {
            result = child.wait_with_output() => {
                result.map_err(|e| CoreError::Other(format!("command failed: {e}")))?
            }
            _ = self.cancel_notify.notified() => {
                #[cfg(unix)]
                kill_process_group(pid);
                return Err(CoreError::Other("command cancelled by user".into()));
            }
            _ = tokio::time::sleep(self.timeout) => {
                #[cfg(unix)]
                kill_process_group(pid);
                return Err(CoreError::Other(format!(
                    "command timed out after {} seconds",
                    self.timeout.as_secs()
                )));
            }
        };

        let duration = start.elapsed();

        let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let (stdout, reported_cwd) = split_cwd_marker(&raw_stdout, &marker);
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code();

        // A command that ends the shell early (`exit 3`, `exec …`) reports
        // nothing; the previous directory then stays in effect.
        if let Some(dir) = reported_cwd {
            if let Ok(mut guard) = self.cwd.lock() {
                *guard = Some(PathBuf::from(dir));
            }
        }

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

/// Unique per-command prefix of the line that reports the final directory.
///
/// Random, so no output a command prints by chance — or on purpose — can
/// pass for it.
fn cwd_marker() -> String {
    format!("__filar_cwd_{}__=", uuid::Uuid::new_v4().simple())
}

/// Build the shell command string for the current platform.
///
/// The user's command comes first; then the shell prints
/// `<marker><final directory>` on stdout and exits with the command's status
/// (#493). A newline ends the command, so a trailing `# comment` cannot
/// swallow the reporting lines.
///
/// On Windows, sets `[Console]::OutputEncoding` to UTF-8 so PowerShell writes
/// its own output (cmdlet output, error messages) as UTF-8 bytes. Unlike
/// `chcp 65001`, this does not change the console active code page
/// (`SetConsoleOutputCP`), which .NET caches at startup and ignores later —
/// and which could trigger font switch / resize events on the parent console
/// (#246). The command runs dot-sourced (its variables stay in scope) with
/// stderr merged into stdout and is piped to `Out-Default`: table output is
/// buffered by the formatter and an explicit `exit` would drop it, so it must
/// be written before the marker. `$?` of the command's last statement decides
/// the exit code — 0, else the native exit code, else 1. The reported
/// directory is the FileSystem location even after `cd HKLM:`, so the next
/// process can start in it.
fn build_shell_command(command: &str, marker: &str) -> String {
    #[cfg(windows)]
    {
        powershell_command(command, marker)
    }
    #[cfg(not(windows))]
    {
        format!(
            "{command}\n__filar_rc=$?\nprintf '%s%s\\n' '{marker}' \"$(pwd)\"\nexit $__filar_rc"
        )
    }
}

/// The PowerShell form of [`build_shell_command`]; built on every platform so
/// it can be tested against a real `pwsh` on Unix too.
#[cfg_attr(not(windows), allow(dead_code))]
fn powershell_command(command: &str, marker: &str) -> String {
    format!(
        "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new(); . {{ {command}\n\
         $global:__filar_ok = $?\n\
         }} 2>&1 | Out-Default\n\
         '{marker}' + (Get-Location -PSProvider FileSystem).ProviderPath\n\
         if ($global:__filar_ok) {{ exit 0 }} elseif ($LASTEXITCODE) {{ exit $LASTEXITCODE }} else {{ exit 1 }}"
    )
}

/// Split the marker line off `stdout`: the command's own output and the
/// directory the shell reported, if any (and if it is a usable cwd).
///
/// The last occurrence wins; output a command printed without a trailing
/// newline stays intact in front of the marker.
fn split_cwd_marker(stdout: &str, marker: &str) -> (String, Option<String>) {
    let Some(pos) = stdout.rfind(marker) else {
        return (stdout.to_string(), None);
    };
    let rest = &stdout[pos + marker.len()..];
    let (line, after) = match rest.find('\n') {
        Some(nl) => (&rest[..nl], &rest[nl + 1..]),
        None => (rest, ""),
    };
    let dir = line.trim_end_matches('\r');
    let cwd = crate::is_safe_cwd(dir).then(|| dir.to_string());
    let mut out = stdout[..pos].to_string();
    out.push_str(after);
    (out, cwd)
}

/// SIGKILL the process group led by `pid` (the shell, made a session leader
/// by [`detach_from_controlling_tty`]), so a cancelled command does not
/// outlive its shell.
#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) else {
        return;
    };
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    // SAFETY: kill(2) with a negative pid signals that process group; it has
    // no memory-safety preconditions. A group already gone yields ESRCH,
    // which is fine to ignore.
    unsafe {
        kill(-pid, SIGKILL);
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

    const M: &str = "__filar_cwd_test__=";

    #[test]
    fn build_shell_command_contains_user_command_and_marker() {
        let result = build_shell_command("echo hello", M);
        assert!(result.contains("echo hello\n"), "a newline must end the user command: {result}");
        assert!(result.contains(M), "must report cwd with the marker: {result}");
    }

    #[test]
    fn powershell_command_has_output_encoding_and_stderr_redirect() {
        let result = powershell_command("dir", M);
        assert!(
            result.starts_with("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new(); . { dir\n"),
            "Windows command must have the OutputEncoding prefix, got: {result}"
        );
        assert!(result.contains("} 2>&1 | Out-Default\n"), "stderr merged and flushed: {result}");
        assert!(result.contains("Get-Location -PSProvider FileSystem"));
    }

    #[test]
    #[cfg(not(windows))]
    fn build_shell_command_unix_no_prefix() {
        let result = build_shell_command("ls", M);
        assert!(result.starts_with("ls\n"), "got: {result}");
        assert!(!result.contains("OutputEncoding"));
    }

    #[test]
    fn split_cwd_marker_strips_the_line_and_returns_the_dir() {
        let (out, cwd) = split_cwd_marker(&format!("a\nb\n{M}/srv/app\n"), M);
        assert_eq!(out, "a\nb\n");
        assert_eq!(cwd.as_deref(), Some("/srv/app"));

        // Output without a trailing newline stays intact.
        let (out, cwd) = split_cwd_marker(&format!("no-newline{M}C:\\Users\\Мой Dir\r\n"), M);
        assert_eq!(out, "no-newline");
        assert_eq!(cwd.as_deref(), Some("C:\\Users\\Мой Dir"));
    }

    #[test]
    fn split_cwd_marker_without_marker_or_with_empty_dir() {
        let (out, cwd) = split_cwd_marker("plain\n", M);
        assert_eq!(out, "plain\n");
        assert!(cwd.is_none());
        let (out, cwd) = split_cwd_marker(&format!("x\n{M}\n"), M);
        assert_eq!(out, "x\n");
        assert!(cwd.is_none(), "a failed pwd must not become the cwd");
    }

    /// `cd` in one command applies to the next one (#493).
    #[tokio::test]
    async fn cd_persists_between_runs() {
        let exec = LocalExecutor::new().await.unwrap();
        let marker = format!("filar_cd_{}", std::process::id());
        let dir = std::env::temp_dir().join(&marker);
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.to_string_lossy().into_owned();
        #[cfg(windows)]
        let (cd, pwd) = (format!("cd '{dir_str}'"), "(Get-Location).Path");
        #[cfg(unix)]
        let (cd, pwd) = (format!("cd '{dir_str}'"), "pwd");
        let first = exec.run(&cd).await.unwrap();
        assert!(!first.stdout.contains("__filar_cwd_"), "marker leaked: {:?}", first.stdout);
        assert!(first.cwd.as_deref().is_some_and(|c| c.contains(&marker)), "{:?}", first.cwd);
        assert!(exec.current_cwd().await.is_some_and(|c| c.contains(&marker)));
        let second = exec.run(pwd).await.unwrap();
        assert!(
            second.stdout.contains(&marker),
            "pwd after cd: {:?} should contain {marker}",
            second.stdout
        );
        assert!(!second.stdout.contains("__filar_cwd_"), "marker leaked: {:?}", second.stdout);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wrapper keeps the command's own exit status.
    #[tokio::test]
    async fn exit_code_is_preserved() {
        let exec = LocalExecutor::new().await.unwrap();
        #[cfg(unix)]
        {
            assert_eq!(exec.run("false").await.unwrap().exit_code, Some(1));
            assert_eq!(exec.run("sh -c 'exit 7'").await.unwrap().exit_code, Some(7));
            assert_eq!(exec.run("true").await.unwrap().exit_code, Some(0));
            // `exit` in the command itself skips the report; the cwd stays.
            let before = exec.current_cwd().await;
            assert_eq!(exec.run("cd /; exit 3").await.unwrap().exit_code, Some(3));
            assert_eq!(exec.current_cwd().await, before);
        }
        #[cfg(windows)]
        {
            assert_eq!(exec.run("cmd /c exit 7").await.unwrap().exit_code, Some(7));
            assert_eq!(exec.run("Write-Output ok").await.unwrap().exit_code, Some(0));
        }
    }

    /// A trailing comment in the command does not swallow the report.
    #[tokio::test]
    async fn trailing_comment_does_not_hide_the_cwd() {
        let exec = LocalExecutor::new().await.unwrap();
        let result = exec.run("echo hi # note").await.unwrap();
        assert!(result.cwd.is_some(), "cwd must be reported");
        assert!(!result.stdout.contains("__filar_cwd_"));
    }

    /// The PowerShell wrapper against a real `pwsh` (#493): `cd` is
    /// reported, table output survives the final `exit`, exit codes and a
    /// trailing comment behave.
    #[test]
    #[cfg(unix)]
    #[ignore = "requires PowerShell: set FILAR_TEST_PWSH to the pwsh binary"]
    fn powershell_command_against_real_pwsh() {
        let pwsh = std::env::var("FILAR_TEST_PWSH").expect("set FILAR_TEST_PWSH");
        let run = |command: &str| {
            let out = std::process::Command::new(&pwsh)
                .args(["-NoProfile", "-NonInteractive", "-Command"])
                .arg(powershell_command(command, M))
                .current_dir("/")
                .env("TERM", "xterm")
                .output()
                .unwrap();
            let (stdout, cwd) = split_cwd_marker(&String::from_utf8_lossy(&out.stdout), M);
            (stdout, cwd, out.status.code())
        };
        let (out, cwd, code) = run("Set-Location /usr; Get-Location");
        assert!(out.contains("/usr"), "table output lost: {out:?}");
        assert_eq!(cwd.as_deref(), Some("/usr"));
        assert_eq!(code, Some(0));
        assert_eq!(run("sh -c 'exit 7'").2, Some(7));
        assert_eq!(run("Write-Error boom").2, Some(1));
        assert_eq!(run("exit 4").2, Some(4));
        let (out, cwd, code) = run("'hi' # note }");
        assert_eq!((out.trim(), cwd.as_deref(), code), ("hi", Some("/"), Some(0)));
    }

    /// Cancel kills the command, not only the wrapping shell (#493).
    #[tokio::test]
    #[cfg(unix)]
    async fn cancel_kills_the_whole_process_group() {
        let exec = Arc::new(LocalExecutor::with_timeout(Duration::from_secs(20)).await.unwrap());
        let tag = format!("filar_cancel_{}", std::process::id());
        let run = {
            let exec = exec.clone();
            let cmd = format!("sleep 30 && echo {tag}");
            tokio::spawn(async move { exec.run(&cmd).await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        exec.cancel().await.unwrap();
        assert!(run.await.unwrap().is_err());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ps = std::process::Command::new("ps").args(["-eo", "args"]).output().unwrap();
        let ps = String::from_utf8_lossy(&ps.stdout);
        assert!(!ps.contains(&tag), "command survived cancel:\n{ps}");
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
