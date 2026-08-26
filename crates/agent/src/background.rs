//! Background job registry and lifecycle for long-running commands.
//!
//! Jobs are scoped to an agent session (tab). `start_background_job` detaches
//! the process so agent tool-call timeouts apply only to status polls, not
//! job lifetime.
//!
//! **Local:** child is spawned via `tokio::process` (Unix: `setsid`); stdout/stderr
//! are captured in an in-memory buffer — no log files on disk.
//!
//! **SSH (zero-install):** start uses `nohup sh -c … & echo $!` through the
//! executor; output is read via short `tail` polls against an ephemeral
//! `/tmp/filar-job-{session}-{id}.log` removed when the job finishes or is
//! cancelled.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use filar_core::{CoreError, Result};
use filar_transport::CommandExecutor;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Lifecycle state of a background job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    Done { exit_code: i32 },
    Failed { exit_code: i32 },
    Cancelled,
}

impl JobState {
    fn label(&self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Done { .. } => "done",
            JobState::Failed { .. } => "failed",
            JobState::Cancelled => "cancelled",
        }
    }
}

/// Metadata stored for each job in the session registry.
struct JobRecord {
    command: String,
    pid: Option<u32>,
    state: JobState,
    /// Remote jobs: last polled tail. Local jobs use `output_buf`.
    output: String,
    log_path: Option<String>,
    local_child: Option<tokio::process::Child>,
    output_buf: Option<Arc<Mutex<String>>>,
}

/// Per-session job table.
#[derive(Default)]
struct SessionJobs {
    next_num: u32,
    jobs: HashMap<String, JobRecord>,
}

fn registry() -> &'static Mutex<HashMap<String, SessionJobs>> {
    static REG: OnceLock<Mutex<HashMap<String, SessionJobs>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_session<F, T>(session_id: &str, f: F) -> Result<T>
where
    F: FnOnce(&mut SessionJobs) -> Result<T>,
{
    let mut guard = registry()
        .lock()
        .map_err(|_| CoreError::Other("background job registry poisoned".into()))?;
    let session = guard
        .entry(session_id.to_string())
        .or_insert_with(SessionJobs::default);
    f(session)
}

fn job_id_for(session: &mut SessionJobs) -> String {
    session.next_num += 1;
    format!("job-{}", session.next_num)
}

fn remote_log_path(session_id: &str, job_id: &str) -> String {
    format!("/tmp/filar-job-{session_id}-{job_id}.log")
}

/// Escape a string for embedding in a single-quoted POSIX `sh -c` argument.
fn sh_single_quote(value: &str) -> String {
    if value.chars().all(|c| {
        c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/'
    }) {
        value.to_string()
    } else {
        let escaped = value.replace('\'', "'\\''");
        format!("'{escaped}'")
    }
}

// ---------------------------------------------------------------------------
// Local spawn helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn detach_from_controlling_tty(cmd: &mut tokio::process::Command) {
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

#[cfg(not(unix))]
#[allow(dead_code)]
fn detach_from_controlling_tty(_cmd: &mut tokio::process::Command) {}

async fn spawn_local_job(command: &str) -> Result<(tokio::process::Child, Arc<Mutex<String>>)> {
    let output_buf = Arc::new(Mutex::new(String::new()));

    #[cfg(windows)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", command]);
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
    #[cfg(unix)]
    detach_from_controlling_tty(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| CoreError::Other(format!("failed to start background job: {e}")))?;

    let pid = child.id();
    debug!(?pid, "local background job spawned");

    if let Some(mut stdout) = child.stdout.take() {
        let buf = output_buf.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut chunk = [0u8; 4096];
            loop {
                match stdout.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&chunk[..n]);
                        if let Ok(mut guard) = buf.lock() {
                            guard.push_str(&text);
                        }
                    }
                }
            }
        });
    }

    if let Some(mut stderr) = child.stderr.take() {
        let buf = output_buf.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&chunk[..n]);
                        if let Ok(mut guard) = buf.lock() {
                            if !guard.is_empty() && !guard.ends_with("[stderr] ") {
                                // prefix stderr chunks once per read batch
                            }
                            guard.push_str(&text);
                        }
                    }
                }
            }
        });
    }

    Ok((child, output_buf))
}

fn refresh_local_job(record: &mut JobRecord) {
    if !matches!(record.state, JobState::Running) {
        return;
    }
    let Some(ref mut child) = record.local_child else {
        return;
    };
    match child.try_wait() {
        Ok(Some(status)) => {
            let code = status.code().unwrap_or(-1);
            record.state = if status.success() {
                JobState::Done { exit_code: code }
            } else {
                JobState::Failed { exit_code: code }
            };
            record.local_child = None;
            record.pid = None;
        }
        Ok(None) => {}
        Err(e) => {
            warn!(error = %e, "try_wait failed for background job");
        }
    }
}

fn local_output_snapshot(record: &JobRecord) -> String {
    if let Some(ref buf) = record.output_buf {
        if let Ok(guard) = buf.lock() {
            return guard.clone();
        }
    }
    record.output.clone()
}

// ---------------------------------------------------------------------------
// Remote (SSH) helpers
// ---------------------------------------------------------------------------

async fn start_remote_job(
    executor: &dyn CommandExecutor,
    session_id: &str,
    job_id: &str,
    command: &str,
) -> Result<u32> {
    let log = remote_log_path(session_id, job_id);
    let quoted = sh_single_quote(command);
    let start_cmd = format!("nohup sh -c {quoted} > {log} 2>&1 & echo $!");
    let result = executor.run(&start_cmd).await?;
    let pid_str = result.stdout.trim();
    let pid: u32 = pid_str
        .lines()
        .last()
        .unwrap_or(pid_str)
        .trim()
        .parse()
        .map_err(|_| {
            CoreError::Other(format!(
                "failed to parse background job PID from: {pid_str:?}"
            ))
        })?;
    Ok(pid)
}

async fn poll_remote_job(
    executor: &dyn CommandExecutor,
    pid: u32,
    log_path: &str,
    tail_lines: u32,
) -> Result<(JobState, String)> {
    let poll_cmd = format!(
        "if kill -0 {pid} 2>/dev/null; then echo __FILAR_STATUS__=running; else \
         wait {pid} 2>/dev/null; ec=$?; echo __FILAR_STATUS__=finished; echo __FILAR_EXIT__=$ec; fi; \
         echo __FILAR_OUTPUT__; tail -n {tail_lines} {log_path} 2>/dev/null || true"
    );
    let result = executor.run(&poll_cmd).await?;
    let mut state = JobState::Running;
    let mut output = String::new();
    let mut in_output = false;
    let mut saw_finished = false;

    for line in result.stdout.lines() {
        if line.starts_with("__FILAR_STATUS__=running") {
            state = JobState::Running;
        } else if line.starts_with("__FILAR_STATUS__=finished") {
            saw_finished = true;
        } else if let Some(code_str) = line.strip_prefix("__FILAR_EXIT__=") {
            if let Ok(code) = code_str.trim().parse::<i32>() {
                state = if code == 0 {
                    JobState::Done { exit_code: code }
                } else {
                    JobState::Failed { exit_code: code }
                };
            }
        } else if line == "__FILAR_OUTPUT__" {
            in_output = true;
        } else if in_output {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(line);
        }
    }

    if saw_finished && matches!(state, JobState::Running) {
        state = JobState::Failed { exit_code: -1 };
    }

    Ok((state, output))
}

async fn cancel_remote_job(
    executor: &dyn CommandExecutor,
    pid: u32,
    log_path: &str,
) -> Result<()> {
    let cmd = format!("kill {pid} 2>/dev/null; rm -f {log_path}");
    executor.run(&cmd).await?;
    Ok(())
}

async fn cleanup_remote_log(executor: &dyn CommandExecutor, log_path: &str) {
    let _ = executor.run(&format!("rm -f {log_path}")).await;
}

// ---------------------------------------------------------------------------
// Public API (tool handlers)
// ---------------------------------------------------------------------------

/// Start a background job for `session_id`. Returns formatted tool output.
pub async fn start_job(
    session_id: &str,
    command: &str,
    is_local: bool,
    executor: &dyn CommandExecutor,
) -> Result<String> {
    let job_id = with_session(session_id, |session| Ok(job_id_for(session)))?;

    let pid = if is_local {
        let (child, output_buf) = spawn_local_job(command).await?;
        let pid = child.id();
        with_session(session_id, |session| {
            session.jobs.insert(
                job_id.clone(),
                JobRecord {
                    command: command.to_string(),
                    pid,
                    state: JobState::Running,
                    output: String::new(),
                    log_path: None,
                    local_child: Some(child),
                    output_buf: Some(output_buf),
                },
            );
            Ok(())
        })?;
        pid
    } else {
        let log_path = remote_log_path(session_id, &job_id);
        let pid = start_remote_job(executor, session_id, &job_id, command).await?;
        with_session(session_id, |session| {
            session.jobs.insert(
                job_id.clone(),
                JobRecord {
                    command: command.to_string(),
                    pid: Some(pid),
                    state: JobState::Running,
                    output: String::new(),
                    log_path: Some(log_path),
                    local_child: None,
                    output_buf: None,
                },
            );
            Ok(())
        })?;
        Some(pid)
    };

    let pid_line = pid
        .map(|p| format!("\npid: {p}"))
        .unwrap_or_default();

    info!(session_id, job_id = %job_id, "background job started");
    Ok(format!(
        "Background job started.\njob_id: {job_id}{pid_line}\nstatus: running\n\
         Poll with background_job_status; cancel with cancel_background_job."
    ))
}

/// Poll job status (subject to executor timeout, not job lifetime).
pub async fn job_status(
    session_id: &str,
    job_id: &str,
    tail_lines: u32,
    is_local: bool,
    executor: &dyn CommandExecutor,
) -> Result<String> {
    if is_local {
        let snapshot = with_session(session_id, |session| {
            let record = session
                .jobs
                .get_mut(job_id)
                .ok_or_else(|| CoreError::Other(format!("unknown job_id: {job_id}")))?;
            refresh_local_job(record);
            Ok((
                record.command.clone(),
                record.pid,
                record.state.clone(),
                local_output_snapshot(record),
            ))
        })?;
        let (command, pid, state, output) = snapshot;
        return Ok(format_status_response(job_id, &command, pid, &state, &output));
    }

    let (command, pid, state, log_path, cached_output) = with_session(session_id, |session| {
        let record = session
            .jobs
            .get(job_id)
            .ok_or_else(|| CoreError::Other(format!("unknown job_id: {job_id}")))?;
        Ok((
            record.command.clone(),
            record.pid,
            record.state.clone(),
            record.log_path.clone(),
            record.output.clone(),
        ))
    })?;

    if !matches!(state, JobState::Running) {
        return Ok(format_status_response(
            job_id,
            &command,
            pid,
            &state,
            &cached_output,
        ));
    }

    let Some(log) = log_path else {
        return Err(CoreError::Other(format!("job {job_id} has no remote log path")));
    };
    let Some(pid) = pid else {
        return Err(CoreError::Other(format!("job {job_id} has no pid")));
    };

    let (new_state, output) = poll_remote_job(executor, pid, &log, tail_lines).await?;

    with_session(session_id, |session| {
        if let Some(record) = session.jobs.get_mut(job_id) {
            record.state = new_state.clone();
            record.output = output.clone();
            if !matches!(record.state, JobState::Running) {
                record.pid = None;
            }
        }
        Ok(())
    })?;

    if !matches!(new_state, JobState::Running) {
        cleanup_remote_log(executor, &log).await;
    }

    Ok(format_status_response(
        job_id,
        &command,
        Some(pid),
        &new_state,
        &output,
    ))
}

fn format_status_response(
    job_id: &str,
    command: &str,
    pid: Option<u32>,
    state: &JobState,
    output: &str,
) -> String {
    let mut lines = vec![
        format!("job_id: {job_id}"),
        format!("status: {}", state.label()),
    ];
    if let Some(p) = pid {
        lines.push(format!("pid: {p}"));
    }
    lines.push(format!("command: {command}"));
    match state {
        JobState::Done { exit_code } => lines.push(format!("exit_code: {exit_code}")),
        JobState::Failed { exit_code } => lines.push(format!("exit_code: {exit_code}")),
        _ => {}
    }
    if output.trim().is_empty() {
        lines.push("(no output yet)".to_string());
    } else {
        lines.push("--- output (tail) ---".to_string());
        lines.push(output.to_string());
    }
    lines.join("\n")
}

/// Cancel a running background job.
pub async fn cancel_job(
    session_id: &str,
    job_id: &str,
    is_local: bool,
    executor: &dyn CommandExecutor,
) -> Result<String> {
    let (pid, log_path, state) = with_session(session_id, |session| {
        let record = session
            .jobs
            .get(job_id)
            .ok_or_else(|| CoreError::Other(format!("unknown job_id: {job_id}")))?;
        Ok((
            record.pid,
            record.log_path.clone(),
            record.state.clone(),
        ))
    })?;

    if !matches!(state, JobState::Running) {
        return Ok(format!(
            "job_id: {job_id}\nstatus: {}\n(already finished — nothing to cancel)",
            state.label()
        ));
    }

    if is_local {
        with_session(session_id, |session| {
            if let Some(record) = session.jobs.get_mut(job_id) {
                if let Some(ref mut child) = record.local_child {
                    let _ = child.start_kill();
                }
                record.state = JobState::Cancelled;
                record.local_child = None;
                record.pid = None;
            }
            Ok(())
        })?;
    } else {
        let Some(pid) = pid else {
            return Err(CoreError::Other(format!("job {job_id} has no pid")));
        };
        if let Some(ref log) = log_path {
            cancel_remote_job(executor, pid, log).await?;
        } else {
            executor.run(&format!("kill {pid} 2>/dev/null")).await?;
        }
        with_session(session_id, |session| {
            if let Some(record) = session.jobs.get_mut(job_id) {
                record.state = JobState::Cancelled;
                record.pid = None;
            }
            Ok(())
        })?;
    }

    info!(session_id, job_id, "background job cancelled");
    Ok(format!("job_id: {job_id}\nstatus: cancelled"))
}

/// List active (and recent) jobs for the session.
pub fn list_jobs(session_id: &str) -> Result<String> {
    with_session(session_id, |session| {
        if session.jobs.is_empty() {
            return Ok("No background jobs for this session.".to_string());
        }
        let mut lines = vec!["Background jobs:".to_string()];
        for (id, job) in &session.jobs {
            let pid = job
                .pid
                .map(|p| format!(" pid={p}"))
                .unwrap_or_default();
            lines.push(format!("- {id}: {}{}", job.state.label(), pid));
        }
        Ok(lines.join("\n"))
    })
}

/// Human-readable command string for confirm dialog / events.
pub fn confirm_command_for_start(command: &str) -> String {
    format!("start_background_job: {command}")
}

pub fn confirm_command_for_cancel(job_id: &str, pid: Option<u32>) -> String {
    match pid {
        Some(p) => format!("cancel_background_job {job_id} (kill {p})"),
        None => format!("cancel_background_job {job_id}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use filar_transport::CommandResult;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    struct MockExecutor {
        responses: StdMutex<Vec<CommandResult>>,
        commands: StdMutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl CommandExecutor for MockExecutor {
        async fn run(&self, command: &str) -> Result<CommandResult> {
            self.commands.lock().unwrap().push(command.to_string());
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(CommandResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    duration: Duration::from_millis(1),
                    cwd: None,
                })
            } else {
                Ok(responses.remove(0))
            }
        }

        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_SESSION_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_session() -> String {
        let n = TEST_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("test-{}-{n}", std::process::id())
    }

    #[tokio::test]
    async fn unknown_job_id_status_errors() {
        let exec = MockExecutor {
            responses: StdMutex::new(vec![]),
            commands: StdMutex::new(vec![]),
        };
        let sid = test_session();
        let err = job_status(&sid, "job-999", 20, false, &exec)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown job_id"));
    }

    #[tokio::test]
    async fn unknown_job_id_cancel_errors() {
        let exec = MockExecutor {
            responses: StdMutex::new(vec![]),
            commands: StdMutex::new(vec![]),
        };
        let sid = test_session();
        let err = cancel_job(&sid, "job-999", false, &exec)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown job_id"));
    }

    #[tokio::test]
    async fn remote_start_status_cancel_lifecycle() {
        let sid = test_session();
        let exec = MockExecutor {
            responses: StdMutex::new(vec![
                CommandResult {
                    stdout: "12345\n".into(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    duration: Duration::from_millis(1),
                    cwd: None,
                },
                CommandResult {
                    stdout: "__FILAR_STATUS__=running\n__FILAR_OUTPUT__\nline1\n".into(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    duration: Duration::from_millis(1),
                    cwd: None,
                },
                CommandResult {
                    stdout: "__FILAR_STATUS__=finished\n__FILAR_EXIT__=0\n__FILAR_OUTPUT__\ndone\n"
                        .into(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    duration: Duration::from_millis(1),
                    cwd: None,
                },
                CommandResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    duration: Duration::from_millis(1),
                    cwd: None,
                },
            ]),
            commands: StdMutex::new(vec![]),
        };

        let start_out = start_job(&sid, "sleep 10", false, &exec)
            .await
            .unwrap();
        assert!(start_out.contains("job_id: job-"));
        assert!(start_out.contains("pid: 12345"));
        assert!(start_out.contains("running"));

        let status_running = job_status(&sid, "job-1", 20, false, &exec)
            .await
            .unwrap();
        assert!(status_running.contains("status: running"));
        assert!(status_running.contains("line1"));

        let status_done = job_status(&sid, "job-1", 20, false, &exec)
            .await
            .unwrap();
        assert!(status_done.contains("status: done"));
        assert!(status_done.contains("done"));

        let cmds = exec.commands.lock().unwrap();
        assert!(cmds[0].contains("nohup sh -c"));
        assert!(cmds.iter().any(|c| c.contains("rm -f")));
    }

    #[test]
    fn list_empty_session() {
        let sid = test_session();
        let out = list_jobs(&sid).unwrap();
        assert!(out.contains("No background jobs"));
    }

    #[tokio::test]
    async fn local_start_and_cancel() {
        let sid = test_session();
        let exec = MockExecutor {
            responses: StdMutex::new(vec![]),
            commands: StdMutex::new(vec![]),
        };

        #[cfg(unix)]
        let cmd = "sleep 60";
        #[cfg(windows)]
        let cmd = "Start-Sleep -Seconds 60";

        let start_out = start_job(&sid, cmd, true, &exec).await.unwrap();
        assert!(start_out.contains("job_id: job-1"));
        assert!(start_out.contains("running"));

        let cancel_out = cancel_job(&sid, "job-1", true, &exec).await.unwrap();
        assert!(cancel_out.contains("cancelled"));

        let list = list_jobs(&sid).unwrap();
        assert!(list.contains("job-1"));
        assert!(list.contains("cancelled"));
    }
}
