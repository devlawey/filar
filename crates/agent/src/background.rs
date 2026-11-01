//! Background job registry and lifecycle for long-running commands.
//!
//! Jobs are scoped to an agent session (tab). `start_background_job` detaches
//! the process so agent tool-call timeouts apply only to status polls, not
//! job lifetime.
//!
//! **Local:** child is spawned via `tokio::process` (Unix: `setsid`); stdout/stderr
//! are captured in an in-memory buffer — no log files on disk.
//!
//! **SSH / pod (zero-install):** start uses `nohup sh -c … & echo $!` through
//! the executor; output is read via short `tail` polls against an ephemeral
//! `<dir>/filar-job-{session}-{id}.log` removed when the job finishes or is
//! cancelled. `<dir>` is chosen per session by a read-only probe — `$TMPDIR`,
//! then `/tmp`, then `/dev/shm`, the first that exists and is writable (#599)
//! — so read-only containers get an honest refusal instead of a job that
//! "exits" without output.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
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
    /// Local jobs: stdout/stderr readers that have not reached EOF yet. The
    /// child can be reaped before its last output is read, so "exited" and
    /// "output complete" are separate facts (#431).
    readers_open: Option<Arc<AtomicUsize>>,
}

/// Per-session job table.
#[derive(Default)]
struct SessionJobs {
    next_num: u32,
    jobs: HashMap<String, JobRecord>,
    /// Remote jobs: directory for job logs, chosen by [`probe_log_dir`] before
    /// the first job and reused until [`reset_log_dir`] (#599).
    log_dir: Option<String>,
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

fn remote_log_path(log_dir: &str, session_id: &str, job_id: &str) -> String {
    let dir = log_dir.trim_end_matches('/');
    format!("{dir}/filar-job-{session_id}-{job_id}.log")
}

/// Marker the log-directory probe prints before the chosen directory.
const LOG_DIR_MARKER: &str = "__FILAR_LOGDIR__=";
/// Marker the start command prints before the job's PID.
const PID_MARKER: &str = "__FILAR_PID__=";
/// Marker the start command prints when the log file cannot be created.
const LOG_FAILED_MARKER: &str = "__FILAR_LOG_FAILED__";

/// Read-only probe for a writable log directory: `$TMPDIR`, `/tmp`,
/// `/dev/shm`, first match wins. Uses only `test`, so nothing is created on
/// the host (invariant 1).
const LOG_DIR_PROBE: &str = "for __filar_d in \"${TMPDIR:-}\" /tmp /dev/shm; do \
     if [ -n \"$__filar_d\" ] && [ -d \"$__filar_d\" ] && [ -w \"$__filar_d\" ]; then \
     echo \"__FILAR_LOGDIR__=$__filar_d\"; break; fi; done; unset __filar_d";

/// Refusal when no candidate directory is writable (#599).
const NO_WRITABLE_DIR_MESSAGE: &str = "background job unavailable: no writable directory \
     for its log in this container or host (checked $TMPDIR, /tmp, /dev/shm) — run a short \
     command directly instead";

/// Clears the cached log directory of `session_id`, so the next remote job
/// probes again (#599).
///
/// Call it when the session's target changes under the same tab: a
/// reconnect, a transport swap, or a pod replaced by the Kubernetes executor
/// (`KubeExecEvent::PodChanged`). Running jobs keep their recorded log paths.
pub fn reset_log_dir(session_id: &str) {
    let _ = with_session(session_id, |session| {
        session.log_dir = None;
        Ok(())
    });
}

/// Runs [`LOG_DIR_PROBE`] and returns the chosen directory, or `None` when
/// none of the candidates is writable.
async fn probe_log_dir(executor: &dyn CommandExecutor) -> Result<Option<String>> {
    let result = executor.run(LOG_DIR_PROBE).await?;
    Ok(result
        .stdout
        .lines()
        .find_map(|line| line.trim_end().strip_prefix(LOG_DIR_MARKER))
        .map(str::to_string)
        .filter(|dir| !dir.is_empty()))
}

/// Cached log directory of the session, probing on first use.
async fn session_log_dir(executor: &dyn CommandExecutor, session_id: &str) -> Result<String> {
    if let Some(dir) = with_session(session_id, |session| Ok(session.log_dir.clone()))? {
        return Ok(dir);
    }
    let dir = probe_log_dir(executor)
        .await?
        .ok_or_else(|| CoreError::Other(NO_WRITABLE_DIR_MESSAGE.into()))?;
    debug!(session_id, dir = %dir, "background job log directory chosen");
    with_session(session_id, |session| {
        session.log_dir = Some(dir.clone());
        Ok(())
    })?;
    Ok(dir)
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

/// `CREATE_NO_WINDOW` process creation flag (`winbase.h`).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Give the job its own hidden console instead of filar's (Windows), so its
/// progress pane and direct console writes cannot paint over the TUI (#523).
#[cfg(windows)]
fn detach_from_parent_console(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// The PowerShell script a local background job runs: progress records are
/// silenced — they go to the console, never to the captured pipes (#523).
/// Built on every platform so it is tested everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
fn powershell_job_script(command: &str) -> String {
    format!("$ProgressPreference = 'SilentlyContinue'\n{command}")
}

/// Decrements the open-reader count when a reader task ends, however it ends.
struct ReaderGuard(Arc<AtomicUsize>);

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, AtomicOrdering::AcqRel);
    }
}

type LocalJob = (tokio::process::Child, Arc<Mutex<String>>, Arc<AtomicUsize>);

async fn spawn_local_job(command: &str) -> Result<LocalJob> {
    let output_buf = Arc::new(Mutex::new(String::new()));
    let readers_open = Arc::new(AtomicUsize::new(0));

    #[cfg(windows)]
    let mut cmd = {
        let script = powershell_job_script(command);
        let mut c = tokio::process::Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        detach_from_parent_console(&mut c);
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
        readers_open.fetch_add(1, AtomicOrdering::AcqRel);
        let guard = ReaderGuard(readers_open.clone());
        tokio::spawn(async move {
            let _guard = guard;
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
        readers_open.fetch_add(1, AtomicOrdering::AcqRel);
        let guard = ReaderGuard(readers_open.clone());
        tokio::spawn(async move {
            let _guard = guard;
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

    Ok((child, output_buf, readers_open))
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

/// Result of one attempt to start a remote job.
#[derive(Debug, PartialEq, Eq)]
enum RemoteStart {
    /// The log was created and the job runs with this PID.
    Started(u32),
    /// The log file could not be created; the job was not started.
    LogNotWritable,
}

/// Shell command that starts `command` in the background with its output in
/// `log`.
///
/// The log is created first, in a subshell (a failed redirection on `:`
/// would otherwise end a non-interactive persistent shell), and the job is
/// started only if that worked. A failed redirect is thus reported as a
/// start error instead of surfacing later as a job that "exited" with no
/// output (#599).
///
/// The markers are spliced in unquoted: they must stay plain shell words
/// (checked by `markers_are_plain_shell_words`).
fn remote_start_command(log: &str, command: &str) -> String {
    let log_quoted = sh_single_quote(log);
    let command_quoted = sh_single_quote(command);
    format!(
        "if ( : > {log_quoted} ) 2>/dev/null; then nohup sh -c {command_quoted} \
         > {log_quoted} 2>&1 & echo \"{PID_MARKER}$!\"; else echo {LOG_FAILED_MARKER}; fi"
    )
}

async fn start_remote_job(
    executor: &dyn CommandExecutor,
    log: &str,
    command: &str,
) -> Result<RemoteStart> {
    let result = executor.run(&remote_start_command(log, command)).await?;
    parse_remote_start(&result.stdout)
}

fn parse_remote_start(stdout: &str) -> Result<RemoteStart> {
    for line in stdout.lines().map(str::trim) {
        if line == LOG_FAILED_MARKER {
            return Ok(RemoteStart::LogNotWritable);
        }
        if let Some(pid) = line.strip_prefix(PID_MARKER) {
            return pid.trim().parse().map(RemoteStart::Started).map_err(|_| {
                CoreError::Other(format!("failed to parse background job PID from: {pid:?}"))
            });
        }
    }
    Err(CoreError::Other(format!(
        "background job failed to start: unexpected shell output {:?}",
        stdout.trim()
    )))
}

/// Starts a remote job in the session's log directory.
///
/// If the log cannot be created there (the directory became read-only, the
/// pod behind the tab was replaced), the cached directory is dropped and the
/// probe runs once more before giving up with a clear start error (#599).
async fn start_remote_job_in_session(
    executor: &dyn CommandExecutor,
    session_id: &str,
    job_id: &str,
    command: &str,
) -> Result<(u32, String)> {
    let mut last_log = String::new();
    for attempt in 0..2 {
        let dir = session_log_dir(executor, session_id).await?;
        let log = remote_log_path(&dir, session_id, job_id);
        match start_remote_job(executor, &log, command).await? {
            RemoteStart::Started(pid) => return Ok((pid, log)),
            RemoteStart::LogNotWritable => {
                warn!(session_id, log = %log, attempt, "background job log not writable");
                reset_log_dir(session_id);
                last_log = log;
            }
        }
    }
    Err(CoreError::Other(format!(
        "background job failed to start: cannot create its log {last_log} (the directory \
         is not writable) — run a short command directly instead"
    )))
}

async fn poll_remote_job(
    executor: &dyn CommandExecutor,
    pid: u32,
    log_path: &str,
    tail_lines: u32,
) -> Result<(JobState, String)> {
    let log_path = sh_single_quote(log_path);
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
    let cmd = format!("kill {pid} 2>/dev/null; rm -f {}", sh_single_quote(log_path));
    executor.run(&cmd).await?;
    Ok(())
}

async fn cleanup_remote_log(executor: &dyn CommandExecutor, log_path: &str) {
    let _ = executor
        .run(&format!("rm -f {}", sh_single_quote(log_path)))
        .await;
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
        let (child, output_buf, readers_open) = spawn_local_job(command).await?;
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
                    readers_open: Some(readers_open),
                },
            );
            Ok(())
        })?;
        pid
    } else {
        let (pid, log_path) =
            start_remote_job_in_session(executor, session_id, &job_id, command).await?;
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
                    readers_open: None,
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

/// Read-only view of one background job, for frontends (#431).
///
/// The job table lives inside the agent and the model only sees it through
/// tool results; [`snapshot`] is the channel a UI reads it through. Taking a
/// snapshot never sends anything to a host: a local job's state and output
/// come from the in-process child and buffer, a remote job's from the
/// agent's **last** `background_job_status` poll (hence [`JobSnapshot::remote`]
/// — its state may be stale, and polling on the UI's behalf would be a
/// command on the host outside the confirm gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSnapshot {
    /// Job id within its session (`job-N`).
    pub job_id: String,
    /// Command the job runs.
    pub command: String,
    /// Lifecycle state (for remote jobs: as of the last poll).
    pub state: JobState,
    /// Last `tail_lines` lines of output.
    pub output_tail: String,
    /// `true` for a job on a remote host (state as of the last poll).
    pub remote: bool,
    /// `false` while a local job's output may still grow: its stdout/stderr
    /// readers have not reached EOF, even if the process already exited.
    /// A frontend keeps refreshing until this is `true`. Always `true` for
    /// remote jobs (their output is whatever the last poll returned).
    pub output_settled: bool,
}

/// Snapshot every job of `session_id`, oldest first (#431).
///
/// Unknown sessions yield an empty list. Only the in-memory registry is
/// read — see [`JobSnapshot`] for why no host is ever contacted here.
pub fn snapshot(session_id: &str, tail_lines: usize) -> Vec<JobSnapshot> {
    let Ok(mut guard) = registry().lock() else {
        return Vec::new();
    };
    let Some(session) = guard.get_mut(session_id) else {
        return Vec::new();
    };
    let mut jobs: Vec<(u32, JobSnapshot)> = session
        .jobs
        .iter_mut()
        .map(|(id, record)| {
            refresh_local_job(record);
            let output_tail = local_output_tail(record, tail_lines);
            let output_settled = record
                .readers_open
                .as_ref()
                .is_none_or(|open| open.load(AtomicOrdering::Acquire) == 0);
            let num = id
                .strip_prefix("job-")
                .and_then(|n| n.parse().ok())
                .unwrap_or(u32::MAX);
            (
                num,
                JobSnapshot {
                    job_id: id.clone(),
                    command: record.command.clone(),
                    state: record.state.clone(),
                    output_tail,
                    remote: record.log_path.is_some(),
                    output_settled,
                },
            )
        })
        .collect();
    jobs.sort_by_key(|(num, _)| *num);
    jobs.into_iter().map(|(_, snap)| snap).collect()
}

/// Last `tail_lines` lines of a job's output, read under the buffer lock
/// without copying the whole buffer.
fn local_output_tail(record: &JobRecord, tail_lines: usize) -> String {
    if let Some(ref buf) = record.output_buf {
        if let Ok(guard) = buf.lock() {
            return last_lines(&guard, tail_lines);
        }
    }
    last_lines(&record.output, tail_lines)
}

/// Last `n` lines of `text` (without a trailing newline).
///
/// Scans backwards from the end, so the cost is the size of the tail, not
/// of the whole text — a noisy job's buffer only grows.
fn last_lines(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let body = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    let start = body
        .rmatch_indices('\n')
        .nth(n - 1)
        .map_or(0, |(i, _)| i + 1);
    body[start..].lines().collect::<Vec<_>>().join("\n")
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

    /// Progress is silenced before the job's command runs (#523).
    #[test]
    fn powershell_job_script_silences_progress_first() {
        let script = powershell_job_script("Test-NetConnection 10.0.0.1");
        assert_eq!(
            script,
            "$ProgressPreference = 'SilentlyContinue'\nTest-NetConnection 10.0.0.1"
        );
    }

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

    fn ok(stdout: &str) -> CommandResult {
        CommandResult {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: Some(0),
            duration: Duration::from_millis(1),
            cwd: None,
        }
    }

    fn mock(responses: Vec<CommandResult>) -> MockExecutor {
        MockExecutor {
            responses: StdMutex::new(responses),
            commands: StdMutex::new(vec![]),
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
                ok("__FILAR_LOGDIR__=/tmp\n"),
                CommandResult {
                    stdout: "__FILAR_PID__=12345\n".into(),
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
        assert_eq!(cmds[0], LOG_DIR_PROBE);
        assert!(cmds[1].contains("nohup sh -c"));
        assert!(cmds.iter().any(|c| c.contains("rm -f")));
    }

    #[test]
    fn snapshot_of_unknown_session_is_empty() {
        assert!(snapshot(&test_session(), 10).is_empty());
    }

    #[test]
    fn last_lines_keeps_the_tail() {
        assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(last_lines("a", 5), "a");
        assert_eq!(last_lines("", 5), "");
        assert_eq!(last_lines("a\r\nb\r\n", 1), "b");
        assert_eq!(last_lines("a\n\nb", 2), "\nb");
        assert_eq!(last_lines("a\nb", 0), "");
    }

    #[tokio::test]
    async fn snapshot_reflects_remote_jobs_without_touching_the_host() {
        let sid = test_session();
        let exec = MockExecutor {
            responses: StdMutex::new(vec![
                ok("__FILAR_LOGDIR__=/tmp\n"),
                ok("__FILAR_PID__=4242\n"),
            ]),
            commands: StdMutex::new(vec![]),
        };
        start_job(&sid, "sleep 100", false, &exec).await.unwrap();
        let sent_before = exec.commands.lock().unwrap().len();

        let snaps = snapshot(&sid, 10);
        assert_eq!(exec.commands.lock().unwrap().len(), sent_before, "snapshot must not run commands");
        assert_eq!(snaps[0].job_id, "job-1");
        assert_eq!(snaps[0].command, "sleep 100");
        assert_eq!(snaps[0].state, JobState::Running);
        assert!(snaps[0].remote);
        assert!(snaps[0].output_settled, "remote output is whatever was polled");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_output_is_settled_only_after_the_readers_finish() {
        let sid = test_session();
        let exec = MockExecutor {
            responses: StdMutex::new(vec![]),
            commands: StdMutex::new(vec![]),
        };
        start_job(&sid, "printf 'one\\ntwo\\n'", true, &exec).await.unwrap();
        // Wait (bounded) for exit and EOF on both pipes.
        let mut snap = snapshot(&sid, 10).remove(0);
        for _ in 0..200 {
            if snap.output_settled && snap.state != JobState::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            snap = snapshot(&sid, 10).remove(0);
        }
        assert_eq!(snap.state, JobState::Done { exit_code: 0 });
        assert!(snap.output_settled);
        assert_eq!(snap.output_tail, "one\ntwo", "final output is complete once settled");
    }

    #[tokio::test]
    async fn snapshot_orders_jobs_numerically() {
        let sid = test_session();
        let exec = MockExecutor {
            responses: StdMutex::new(vec![]),
            commands: StdMutex::new(vec![]),
        };
        #[cfg(unix)]
        let cmd = "sleep 60";
        #[cfg(windows)]
        let cmd = "Start-Sleep -Seconds 60";
        for _ in 0..11 {
            start_job(&sid, cmd, true, &exec).await.unwrap();
        }
        let ids: Vec<String> = snapshot(&sid, 1).into_iter().map(|s| s.job_id).collect();
        assert_eq!(ids.first().map(String::as_str), Some("job-1"));
        assert_eq!(ids.get(1).map(String::as_str), Some("job-2"));
        assert_eq!(ids.last().map(String::as_str), Some("job-11"));
        assert!(snapshot(&sid, 1).iter().all(|s| !s.remote));
        for id in ids {
            cancel_job(&sid, &id, true, &exec).await.unwrap();
        }
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

    // --- #599: log directory for remote jobs ---------------------------------

    /// On a plain SSH host without `$TMPDIR` the probe yields `/tmp`: the log
    /// path and the commands are the same as before #599.
    #[tokio::test]
    async fn ssh_host_with_writable_tmp_keeps_the_tmp_log_path() {
        let sid = test_session();
        let exec = mock(vec![ok("__FILAR_LOGDIR__=/tmp\n"), ok("__FILAR_PID__=77\n")]);
        start_job(&sid, "sleep 5", false, &exec).await.unwrap();
        let cmds = exec.commands.lock().unwrap().clone();
        assert_eq!(cmds.len(), 2);
        assert!(cmds[0].contains("${TMPDIR:-}\" /tmp /dev/shm"), "{}", cmds[0]);
        let log = format!("/tmp/filar-job-{sid}-job-1.log");
        assert!(cmds[1].contains(&format!("( : > {log} )")), "{}", cmds[1]);
        assert!(cmds[1].contains(&format!("> {log} 2>&1 &")), "{}", cmds[1]);
    }

    #[test]
    fn probe_only_tests_and_never_writes() {
        assert!(LOG_DIR_PROBE.contains("[ -d \"$__filar_d\" ] && [ -w \"$__filar_d\" ]"));
        for writer in ["mkdir", "touch", ">", "rm "] {
            assert!(!LOG_DIR_PROBE.contains(writer), "probe must be read-only: {writer}");
        }
    }

    /// The directory is probed once per session and reused for later jobs.
    #[tokio::test]
    async fn log_dir_is_cached_per_session_and_follows_tmpdir() {
        let sid = test_session();
        let exec = mock(vec![
            ok("__FILAR_LOGDIR__=/var/run/app tmp/\n"),
            ok("__FILAR_PID__=1\n"),
            ok("__FILAR_PID__=2\n"),
        ]);
        start_job(&sid, "sleep 1", false, &exec).await.unwrap();
        start_job(&sid, "sleep 2", false, &exec).await.unwrap();
        let cmds = exec.commands.lock().unwrap().clone();
        assert_eq!(cmds.len(), 3, "second job must not probe again: {cmds:?}");
        assert!(cmds[1].contains(&format!("'/var/run/app tmp/filar-job-{sid}-job-1.log'")));
        assert!(cmds[2].contains(&format!("'/var/run/app tmp/filar-job-{sid}-job-2.log'")));
    }

    /// `reset_log_dir` (reconnect, pod change) makes the next job probe again.
    #[tokio::test]
    async fn reset_log_dir_forces_a_new_probe() {
        let sid = test_session();
        let exec = mock(vec![
            ok("__FILAR_LOGDIR__=/tmp\n"),
            ok("__FILAR_PID__=1\n"),
            ok("__FILAR_LOGDIR__=/dev/shm\n"),
            ok("__FILAR_PID__=2\n"),
        ]);
        start_job(&sid, "sleep 1", false, &exec).await.unwrap();
        reset_log_dir(&sid);
        start_job(&sid, "sleep 2", false, &exec).await.unwrap();
        let cmds = exec.commands.lock().unwrap().clone();
        assert_eq!(cmds[2], LOG_DIR_PROBE);
        assert!(cmds[3].contains(&format!("/dev/shm/filar-job-{sid}-job-2.log")));
    }

    /// No writable candidate: an honest refusal, nothing is started or
    /// registered.
    #[tokio::test]
    async fn no_writable_dir_refuses_without_starting_a_job() {
        let sid = test_session();
        let exec = mock(vec![ok("")]);
        let err = start_job(&sid, "sleep 1", false, &exec).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("background job unavailable"), "{msg}");
        assert!(msg.contains("$TMPDIR, /tmp, /dev/shm"), "{msg}");
        assert!(msg.contains("run a short command directly"), "{msg}");
        assert_eq!(exec.commands.lock().unwrap().len(), 1, "only the probe ran");
        assert!(snapshot(&sid, 10).is_empty(), "no phantom job");
    }

    /// A failed redirect is a start error, not a job that "exited" silently:
    /// the probe runs once more, and a second failure gives up.
    #[tokio::test]
    async fn log_creation_failure_is_a_start_error() {
        let sid = test_session();
        let exec = mock(vec![
            ok("__FILAR_LOGDIR__=/tmp\n"),
            ok("__FILAR_LOG_FAILED__\n"),
            ok("__FILAR_LOGDIR__=/tmp\n"),
            ok("__FILAR_LOG_FAILED__\n"),
        ]);
        let err = start_job(&sid, "sleep 1", false, &exec).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("background job failed to start"), "{msg}");
        assert!(msg.contains("not writable"), "{msg}");
        let cmds = exec.commands.lock().unwrap().clone();
        assert_eq!(cmds.len(), 4, "{cmds:?}");
        assert!(cmds.iter().all(|c| !c.contains("nohup") || c.starts_with("if ( : >")));
        assert!(snapshot(&sid, 10).is_empty(), "no phantom job");
    }

    /// A stale cached directory (pod replaced, volume gone) recovers on the
    /// re-probe.
    #[tokio::test]
    async fn stale_cached_dir_recovers_after_reprobe() {
        let sid = test_session();
        let exec = mock(vec![
            ok("__FILAR_LOGDIR__=/tmp\n"),
            ok("__FILAR_LOG_FAILED__\n"),
            ok("__FILAR_LOGDIR__=/dev/shm\n"),
            ok("__FILAR_PID__=99\n"),
        ]);
        let out = start_job(&sid, "sleep 1", false, &exec).await.unwrap();
        assert!(out.contains("pid: 99"), "{out}");
        let cmds = exec.commands.lock().unwrap().clone();
        assert!(cmds[3].contains(&format!("/dev/shm/filar-job-{sid}-job-1.log")));
    }

    /// Cancelling removes the log in the chosen directory.
    #[tokio::test]
    async fn cancel_removes_the_log_in_the_chosen_dir() {
        let sid = test_session();
        let exec = mock(vec![ok("__FILAR_LOGDIR__=/dev/shm\n"), ok("__FILAR_PID__=5\n")]);
        start_job(&sid, "sleep 60", false, &exec).await.unwrap();
        cancel_job(&sid, "job-1", false, &exec).await.unwrap();
        let cmds = exec.commands.lock().unwrap().clone();
        let last = cmds.last().unwrap();
        assert_eq!(last, &format!("kill 5 2>/dev/null; rm -f /dev/shm/filar-job-{sid}-job-1.log"));
    }

    /// Markers are spliced into shell commands unquoted.
    #[test]
    fn markers_are_plain_shell_words() {
        for marker in [LOG_DIR_MARKER, PID_MARKER, LOG_FAILED_MARKER] {
            assert!(
                marker.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c == '='),
                "{marker} must need no shell quoting"
            );
        }
    }

    #[test]
    fn unexpected_start_output_is_an_error() {
        assert!(parse_remote_start("sh: nohup: not found\n").is_err());
        assert_eq!(parse_remote_start("noise\n__FILAR_PID__=12\n").unwrap(), RemoteStart::Started(12));
    }
}
