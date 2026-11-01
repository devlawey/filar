//! Main TUI runner — sets up the terminal and runs the event loop.
//!
//! The runner uses `tokio::select!` to poll both crossterm terminal events
//! (keyboard) and agent events (from the agent task). The agent runs in a
//! separate tokio task, and communication happens via channels.

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use filar_agent::{AgentBuilder, CommandConfirmer, LlmClient};
use filar_core::{CommandConfirmMode, CoreError, Result, SecretProvider, StaticSecretProvider};
use filar_transport::{
    posix_cd_input, ClusterApi, CommandExecutor, InteractiveTerminal, KubeClusterApi,
    LocalInteractive, SecretSubstitutingExecutor, SshExecutor, SshInteractive, SshTransportConfig,
    OSC7_PWD_PROBE,
};
use tokio_util::sync::CancellationToken;

use crate::app::{messages_to_markdown, App, AppMode, RunbookState, SaveProgress, SessionId};
use crate::confirmer::TuiConfirmer;
use crate::event::TuiEvent;
use crate::path_picker::{self, PathEntry};
use crate::terminal::TerminalModel;
use crate::ui;

/// Chunk emitted by a per-backend reader task via the tagged channel.
enum TermChunk {
    /// Output bytes to feed into the terminal model.
    Bytes(Vec<u8>),
    /// Terminal session ended (shell exited).
    Eof,
    /// I/O error reading from the backend.
    Err(filar_core::CoreError),
}

/// Outcome of routing a terminal chunk to its session.
#[derive(Debug)]
enum RouteOutcome {
    /// Chunk fed to the target session's terminal model (and/or marked).
    Fed,
    /// EOF – terminal ended, caller should teardown backend.
    Eof,
    /// Error – caller should teardown backend.
    Error(filar_core::CoreError),
    /// Session not found (tab already closed), chunk discarded.
    Ignored,
}

/// Route a terminal chunk to the correct session by SessionId.
///
/// - `Bytes` are fed into the target session's `terminal` model.
///   If the target is not the active session, `has_new` is set to true.
/// - `Eof`/`Err` return the respective outcome without modifying the model.
/// - If the session is not found (tab closed), the chunk is silently ignored.
fn route_term_chunk(app: &mut App, sid: SessionId, chunk: TermChunk) -> RouteOutcome {
    if app.sessions.is_empty() {
        return RouteOutcome::Ignored;
    }
    let Some(current) = app.sessions.get(app.active) else {
        return RouteOutcome::Ignored;
    };
    let active_id = current.id;
    let Some(session) = app.sessions.iter_mut().find(|s| s.id == sid) else {
        return RouteOutcome::Ignored;
    };
    let is_background = active_id != sid;

    match chunk {
        TermChunk::Bytes(bytes) => {
            if let Some(ref mut model) = session.terminal {
                model.feed(&bytes);
                if let Some(cwd) = model.take_osc7_cwd() {
                    session.pty_prompt_cwd = Some(cwd.clone());
                    session.cwd = Some(cwd);
                }
            }
            if is_background {
                session.has_new = true;
            }
            RouteOutcome::Fed
        }
        TermChunk::Eof => RouteOutcome::Eof,
        TermChunk::Err(e) => RouteOutcome::Error(e),
    }
}

/// Drain PTY output until `session.cwd` is set (fresh after clear) or timeout.
async fn drain_pty_cwd(
    app: &mut App,
    sid: SessionId,
    term_rx: &mut Option<mpsc::UnboundedReceiver<(SessionId, TermChunk)>>,
    timeout: Duration,
) {
    let Some(rx) = term_rx.as_mut() else {
        return;
    };
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some((chunk_sid, chunk))) => {
                let _ = route_term_chunk(app, chunk_sid, chunk);
                if app
                    .sessions
                    .iter()
                    .find(|s| s.id == sid)
                    .and_then(|s| s.cwd.as_ref())
                    .is_some()
                {
                    return;
                }
            }
            _ => return,
        }
    }
}

/// Route PTY output until the shell goes quiet — a self-reporting shell's
/// prompt (#482) may still be on its way when the user hides right after a
/// `cd`. Stops after [`SETTLE_QUIET`] without output, or [`SETTLE_MAX`] in
/// total; an idle shell costs one quiet window.
async fn drain_pty_until_quiet(
    app: &mut App,
    term_rx: &mut Option<mpsc::UnboundedReceiver<(SessionId, TermChunk)>>,
) {
    let Some(rx) = term_rx.as_mut() else {
        return;
    };
    let deadline = tokio::time::Instant::now() + SETTLE_MAX;
    loop {
        let quiet = (tokio::time::Instant::now() + SETTLE_QUIET).min(deadline);
        match tokio::time::timeout_at(quiet, rx.recv()).await {
            Ok(Some((chunk_sid, chunk))) => {
                let _ = route_term_chunk(app, chunk_sid, chunk);
            }
            _ => return,
        }
    }
}

/// Silence that ends [`drain_pty_until_quiet`].
const SETTLE_QUIET: Duration = Duration::from_millis(80);
/// Upper bound of [`drain_pty_until_quiet`], for a shell that never stops.
const SETTLE_MAX: Duration = Duration::from_millis(500);

/// Output must be quiet this long before the terminal's directory is asked
/// for (#499): the command has most likely finished printing.
const CWD_WATCH_QUIET: Duration = Duration::from_millis(200);
/// Shortest pause between two questions about one terminal.
const CWD_WATCH_MIN_GAP: Duration = Duration::from_secs(1);
/// Longest pause between two questions while a program keeps running.
const CWD_WATCH_MAX_GAP: Duration = Duration::from_secs(10);

/// When to ask a terminal where it is (#499).
///
/// A POSIX shell does not report its directory, so the status bar would
/// only move when the terminal is hidden. Instead the backend is asked
/// ([`InteractiveTerminal::foreground_cwd`] — nothing is typed) after the
/// user presses Enter, once the output goes quiet. While the started program
/// is still in the foreground its output keeps the watch armed, at a growing
/// interval; back at the prompt the watch rests until the next Enter, so an
/// idle terminal and a user typing a line cost nothing.
#[derive(Debug)]
struct CwdWatch {
    /// A question is due once the output is quiet.
    armed: bool,
    /// The program started by the last Enter may still be running.
    busy: bool,
    /// A question is on its way.
    in_flight: bool,
    last_activity: Instant,
    last_asked: Option<Instant>,
    gap: Duration,
}

impl CwdWatch {
    fn new(now: Instant) -> Self {
        Self {
            armed: false,
            busy: false,
            in_flight: false,
            last_activity: now,
            last_asked: None,
            gap: CWD_WATCH_MIN_GAP,
        }
    }

    /// The user pressed Enter: a command may have started.
    fn note_enter(&mut self, now: Instant) {
        self.armed = true;
        self.busy = true;
        self.gap = CWD_WATCH_MIN_GAP;
        self.last_activity = now;
    }

    /// The terminal printed something.
    fn note_output(&mut self, now: Instant) {
        if self.busy {
            self.armed = true;
            self.last_activity = now;
        }
    }

    /// Whether to ask now.
    fn due(&self, now: Instant) -> bool {
        self.armed
            && !self.in_flight
            && now.duration_since(self.last_activity) >= CWD_WATCH_QUIET
            && self
                .last_asked
                .is_none_or(|t| now.duration_since(t) >= self.gap)
    }

    /// The question was sent.
    fn asked(&mut self, now: Instant) {
        self.armed = false;
        self.in_flight = true;
        self.last_asked = Some(now);
    }

    /// The answer came: `at_prompt` is `None` when the backend could not tell.
    fn answered(&mut self, at_prompt: Option<bool>) {
        self.in_flight = false;
        if at_prompt == Some(false) {
            self.gap = (self.gap * 2).min(CWD_WATCH_MAX_GAP);
        } else {
            self.busy = false;
        }
    }
}

/// Whether `a` and `b` are the same backend. Compared by address: a tab's
/// terminal can be closed and opened again while a question to the old one
/// is still on its way (#499 review).
fn same_backend(a: &Arc<dyn InteractiveTerminal>, b: &Arc<dyn InteractiveTerminal>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

/// Whether input bytes on their way to the PTY contain an Enter.
fn has_enter(bytes: &[u8]) -> bool {
    bytes.iter().any(|b| matches!(b, b'\r' | b'\n'))
}

/// Put an answer of [`InteractiveTerminal::foreground_cwd`] into the tab's
/// status (#499). The prompt bookkeeping of #493 (`pty_prompt_cwd`) is left
/// alone: it still follows only what the shell itself reported.
fn apply_foreground_cwd(app: &mut App, sid: SessionId, path: String) -> bool {
    let Some(idx) = app.find_session_idx(sid) else {
        return false;
    };
    let session = &mut app.sessions[idx];
    if session.terminal.is_none() || session.cwd.as_deref() == Some(path.as_str()) {
        return false;
    }
    session.cwd = Some(path);
    true
}

/// Keystrokes on their way to the PTY: an Enter may start a program, and
/// text without one sits on the prompt line where a typed `cd` would join
/// it — either way the shell is not known to sit at an empty prompt until it
/// reports again (#493).
fn note_pty_input(app: &mut App, sid: SessionId, bytes: &[u8]) {
    if !bytes.is_empty() {
        if let Some(idx) = app.find_session_idx(sid) {
            app.sessions[idx].pty_prompt_cwd = None;
        }
    }
}

/// Before a hidden terminal is shown again, type a `cd` to the tab's cwd if
/// the agent or `!` moved it since the shell last reported (#493).
///
/// Only a shell known to be idle at its prompt is typed into, so a program
/// left running in the terminal (an editor, `top`) never receives the `cd`.
async fn carry_agent_cwd_into_pty(
    app: &mut App,
    sid: SessionId,
    term: &Arc<dyn InteractiveTerminal>,
) {
    let Some(idx) = app.find_session_idx(sid) else {
        return;
    };
    let session = &app.sessions[idx];
    let (Some(want), Some(at)) = (session.cwd.as_deref(), session.pty_prompt_cwd.as_deref()) else {
        return;
    };
    if want == at {
        return;
    }
    let Some(input) = term.cd_input(want) else {
        return;
    };
    if term.write_input(input.as_bytes()).await.is_ok() {
        app.sessions[idx].pty_prompt_cwd = None;
    }
}

/// Probe OSC 7 on a live interactive PTY, update `session.cwd`, and `set_cwd`
/// on the agent executor. Does not close the backend (#338).
///
/// A POSIX shell (local or over SSH) is always probed, so a stale
/// `session.cwd` from enter-time does not skip refresh; the previous cwd is
/// restored if the probe times out. A shell that reports OSC 7 from its
/// prompt (cmd.exe, PowerShell — #482) is not probed: its queued output is
/// read until it goes quiet and the latest reported directory is used.
async fn sync_cwd_from_interactive(
    app: &mut App,
    sid: SessionId,
    term: &Arc<dyn InteractiveTerminal>,
    executors: &HashMap<SessionId, ExecutorEntry>,
    term_rx: &mut Option<mpsc::UnboundedReceiver<(SessionId, TermChunk)>>,
) {
    let prev = app
        .sessions
        .iter()
        .find(|s| s.id == sid)
        .and_then(|s| s.cwd.clone());
    // The probe goes by the shell, not the client's OS (#482): a POSIX shell
    // — local or over SSH — is asked with `printf`; cmd.exe and PowerShell
    // report OSC 7 from every prompt, so their queued output already holds
    // the latest directory.
    if term.reports_cwd_itself() {
        drain_pty_until_quiet(app, term_rx).await;
    } else {
        if let Some(idx) = app.find_session_idx(sid) {
            app.sessions[idx].cwd = None;
        }
        if term.write_input(OSC7_PWD_PROBE).await.is_err() {
            if let Some(idx) = app.find_session_idx(sid) {
                app.sessions[idx].cwd = prev;
            }
        } else {
            drain_pty_cwd(app, sid, term_rx, Duration::from_millis(400)).await;
            let still_empty = app
                .sessions
                .iter()
                .find(|s| s.id == sid)
                .and_then(|s| s.cwd.as_ref())
                .is_none();
            if still_empty {
                if let Some(idx) = app.find_session_idx(sid) {
                    app.sessions[idx].cwd = prev;
                }
            }
        }
    }
    // Always apply known cwd to the executor (incl. Windows last-known path).
    if let Some(cwd) = app
        .sessions
        .iter()
        .find(|s| s.id == sid)
        .and_then(|s| s.cwd.clone())
    {
        if let Some(entry) = executors.get(&sid) {
            if let Err(e) = entry.executor.set_cwd(&cwd).await {
                warn!(error = %e, "failed to sync executor cwd");
            }
        }
    }
}

use filar_core::ChatBlock;

// ---------------------------------------------------------------------------
// TuiExecutor — wraps an executor for runtime swapping
// ---------------------------------------------------------------------------

/// A [`CommandExecutor`] wrapper whose inner executor is swappable at runtime.
///
/// This allows the transport to switch from local to SSH (or vice versa)
/// without restarting the app. Secret substitution and output sanitisation
/// are handled by [`SecretSubstitutingExecutor`] in the engine, which wraps
/// this executor during `AgentBuilder::build()`.
struct TuiExecutor {
    inner: Arc<tokio::sync::RwLock<Arc<dyn CommandExecutor>>>,
}

impl TuiExecutor {
    /// Swap the inner executor of session `sid` to a new one (e.g. local →
    /// SSH).
    ///
    /// The new target may have a different writable temp directory, so the
    /// session's cached background-job log directory is dropped (#599).
    async fn swap_executor(&self, sid: SessionId, new: Arc<dyn CommandExecutor>) {
        let mut guard = self.inner.write().await;
        *guard = new;
        filar_agent::background::reset_log_dir(&sid.0.to_string());
    }
}

#[async_trait::async_trait]
impl CommandExecutor for TuiExecutor {
    async fn run(&self, command: &str) -> Result<filar_transport::CommandResult> {
        let executor = self.inner.read().await.clone();
        executor.run(command).await
    }

    async fn cancel(&self) -> Result<()> {
        let executor = self.inner.read().await.clone();
        executor.cancel().await
    }

    async fn set_cwd(&self, path: &str) -> Result<()> {
        let executor = self.inner.read().await.clone();
        executor.set_cwd(path).await
    }

    async fn current_cwd(&self) -> Option<String> {
        let executor = self.inner.read().await.clone();
        executor.current_cwd().await
    }
}

// ---------------------------------------------------------------------------
// Per-session executor storage
// ---------------------------------------------------------------------------

/// Entry in the per-session executor map.
///
/// Each session tab has its own executor so that `!ssh` only reconnects
/// the tab that issued it, and `Ctrl+T` opens a PTY on the tab's host.
struct ExecutorEntry {
    executor: Arc<TuiExecutor>,
    /// SSH target stored so Ctrl+T can open an interactive terminal on the
    /// same host. `None` for local sessions. Shared via `Arc<RwLock>` so the
    /// `!ssh` spawned task can write it alongside swapping the executor.
    ssh_target: Arc<tokio::sync::RwLock<Option<filar_core::SshTarget>>>,
    /// The tab's SSH executor itself (unwrapped), so a file panel on the
    /// tab's host can open an SFTP channel on its connection (#535). `None`
    /// for local tabs. Kept next to `ssh_target` at every swap.
    ssh_exec: Arc<tokio::sync::RwLock<Option<Arc<SshExecutor>>>>,
}

// ---------------------------------------------------------------------------
// Panic hook guard
// ---------------------------------------------------------------------------

/// Shared panic-safe session snapshot.
///
/// The periodic auto-save keeps this fresh so the panic hook can persist the
/// last known-good session even when the app dies unexpectedly.
type SessionSnapshot = Arc<Mutex<Option<filar_core::Session>>>;

/// RAII guard that restores the default panic hook when dropped.
///
/// Installs a custom panic hook that restores the terminal state
/// (disables raw mode, leaves alternate screen, disables mouse capture)
/// *before* printing the panic message. This ensures the user can read
/// the panic text and select it with the mouse even if a panic occurs
/// inside the event loop or rendering code.
///
/// The hook also performs a best-effort session save from the shared
/// snapshot (see [`run`]), so a panic loses at most one auto-save period.
///
/// When the guard is dropped (either after normal teardown or on early
/// return), the original panic hook is restored via `take_hook()`, so
/// code running after the TUI is unaffected.
struct PanicHookGuard;

impl PanicHookGuard {
    /// Install the terminal-restoring panic hook and return a guard.
    fn install(snapshot: SessionSnapshot) -> Self {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Restore terminal state BEFORE printing the panic message
            // so the user can read it and select the text.
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::event::DisableBracketedPaste,
                crossterm::event::DisableMouseCapture,
                crossterm::terminal::LeaveAlternateScreen
            );
            let _ = crossterm::terminal::disable_raw_mode();

            // Best-effort session save. `try_lock` avoids blocking (and a
            // potential double-panic) if the mutex is somehow still held.
            // Holding the lock while writing serialises this save with the
            // periodic auto-save (which uses the same mutex around its write).
            if let Ok(guard) = snapshot.try_lock() {
                if let Some(session) = guard.clone() {
                    if let Ok(store) = filar_core::SessionStore::with_default_dir() {
                        let _ = store.save(&session);
                        let _ = store.prune_to(filar_core::session::MAX_SESSIONS);
                    }
                }
            }

            default_hook(info);
        }));
        Self
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        // Never call take_hook/set_hook while unwinding — that panics again and
        // turns a recoverable TUI panic into abort (#324).
        if std::thread::panicking() {
            return;
        }
        // Restore the original panic hook.
        let _ = std::panic::take_hook();
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Configuration for the TUI runner.
pub struct TuiConfig {
    pub target_name: String,
    pub confirm_mode: CommandConfirmMode,
    /// Confirmation mode from config.toml — the baseline tag policies fold
    /// against (#414). Distinct from `confirm_mode`, which a restored
    /// session may override.
    pub global_confirm_mode: CommandConfirmMode,
    /// Tag-bound confirmation policies from `[[tag_policies]]` (#414).
    pub tag_policies: Vec<filar_core::TagPolicy>,
    /// Host groups from `[[host_groups]]` (#418). Targets that are members
    /// of a read-only group get a [`filar_transport::ReadOnlyExecutor`]
    /// around every SSH executor swapped in for their session (#419).
    pub host_groups: Vec<filar_core::HostGroup>,
    /// Group to open the fleet layer over at start-up (`--group`, #432).
    /// `None` starts on the single target, exactly as before.
    pub initial_group: Option<String>,
    /// A saved fleet session to reopen at start-up (`--resume` of a fleet
    /// dialogue, #442): the fleet layer over its saved composition. Takes
    /// precedence over `initial_group`.
    pub initial_fleet: Option<filar_core::Session>,
    pub llm_profile: String,
    pub initial_messages: Vec<ChatBlock>,
    /// Initial agent input history (for session restore).
    pub initial_input_history: Vec<String>,
    /// LLM profile restored from a previous session.
    pub initial_llm_profile: Option<String>,
    /// Input token count restored from a previous session.
    pub initial_tokens_in: u64,
    /// Output token count restored from a previous session.
    pub initial_tokens_out: u64,
    /// Cumulative cost in USD restored from a previous session.
    pub initial_cost_usd: Option<f64>,
    /// Per-profile token breakdown restored from a previous session.
    pub initial_per_profile: HashMap<String, filar_core::ProfileUsage>,
    /// Last served model restored from a previous session.
    pub initial_last_served_model: Option<String>,
    /// Per-profile served model map restored from a previous session.
    pub initial_model_per_profile: HashMap<String, String>,
    /// SSH target for interactive terminal mode (Ctrl+T).
    /// If `None`, the agent runs in local mode.
    pub ssh_target: Option<filar_core::SshTarget>,
    /// The start-up SSH executor itself, when the first tab is on SSH — the
    /// file panels open SFTP on its connection (#535). `None` falls back to a
    /// connection of the panel's own.
    pub initial_ssh_executor: Option<Arc<SshExecutor>>,
    /// SSH connection info restored from a saved session (e.g. `user@host:port`).
    /// Used to pre-populate the session's display info on restore; the actual
    /// reconnection is handled by the session-select overlay / launcher.
    pub initial_ssh_info: Option<String>,
    /// Whether commands execute on the local machine (true) or over SSH (false).
    pub is_local: bool,
    /// Secret provider for command substitution and output sanitisation.
    /// Shared between the TUI (for dynamic `$FILAR_SECRET_N` insertion via
    /// Ctrl+P) and the agent (via `SecretSubstitutingExecutor`).
    pub secret_provider: Arc<StaticSecretProvider>,
    /// All configured LLM profiles loaded from config.
    pub profiles: Vec<filar_core::LlmProfile>,
    /// Name of the default (startup) profile.
    pub default_profile_name: String,
    /// Factory for creating per-session LLM clients from profiles.
    pub llm_factory: Arc<
        dyn Fn(
                &filar_core::LlmProfile,
                &StaticSecretProvider,
            ) -> std::result::Result<Arc<dyn LlmClient>, CoreError>
            + Send
            + Sync,
    >,
    /// For validating a profile's API key at Ctrl+L time without building a client.
    pub key_checker: Arc<dyn Fn(&filar_core::LlmProfile) -> Option<String> + Send + Sync>,
    /// Named SSH targets from config for Ctrl+O cycling.
    pub ssh_targets: Vec<filar_core::SshTarget>,
    /// Named Kubernetes targets available in Ctrl+O (#575).
    pub kube_clusters: Vec<filar_core::KubeCluster>,
    /// Cluster identity restored by configured target name; reconnection is asynchronous.
    pub initial_kube: Option<filar_core::KubeSessionSnapshot>,
    /// Receiver for WARN/ERROR log lines forwarded from the tracing subscriber
    /// (see [`crate::log_layer`]). The runner polls it and shows each line as a
    /// `System` block, so important logs surface in the chat instead of being
    /// painted over the interface. `None` disables the feature (e.g. in tests).
    pub log_rx: Option<mpsc::UnboundedReceiver<String>>,
    /// Directory where Ctrl+S session exports are written (`None` = CWD).
    pub save_dir: Option<std::path::PathBuf>,
    /// When true, an explicit Ctrl+S also asks the session's LLM for a
    /// runbook, written next to the export as `{stem}.runbook.md` (#401).
    pub save_runbook: bool,
    /// Per-command execution timeout from `[timeouts].command_secs`.
    /// Applied to SSH marker wait and local subprocess execution.
    pub command_timeout: Duration,
    /// When true, run the command arbiter before each confirmation gate.
    pub arbiter_enabled: bool,
    /// Optional named profile for the arbiter (`None` = session profile).
    pub arbiter_profile: Option<String>,
    /// Command safety scorer from `[risk_scorer]` (#549/#551): every
    /// command the agent proposes is rated, advisory only. `None` = off.
    pub risk_scorer: Option<Arc<dyn filar_agent::RiskScorer>>,
    /// `(warn_below, danger_below)` for colouring the ratings.
    pub risk_thresholds: (u8, u8),
    /// Shown once at start-up when the scorer is configured but cannot be
    /// used (invalid section, missing key). Never contains a secret.
    pub risk_scorer_note: Option<String>,
}

/// Result of an asynchronous cluster connection or namespace discovery (#575).
struct KubeConnectionResult {
    session_id: SessionId,
    connection_id: u64,
    result: std::result::Result<(Arc<dyn ClusterApi>, Vec<String>), String>,
}

/// Result of asynchronous Kubernetes fleet discovery or restored API binding.
struct KubeFleetResult {
    session_id: SessionId,
    connection_id: u64,
    result: std::result::Result<Arc<filar_agent::KubeFleet>, String>,
}

/// A single-cluster target or a frozen fleet executed by the same agent loop.
enum KubeAgentTarget {
    Cluster {
        api: Arc<dyn ClusterApi>,
        target: filar_core::KubeCluster,
        namespace: Option<String>,
    },
    Fleet(Arc<filar_agent::KubeFleet>),
}

/// Pod-tab work reported back to the event loop (#565).
enum PodRunnerEvent {
    /// Pods for the chooser of a cluster tab.
    PodList {
        session_id: SessionId,
        select_id: u64,
        result: std::result::Result<Vec<crate::app::PodChoice>, String>,
    },
    /// The pod shell is open.
    Connected {
        session_id: SessionId,
        connection_id: u64,
        exec: Arc<filar_transport::KubeExecutor>,
    },
    /// The pod shell could not be opened.
    Failed {
        session_id: SessionId,
        connection_id: u64,
        error: String,
    },
    /// The shell reconnected or followed its selector to another pod.
    Shell {
        session_id: SessionId,
        connection_id: u64,
        event: filar_transport::KubeExecEvent,
    },
}

/// A pod tab's live shell and the task forwarding its events.
struct PodShell {
    exec: Arc<filar_transport::KubeExecutor>,
    /// The tab's connection generation this shell belongs to.
    connection_id: u64,
    events: tokio::task::JoinHandle<()>,
}

impl PodShell {
    /// Stop forwarding events and close the shell.
    async fn release(self) {
        self.events.abort();
        if let Err(error) = self.exec.close().await {
            debug!(error = %error, "closing a pod shell failed");
        }
    }
}

/// List the running pods of a cluster tab's namespace for its chooser (#565).
fn start_pending_kube_pod_lists(
    app: &mut App,
    kube_apis: &HashMap<SessionId, Arc<dyn ClusterApi>>,
    pod_tx: &mpsc::UnboundedSender<PodRunnerEvent>,
) {
    for request in app.take_pending_kube_pod_lists() {
        let tx = pod_tx.clone();
        let Some(api) = kube_apis.get(&request.session_id).cloned() else {
            let _ = tx.send(PodRunnerEvent::PodList {
                session_id: request.session_id,
                select_id: request.select_id,
                result: Err(
                    "кластер ещё не подключён — повторите, когда подключение завершится".into(),
                ),
            });
            continue;
        };
        tokio::spawn(async move {
            let result = api
                .list_pods(Some(&request.namespace), None, Some("status.phase=Running"))
                .await
                .map(|pods| {
                    crate::app::pod_tab::pod_choices(
                        pods.into_iter()
                            .map(|pod| {
                                let containers =
                                    pod.containers.into_iter().map(|c| c.name).collect();
                                (pod.name, pod.ready, containers)
                            })
                            .collect(),
                    )
                })
                .map_err(|error| error.to_string());
            let _ = tx.send(PodRunnerEvent::PodList {
                session_id: request.session_id,
                select_id: request.select_id,
                result,
            });
        });
    }
}

/// Open every queued pod shell in the background, like an SSH connect (#565).
fn start_pending_kube_pod_connects(
    app: &mut App,
    pod_tx: &mpsc::UnboundedSender<PodRunnerEvent>,
) {
    for request in app.take_pending_kube_pod_connects() {
        let tx = pod_tx.clone();
        tokio::spawn(async move {
            let event =
                match filar_transport::KubeExecutor::connect(&request.cluster, &request.pod_ref)
                    .await
                {
                    Ok(exec) => PodRunnerEvent::Connected {
                        session_id: request.session_id,
                        connection_id: request.connection_id,
                        exec: Arc::new(exec),
                    },
                    Err(error) => PodRunnerEvent::Failed {
                        session_id: request.session_id,
                        connection_id: request.connection_id,
                        error: error.to_string(),
                    },
                };
            let _ = tx.send(event);
        });
    }
}

/// Forward a pod shell's reconnect events to the event loop. The first
/// `Connected` is already over by the time this subscribes; the identity
/// is read from the executor right after `connect` instead.
fn forward_pod_events(
    exec: &filar_transport::KubeExecutor,
    session_id: SessionId,
    connection_id: u64,
    pod_tx: mpsc::UnboundedSender<PodRunnerEvent>,
) -> tokio::task::JoinHandle<()> {
    let mut events = exec.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if pod_tx
                        .send(PodRunnerEvent::Shell {
                            session_id,
                            connection_id,
                            event,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Start every queued cluster connection or namespace discovery (#575).
fn start_pending_kube_connects(
    app: &mut App,
    kube_apis: &mut HashMap<SessionId, Arc<dyn ClusterApi>>,
    kube_tx: &mpsc::UnboundedSender<KubeConnectionResult>,
) {
    for request in app.take_pending_kube_connects() {
        kube_apis.remove(&request.session_id);
        let tx = kube_tx.clone();
        tokio::spawn(async move {
            let session_id = request.session_id;
            let connection_id = request.connection_id;
            let result = match KubeClusterApi::connect(&request.target).await {
                Ok(api) => {
                    let api: Arc<dyn ClusterApi> = Arc::new(api);
                    if request.discover {
                        match api.namespaces().await {
                            Ok(namespaces) => Ok((
                                api,
                                namespaces
                                    .into_iter()
                                    .map(|namespace| namespace.name)
                                    .collect(),
                            )),
                            Err(error) => Err(error.to_string()),
                        }
                    } else {
                        Ok((api, Vec::new()))
                    }
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = tx.send(KubeConnectionResult {
                session_id,
                connection_id,
                result,
            });
        });
    }
}

/// Start every queued Kubernetes fleet discovery or restored API binding (#569).
fn start_pending_kube_fleets(
    app: &mut App,
    kube_fleets: &mut HashMap<SessionId, Arc<filar_agent::KubeFleet>>,
    kube_fleet_tx: &mpsc::UnboundedSender<KubeFleetResult>,
) {
    let configured = app.kube_clusters.clone();
    let secrets: Arc<dyn SecretProvider> = app.secrets.clone();
    for request in app.take_pending_kube_fleets() {
        kube_fleets.remove(&request.session_id);
        let session_id = request.session_id;
        let connection_id = request.connection_id;
        let operation = request.operation;
        let discover = request.discover;
        let cancellation = request.cancellation;
        let mut names = if discover {
            operation
                .group()
                .kube
                .as_ref()
                .map(|kube| kube.clusters.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if !discover {
            for member in operation.members() {
                let name = match member.target() {
                    filar_core::FleetTarget::KubePod(pod) => &pod.cluster,
                    filar_core::FleetTarget::KubeCluster(scope) => &scope.cluster,
                    filar_core::FleetTarget::Ssh(_) => continue,
                };
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }

        let tx = kube_fleet_tx.clone();
        let configured = configured.clone();
        let secrets = secrets.clone();
        tokio::spawn(async move {
            let result: std::result::Result<Arc<filar_agent::KubeFleet>, String> = async {
                let mut clusters = Vec::with_capacity(names.len());
                for name in names {
                    let target = configured
                        .iter()
                        .find(|target| target.name == name)
                        .cloned()
                        .ok_or_else(|| format!("Kubernetes cluster '{name}' is not configured"))?;
                    let cluster = tokio::select! {
                        _ = cancellation.cancelled() => {
                            return Err("cancelled".to_string());
                        }
                        result = filar_agent::KubeFleetCluster::connect(
                            target,
                            Arc::clone(&secrets),
                        ) => result.map_err(|error| error.to_string())?,
                    };
                    clusters.push(cluster);
                }
                let fleet = if discover {
                    filar_agent::KubeFleet::open_until(operation.group(), clusters, &cancellation)
                        .await
                        .map_err(|error| error.to_string())?
                } else {
                    filar_agent::KubeFleet::from_operation(operation, clusters)
                        .map_err(|error| error.to_string())?
                };
                Ok(Arc::new(fleet))
            }
            .await;
            let _ = tx.send(KubeFleetResult {
                session_id,
                connection_id,
                result,
            });
        });
    }
}

/// SSH transport tunables with the app-configured command timeout.
fn ssh_transport_config(command_timeout: Duration) -> SshTransportConfig {
    SshTransportConfig::default().with_command_timeout(command_timeout)
}

// ---------------------------------------------------------------------------
// File panels (Ctrl+G, #535)
// ---------------------------------------------------------------------------

/// An open panel source.
#[derive(Clone)]
struct PanelConn {
    ft: Arc<dyn filar_transport::FileTransfer>,
    /// Set for SSH sources, so the channel (and an owned connection) can be
    /// closed when the panel moves on.
    sftp: Option<Arc<filar_transport::SftpFileTransfer>>,
    /// Writes refused (read-only host group, #419).
    read_only: bool,
}

impl PanelConn {
    /// Close an SSH source in the background (a tab's connection is kept) —
    /// unless a copy still uses it: the copy closes it when done (#536).
    fn close(self) {
        let PanelConn { ft, sftp, .. } = self;
        drop(ft);
        if let Some(sftp) = sftp {
            if Arc::strong_count(&sftp) == 1 {
                tokio::spawn(async move {
                    let _ = sftp.close().await;
                });
            }
        }
    }
}

/// Messages from panel tasks.
enum PanelMsg {
    /// A source opened for `side` under request `generation`.
    Ready {
        side: crate::file_panels::Side,
        generation: u64,
        conn: PanelConn,
    },
    /// State for the panel itself.
    Event(crate::file_panels::PanelEvent),
    /// Progress of copy `id` (#536).
    CopyEvent {
        id: u64,
        event: filar_transport::CopyEvent,
    },
    /// Copy `id` asks whether to replace `dst`.
    CopyAsk {
        id: u64,
        dst: String,
        reply: tokio::sync::oneshot::Sender<filar_transport::OverwriteDecision>,
    },
    /// Copy `id` ended.
    CopyDone {
        id: u64,
        report: filar_transport::CopyReport,
    },
}

/// A copy in flight: how to stop it and how to answer its question.
struct CopyTask {
    cancel: CancellationToken,
    reply: Option<tokio::sync::oneshot::Sender<filar_transport::OverwriteDecision>>,
}

/// Forwards engine progress to the runner; asks overwrite questions through it.
struct ChannelObserver {
    id: u64,
    tx: tokio::sync::mpsc::UnboundedSender<PanelMsg>,
}

#[async_trait::async_trait]
impl filar_transport::CopyObserver for ChannelObserver {
    async fn on_event(&self, event: filar_transport::CopyEvent) {
        let _ = self.tx.send(PanelMsg::CopyEvent { id: self.id, event });
    }

    async fn decide_overwrite(&self, dst: &str) -> filar_transport::OverwriteDecision {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let asked = self.tx.send(PanelMsg::CopyAsk {
            id: self.id,
            dst: dst.to_string(),
            reply,
        });
        if asked.is_err() {
            return filar_transport::OverwriteDecision::Cancel;
        }
        // No answer (the app went away) never replaces anything.
        answer
            .await
            .unwrap_or(filar_transport::OverwriteDecision::Cancel)
    }
}

/// What panel tasks need from the runner.
struct PanelCtx {
    tx: tokio::sync::mpsc::UnboundedSender<PanelMsg>,
    command_timeout: Duration,
    host_groups: Vec<filar_core::HostGroup>,
}

/// How a source is reached.
enum PanelOpen {
    Local,
    /// A configured target; saved secrets are filled like Ctrl+O.
    Target(filar_core::SshTarget),
    /// A target with a secret the user just typed.
    Typed(filar_core::SshTarget),
    /// A tab's host: SFTP on its connection when the executor is known.
    TabHost {
        exec: Arc<tokio::sync::RwLock<Option<Arc<SshExecutor>>>>,
        target: Arc<tokio::sync::RwLock<Option<filar_core::SshTarget>>>,
    },
}

fn side_index(side: crate::file_panels::Side) -> usize {
    match side {
        crate::file_panels::Side::Left => 0,
        crate::file_panels::Side::Right => 1,
    }
}

/// Apply a panel task's answer.
fn handle_panel_msg(
    app: &mut App,
    sides: &mut [Option<PanelConn>; 2],
    copies: &mut HashMap<u64, CopyTask>,
    msg: PanelMsg,
) {
    match msg {
        PanelMsg::CopyEvent { id, event } => app.apply_copy_event(id, event),
        PanelMsg::CopyAsk { id, dst, reply } => match copies.get_mut(&id) {
            Some(task) => {
                task.reply = Some(reply);
                app.ask_copy_overwrite(id, dst);
            }
            None => {
                let _ = reply.send(filar_transport::OverwriteDecision::Cancel);
            }
        },
        PanelMsg::CopyDone { id, report } => {
            copies.remove(&id);
            app.finish_copy(id, report);
        }
        PanelMsg::Ready {
            side,
            generation,
            conn,
        } => {
            let current = app
                .file_panels
                .as_ref()
                .is_some_and(|fp| fp.panel(side).generation == generation);
            if current {
                if let Some(old) = sides[side_index(side)].replace(conn) {
                    old.close();
                }
            } else {
                conn.close();
            }
        }
        PanelMsg::Event(event) => {
            if let Some(fp) = app.file_panels.as_mut() {
                fp.apply(event);
            }
        }
    }
}

/// Start the panels' pending requests; close the sources of closed panels.
fn drive_file_panels(
    app: &mut App,
    sides: &mut [Option<PanelConn>; 2],
    executors: &HashMap<SessionId, ExecutorEntry>,
    ctx: &PanelCtx,
) {
    use crate::file_panels::{PanelRequest, PanelSource};
    let Some(fp) = app.file_panels.as_mut() else {
        for conn in sides.iter_mut().filter_map(Option::take) {
            conn.close();
        }
        return;
    };
    let requests = std::mem::take(&mut fp.requests);
    for req in requests {
        match req {
            PanelRequest::Open {
                side,
                source,
                start_dir,
                generation,
            } => {
                let open = match source {
                    PanelSource::Local => PanelOpen::Local,
                    PanelSource::Target(i) => match app.ssh_targets.get(i) {
                        Some(t) => PanelOpen::Target(t.clone()),
                        None => {
                            fail_open(ctx, side, generation, "no such host in the config");
                            continue;
                        }
                    },
                    PanelSource::TabHost(sid) => match executors.get(&sid) {
                        Some(e) => PanelOpen::TabHost {
                            exec: e.ssh_exec.clone(),
                            target: e.ssh_target.clone(),
                        },
                        None => {
                            fail_open(ctx, side, generation, "the tab is closed");
                            continue;
                        }
                    },
                };
                spawn_panel_open(ctx, side, generation, open, start_dir);
            }
            PanelRequest::OpenTarget {
                side,
                target,
                generation,
            } => spawn_panel_open(ctx, side, generation, PanelOpen::Typed(target), None),
            PanelRequest::CreateDir { side, dir, name } => {
                use crate::file_panels::PanelEvent;
                let conn = sides[side_index(side)].clone();
                let tx = ctx.tx.clone();
                let Some(conn) = conn else {
                    let _ = tx.send(PanelMsg::Event(PanelEvent::DirCreated {
                        side,
                        dir,
                        name,
                        result: Err("the panel lost its connection — refresh it (r)".into()),
                    }));
                    continue;
                };
                tokio::spawn(async move {
                    let path = conn.ft.path_style().join(&dir, &name);
                    // Only through the side's `create_dir` (SFTP `mkdir` on a
                    // host): nothing runs in a shell (#546, AGENTS.md #531).
                    let result = if conn.read_only {
                        Err("the host is read-only".to_string())
                    } else {
                        match conn.ft.stat(&path).await {
                            Ok(Some(_)) => Err("already exists".to_string()),
                            Ok(None) => conn.ft.create_dir(&path).await.map_err(|e| e.to_string()),
                            Err(e) => Err(e.to_string()),
                        }
                    };
                    let _ = tx.send(PanelMsg::Event(PanelEvent::DirCreated {
                        side,
                        dir,
                        name,
                        result,
                    }));
                });
            }
            PanelRequest::List {
                side,
                path,
                generation,
            } => match &sides[side_index(side)] {
                Some(conn) => {
                    let ft = conn.ft.clone();
                    let tx = ctx.tx.clone();
                    tokio::spawn(async move {
                        let result = ft.list_dir(&path).await;
                        if let Err(e) = &result {
                            if filar_transport::is_connection_lost(e) {
                                warn!(error = %e, "file panel source lost its connection");
                            }
                        }
                        let _ = tx.send(PanelMsg::Event(crate::file_panels::PanelEvent::Listed {
                            side,
                            generation,
                            path,
                            result: result.map_err(|e| e.to_string()),
                        }));
                    });
                }
                // The source went away (closed, or never opened): open it
                // again at the same place.
                None => {
                    let source = fp.panel(side).source.clone();
                    let open = match source {
                        Some(PanelSource::Local) => PanelOpen::Local,
                        Some(PanelSource::Target(i)) => match app.ssh_targets.get(i) {
                            Some(t) => PanelOpen::Target(t.clone()),
                            None => continue,
                        },
                        Some(PanelSource::TabHost(sid)) => match executors.get(&sid) {
                            Some(e) => PanelOpen::TabHost {
                                exec: e.ssh_exec.clone(),
                                target: e.ssh_target.clone(),
                            },
                            None => continue,
                        },
                        None => continue,
                    };
                    spawn_panel_open(ctx, side, generation, open, Some(path));
                }
            },
        }
    }
}

/// Start, cancel and answer copies (#536).
fn drive_copies(
    app: &mut App,
    sides: &[Option<PanelConn>; 2],
    copies: &mut HashMap<u64, CopyTask>,
    ctx: &PanelCtx,
) {
    use crate::file_panels::CopyRequest;
    for req in std::mem::take(&mut app.copy_requests) {
        match req {
            CopyRequest::Start { id, plan } => {
                let from = sides[side_index(plan.from)].clone();
                let to = sides[side_index(plan.from.other())].clone();
                let refused = match (&from, &to) {
                    (Some(_), Some(to)) if to.read_only => Some("the target host is read-only"),
                    (Some(_), Some(_)) => None,
                    _ => Some("a panel lost its connection — refresh it (r) and try again"),
                };
                if let Some(why) = refused {
                    app.finish_copy(
                        id,
                        filar_transport::CopyReport {
                            failed: vec![(plan.target(), why.to_string())],
                            ..Default::default()
                        },
                    );
                    continue;
                }
                let (Some(from), Some(to)) = (from, to) else {
                    continue;
                };
                let cancel = CancellationToken::new();
                copies.insert(
                    id,
                    CopyTask {
                        cancel: cancel.clone(),
                        reply: None,
                    },
                );
                let tx = ctx.tx.clone();
                tokio::spawn(async move {
                    let observer = ChannelObserver { id, tx: tx.clone() };
                    let report = filar_transport::copy(
                        &*from.ft,
                        &plan.sources,
                        &*to.ft,
                        &plan.dst_dir,
                        &observer,
                        &cancel,
                    )
                    .await;
                    let _ = tx.send(PanelMsg::CopyDone { id, report });
                    // Sources the panels already let go of close here.
                    from.close();
                    to.close();
                });
            }
            CopyRequest::Cancel { id } => {
                if let Some(task) = copies.get_mut(&id) {
                    task.cancel.cancel();
                    if let Some(reply) = task.reply.take() {
                        let _ = reply.send(filar_transport::OverwriteDecision::Cancel);
                    }
                }
            }
            CopyRequest::Answer { id, decision } => {
                if let Some(reply) = copies.get_mut(&id).and_then(|t| t.reply.take()) {
                    let _ = reply.send(decision);
                }
            }
        }
    }
}

fn fail_open(ctx: &PanelCtx, side: crate::file_panels::Side, generation: u64, error: &str) {
    let _ = ctx.tx.send(PanelMsg::Event(
        crate::file_panels::PanelEvent::OpenFailed {
            side,
            generation,
            error: error.to_string(),
        },
    ));
}

/// Open a panel source in the background, then list its first directory.
fn spawn_panel_open(
    ctx: &PanelCtx,
    side: crate::file_panels::Side,
    generation: u64,
    open: PanelOpen,
    start_dir: Option<String>,
) {
    use crate::file_panels::PanelEvent;
    let tx = ctx.tx.clone();
    let cfg = ssh_transport_config(ctx.command_timeout);
    let host_groups = ctx.host_groups.clone();
    tokio::spawn(async move {
        let opened = open_panel_source(open, &cfg, &host_groups).await;
        let (conn, label, read_only) = match opened {
            PanelOpened::Ready {
                conn,
                label,
                read_only,
            } => (conn, label, read_only),
            PanelOpened::NeedSecret { target, retry } => {
                let _ = tx.send(PanelMsg::Event(PanelEvent::NeedSecret {
                    side,
                    generation,
                    target,
                    retry,
                }));
                return;
            }
            PanelOpened::Failed(error) => {
                let _ = tx.send(PanelMsg::Event(PanelEvent::OpenFailed {
                    side,
                    generation,
                    error,
                }));
                return;
            }
        };
        let ft = conn.ft.clone();
        let style = ft.path_style();
        let _ = tx.send(PanelMsg::Ready {
            side,
            generation,
            conn,
        });
        let _ = tx.send(PanelMsg::Event(PanelEvent::Opened {
            side,
            generation,
            label,
            style,
            read_only,
        }));
        // The wanted directory, else home when it is gone or unreadable.
        let mut listed = None;
        if let Some(dir) =
            start_dir.filter(|d| !d.is_empty() || style == filar_transport::PathStyle::Windows)
        {
            if let Ok(entries) = ft.list_dir(&dir).await {
                listed = Some((dir, Ok(entries)));
            }
        }
        let (path, result) = match listed {
            Some(l) => l,
            None => match ft.home_dir().await {
                Ok(home) => {
                    let r = ft.list_dir(&home).await.map_err(|e| e.to_string());
                    (home, r)
                }
                Err(e) => (String::new(), Err(e.to_string())),
            },
        };
        let _ = tx.send(PanelMsg::Event(PanelEvent::Listed {
            side,
            generation,
            path,
            result,
        }));
    });
}

/// Outcome of opening a panel source.
enum PanelOpened {
    Ready {
        conn: PanelConn,
        label: String,
        read_only: bool,
    },
    NeedSecret {
        target: filar_core::SshTarget,
        retry: bool,
    },
    Failed(String),
}

/// Open a panel source. SSH goes through the same secret sources, host-key
/// check and read-only rule as the tab's own connection.
async fn open_panel_source(
    open: PanelOpen,
    cfg: &SshTransportConfig,
    host_groups: &[filar_core::HostGroup],
) -> PanelOpened {
    let target = match open {
        PanelOpen::Local => {
            return PanelOpened::Ready {
                conn: PanelConn {
                    ft: Arc::new(filar_transport::LocalFileTransfer::new()),
                    sftp: None,
                    read_only: false,
                },
                label: "local".into(),
                read_only: false,
            }
        }
        PanelOpen::TabHost { exec, target } => {
            let target = target.read().await.clone();
            let exec = exec.read().await.clone();
            match (exec, target) {
                (Some(exec), Some(t)) => {
                    return match exec.open_sftp().await {
                        Ok(sftp) => ready_sftp(sftp, &t, host_groups),
                        Err(e) => PanelOpened::Failed(e.to_string()),
                    };
                }
                // Known host, but not its executor (e.g. a restored tab):
                // a connection of the panel's own.
                (None, Some(t)) => match fill_saved_secret(t).await {
                    Ok(SavedSecret::Ready(t)) => t,
                    Ok(SavedSecret::Missing(t)) => {
                        return PanelOpened::NeedSecret {
                            target: t,
                            retry: false,
                        }
                    }
                    Err(e) => return PanelOpened::Failed(e.to_string()),
                },
                _ => return PanelOpened::Failed("this tab is not on an SSH host".into()),
            }
        }
        PanelOpen::Target(t) => match fill_saved_secret(t).await {
            Ok(SavedSecret::Ready(t)) => t,
            Ok(SavedSecret::Missing(t)) => {
                return PanelOpened::NeedSecret {
                    target: t,
                    retry: false,
                }
            }
            Err(e) => return PanelOpened::Failed(e.to_string()),
        },
        PanelOpen::Typed(mut t) => {
            // A typed key passphrase is checked locally first, like Ctrl+O.
            if matches!(t.auth, filar_core::SshAuth::Key { .. })
                && !typed_passphrase_opens_key(&t).await
            {
                if let filar_core::SshAuth::Key { passphrase, .. } = &mut t.auth {
                    *passphrase = None;
                }
                return PanelOpened::NeedSecret {
                    target: t,
                    retry: true,
                };
            }
            t
        }
    };
    match filar_transport::SftpFileTransfer::connect(
        &target,
        cfg,
        &filar_core::EnvSecretProvider::new(),
    )
    .await
    {
        Ok(sftp) => ready_sftp(sftp, &target, host_groups),
        Err(e) => PanelOpened::Failed(e.to_string()),
    }
}

fn ready_sftp(
    sftp: filar_transport::SftpFileTransfer,
    target: &filar_core::SshTarget,
    host_groups: &[filar_core::HostGroup],
) -> PanelOpened {
    let sftp = Arc::new(sftp);
    let read_only = filar_core::is_read_only_target(host_groups, target);
    PanelOpened::Ready {
        conn: PanelConn {
            ft: sftp.clone(),
            sftp: Some(sftp),
            read_only,
        },
        label: target.name.clone(),
        read_only,
    }
}

/// A target with its secret filled from the saved sources, or still missing it.
enum SavedSecret {
    /// Ready to connect.
    Ready(filar_core::SshTarget),
    /// No saved password / key passphrase anywhere: ask the user.
    Missing(filar_core::SshTarget),
}

/// Fill `target`'s secret the way Ctrl+O does — and the file panels (#535),
/// which must connect exactly like it: a password from config, the OS
/// credential store (`ssh_target:<name>`) or `SSH_PASSWORD`; an encrypted
/// key's passphrase from the credential store or `SSH_KEY_PASSPHRASE` (#480).
async fn fill_saved_secret(mut target: filar_core::SshTarget) -> Result<SavedSecret> {
    if let filar_core::SshAuth::Password { ref password } = target.auth {
        let keyring_name = format!("ssh_target:{}", target.name);
        let resolved = password
            .clone()
            .or_else(|| {
                let kr = filar_core::KeyringSecretProvider::new();
                kr.get(&keyring_name).ok().filter(|v| !v.is_empty())
            })
            .or_else(|| std::env::var("SSH_PASSWORD").ok().filter(|v| !v.is_empty()));
        match resolved {
            Some(pw) => {
                if password.is_some() {
                    warn!(target = %target.name, "password in config.toml for SSH target — consider moving to OS credential store");
                }
                target.auth = filar_core::SshAuth::Password { password: Some(pw) };
            }
            None => return Ok(SavedSecret::Missing(target)),
        }
    }
    // An encrypted key: decrypting is blocking.
    if matches!(target.auth, filar_core::SshAuth::Key { .. }) {
        let (t, found) = tokio::task::spawn_blocking(move || {
            let store = filar_core::KeyringSecretProvider::new();
            let env = filar_core::EnvSecretProvider::new();
            filar_transport::fill_key_passphrase(&mut target, &store, Some(&env))
                .map(|found| (target, found))
        })
        .await
        .map_err(|e| CoreError::Other(format!("key check task failed: {e}")))??;
        if matches!(found, filar_transport::KeyPassphrase::Missing(_)) {
            return Ok(SavedSecret::Missing(t));
        }
        target = t;
    }
    Ok(SavedSecret::Ready(target))
}

/// Whether the passphrase typed for `target`'s encrypted key decrypts it
/// (#480). Decrypting is blocking, so it runs off the runtime.
async fn typed_passphrase_opens_key(target: &filar_core::SshTarget) -> bool {
    let filar_core::SshAuth::Key {
        path,
        passphrase: Some(passphrase),
    } = &target.auth
    else {
        return false;
    };
    let path = filar_transport::resolve_key_path(path.as_deref());
    let passphrase = passphrase.clone();
    tokio::task::spawn_blocking(move || filar_transport::key_passphrase_matches(&path, &passphrase))
        .await
        .unwrap_or(false)
}

/// Wrap `exec` in a [`filar_transport::ReadOnlyExecutor`] when `target` is a
/// member of a read-only [`filar_core::HostGroup`] (#419); return it
/// untouched otherwise. Ad-hoc targets carry no tags and never match.
fn wrap_if_read_only(
    exec: Arc<dyn CommandExecutor>,
    target: &filar_core::SshTarget,
    host_groups: &[filar_core::HostGroup],
) -> Arc<dyn CommandExecutor> {
    if filar_core::is_read_only_target(host_groups, target) {
        Arc::new(filar_transport::ReadOnlyExecutor::new(exec))
    } else {
        exec
    }
}

/// How the fleet executor reaches one host (#435): the same SSH transport
/// as a tab, always behind [`filar_transport::ReadOnlyExecutor`] — a fleet
/// is read-only whatever the group's tags say (#419).
///
/// The target arrives with its own password already filled in (#436). The
/// transport gets an empty secret provider, so the shared `SSH_PASSWORD`
/// fallback a tab uses can never be sent to a fleet host.
fn fleet_connector(command_timeout: Duration) -> filar_agent::fleet_exec::HostConnector {
    Arc::new(move |target: filar_core::FleetTarget| {
        Box::pin(async move {
            let filar_core::FleetTarget::Ssh(target) = target else {
                return Err(CoreError::Other(
                    "this connector supports SSH fleet members only".into(),
                ));
            };
            let ssh = SshExecutor::connect_with_provider(
                &target,
                ssh_transport_config(command_timeout),
                Arc::new(StaticSecretProvider::new()),
            )
            .await?;
            Ok(
                Arc::new(filar_transport::ReadOnlyExecutor::new(Arc::new(ssh)))
                    as Arc<dyn CommandExecutor>,
            )
        })
    })
}

/// The group-level executor for the fleet session `sid` (#435).
///
/// Reused only while it was built from this very `fleet` operation: an entry
/// built from any other composition is replaced, so the hosts a command runs
/// on are always the ones the gate shows as the radius — whatever the
/// session lifecycle around it does.
///
/// `credentials` are the ones resolved when the fleet was entered (#436);
/// credentials of any other operation are refused. `observer` receives the
/// person-facing view of every operation, for the side panel (#438).
fn fleet_executor_for(
    map: &mut HashMap<SessionId, Arc<filar_agent::fleet_exec::FleetExecutor>>,
    sid: SessionId,
    fleet: &filar_core::FleetOperation,
    credentials: &filar_agent::fleet_creds::FleetCredentials,
    connector: impl FnOnce() -> filar_agent::fleet_exec::HostConnector,
    observer: impl FnOnce() -> Option<(
        filar_agent::fleet_exec::FleetObserver,
        filar_agent::fleet_exec::FleetStatusObserver,
    )>,
) -> filar_core::Result<Arc<filar_agent::fleet_exec::FleetExecutor>> {
    match map.get(&sid) {
        Some(exec) if exec.built_from() == fleet.id() => Ok(Arc::clone(exec)),
        _ => {
            let mut exec = filar_agent::fleet_exec::FleetExecutor::new(
                fleet,
                credentials.clone(),
                connector(),
            )?;
            if let Some((view, status)) = observer() {
                exec = exec.with_observer(view).with_status(status);
            }
            let exec = Arc::new(exec);
            map.insert(sid, Arc::clone(&exec));
            Ok(exec)
        }
    }
}

/// Run the TUI with the given LLM client, executor, and configuration.
pub async fn run(
    _llm: Arc<dyn LlmClient>,
    executor: Arc<dyn CommandExecutor>,
    config: TuiConfig,
) -> Result<()> {
    // Install panic hook to restore terminal state on panic.
    // The hook is automatically uninstalled when _hook_guard is dropped
    // (on normal return, early error, or panic). The shared snapshot lets
    // the hook persist the last auto-saved session on a crash.
    let snapshot: SessionSnapshot = Arc::new(Mutex::new(None));
    let _hook_guard = PanicHookGuard::install(snapshot.clone());

    // Set up terminal.
    enable_raw_mode().map_err(|e| CoreError::Other(format!("failed to enable raw mode: {e}")))?;
    let mut stdout = io::stdout();
    if let Err(e) = crossterm::execute!(stdout, EnterAlternateScreen) {
        // Restore terminal state before returning the error.
        disable_raw_mode().ok();
        return Err(CoreError::Other(format!(
            "failed to enter alternate screen: {e}"
        )));
    }
    // Mouse capture is optional — degrade gracefully if unsupported.
    if let Err(e) = crossterm::execute!(io::stdout(), EnableMouseCapture) {
        warn!(error = %e, "mouse capture not available — mouse support disabled");
    }
    // Bracketed paste enables Event::Paste for pasting from the system clipboard.
    if let Err(e) = crossterm::execute!(io::stdout(), EnableBracketedPaste) {
        warn!(error = %e, "bracketed paste not supported");
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)
        .map_err(|e| CoreError::Other(format!("failed to create terminal: {e}")))?;

    let result = run_app(&mut terminal, _llm, executor, config, snapshot).await;

    // Restore the original panic hook before terminal teardown.
    // The custom hook is no longer needed — teardown uses .ok() and
    // cannot panic. Removing the hook here avoids a redundant double
    // DisableMouseCapture if the default hook fires during teardown.
    drop(_hook_guard);

    // Restore terminal.
    disable_raw_mode().ok();
    crossterm::execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .ok();

    result
}

/// The main application loop.
async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    _llm: Arc<dyn LlmClient>,
    executor: Arc<dyn CommandExecutor>,
    mut config: TuiConfig,
    snapshot: SessionSnapshot,
) -> Result<()> {
    let command_timeout = config.command_timeout;
    // Read-only wrap decisions need the groups for every session swap (#419),
    // captured before the config is partially consumed below.
    let host_groups = config.host_groups.clone();
    let profiles_for_restore = std::mem::take(&mut config.profiles);
    let default_for_restore = std::mem::take(&mut config.default_profile_name);
    let has_history = !config.initial_messages.is_empty()
        || !config.initial_input_history.is_empty()
        || config
            .initial_llm_profile
            .as_ref()
            .is_some_and(|p| !p.is_empty())
        || config.initial_tokens_in > 0
        || config.initial_tokens_out > 0
        || config.initial_kube.is_some();
    let mut app = if has_history {
        let mut a = App::with_history(
            config.target_name.clone(),
            config.confirm_mode,
            std::mem::take(&mut config.initial_messages),
            std::mem::take(&mut config.initial_input_history),
            std::mem::take(&mut config.initial_llm_profile),
            config.initial_tokens_in,
            config.initial_tokens_out,
            config.initial_cost_usd,
            config.initial_per_profile,
            config.initial_last_served_model,
            std::mem::take(&mut config.initial_model_per_profile),
            &profiles_for_restore,
            &default_for_restore,
        );
        a.profiles = profiles_for_restore;
        a.default_profile_name = default_for_restore;
        a.ssh_targets = config.ssh_targets.clone();
        a.kube_clusters = config.kube_clusters.clone();
        a.save_dir = config.save_dir.clone();
        a
    } else {
        let mut a = App::new(config.target_name.clone(), config.confirm_mode);
        a.profiles = profiles_for_restore;
        a.default_profile_name = default_for_restore;
        a.ssh_targets = config.ssh_targets.clone();
        a.kube_clusters = config.kube_clusters.clone();
        a.save_dir = config.save_dir.clone();
        if let Some(s) = a.sessions.first_mut() {
            s.llm_profile = Some(config.llm_profile.clone());
        }
        a
    };
    // Tag-policy state (#414): the global baseline is the config's own
    // `confirm_mode` (not the possibly session-restored one), and policies
    // come from config only — the GUI has no editor for them.
    app.global_confirm_mode = config.global_confirm_mode;
    app.tag_policies = config.tag_policies.clone();
    // Load available LLM profiles and default profile name.
    app.key_checker = Some(config.key_checker.clone());
    // Wire the App to the same StaticSecretProvider instance used by the
    // agent's SecretSubstitutingExecutor, so Ctrl+P inserts are visible to
    // command substitution and output sanitisation.
    app.secrets = config.secret_provider.clone();
    // Fleet hosts' own passwords come from the OS credential store (#436).
    app.credential_store = Arc::new(filar_core::KeyringSecretProvider::new());
    app.runbook_enabled = config.save_runbook;
    app.risk_scorer_on = config.risk_scorer.is_some();
    app.risk_thresholds = config.risk_thresholds;
    if let Some(note) = config.risk_scorer_note.take() {
        app.push_message(ChatBlock::System(note));
    }
    // Host groups are fleet entry points in Ctrl+O; `--group` opens the fleet
    // layer straight away, over the start-up tab (#432).
    app.host_groups = config.host_groups.clone();
    if let Some(saved) = config.initial_fleet.take() {
        app.restore_fleet(saved);
    } else if let Some(group) = config.initial_group.take() {
        app.enter_fleet(&group);
    }

    // Channel for agent → UI events.
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<TuiEvent>();
    // Live typed Kubernetes APIs stay in the runner, never in persisted state.
    let (kube_tx, mut kube_rx) = mpsc::unbounded_channel::<KubeConnectionResult>();
    let mut kube_apis: HashMap<SessionId, Arc<dyn ClusterApi>> = HashMap::new();
    // Pod tabs (#565): pod lists, shell connections and shell events.
    let (pod_tx, mut pod_rx) = mpsc::unbounded_channel::<PodRunnerEvent>();
    let mut pod_shells: HashMap<SessionId, PodShell> = HashMap::new();
    let (kube_fleet_tx, mut kube_fleet_rx) = mpsc::unbounded_channel::<KubeFleetResult>();
    let mut kube_fleets: HashMap<SessionId, Arc<filar_agent::KubeFleet>> = HashMap::new();

    // Channel for session-save progress (Ctrl+S, #234/#235).
    let (save_tx, mut save_rx) = tokio::sync::mpsc::unbounded_channel::<SaveProgress>();
    // The runner keeps a clone of its own: the runbook task it spawns after a
    // `Done` reports back through this same channel (#401).
    app.save_tx = Some(save_tx.clone());

    // Channel for in-TUI path picker directory listings (#351).
    let (path_picker_tx, mut path_picker_rx) =
        tokio::sync::mpsc::unbounded_channel::<(u64, PathPickerLoadResult)>();
    let path_picker_tx_load = path_picker_tx.clone();
    let mut path_picker_load_in_flight: Option<u64> = None;

    // File panels (Ctrl+G, #535): one open source per panel, answers from
    // the tasks that open and list them.
    let (panel_tx, mut panel_rx) = tokio::sync::mpsc::unbounded_channel::<PanelMsg>();
    let mut panel_sides: [Option<PanelConn>; 2] = [None, None];
    // Copies started from the panels (#536), by copy number.
    let mut copy_tasks: HashMap<u64, CopyTask> = HashMap::new();
    let panel_ctx = PanelCtx {
        tx: panel_tx.clone(),
        command_timeout,
        host_groups: host_groups.clone(),
    };

    // Receiver for WARN/ERROR log lines mirrored into the chat.
    let mut log_rx = config.log_rx.take();

    // Per-session executors. The initial executor (from main.rs) is stored
    // for the start-up session; new tabs get a LocalExecutor created on demand.
    let initial_tui = Arc::new(TuiExecutor {
        inner: Arc::new(tokio::sync::RwLock::new(executor)),
    });
    let initial_sid = app.sessions[0].id;
    // The fleet's group-level executor (#435), one per fleet session. Kept
    // apart from `executors` on purpose: a fleet never has a tab executor,
    // so nothing that looks one up (terminal, cwd sync, host switch) can
    // reach the fleet's hosts through it.
    let mut fleet_executors: HashMap<
        crate::app::SessionId,
        Arc<filar_agent::fleet_exec::FleetExecutor>,
    > = HashMap::new();
    let mut executors: std::collections::HashMap<crate::app::SessionId, ExecutorEntry> =
        std::collections::HashMap::new();
    executors.insert(
        initial_sid,
        ExecutorEntry {
            executor: initial_tui,
            ssh_target: Arc::new(tokio::sync::RwLock::new(config.ssh_target.clone())),
            ssh_exec: Arc::new(tokio::sync::RwLock::new(
                config.initial_ssh_executor.clone(),
            )),
        },
    );
    // Set initial ssh_info on the first session.
    if let Some(ref target) = config.ssh_target {
        app.sessions[0].ssh_info = Some(format!("{}@{}:{}", target.user, target.host, target.port));
        // Do not run unconfirmed `pwd` here (AGENTS.md confirm gate).
        // Pwd appears once OSC 7 reports it (interactive) or #313 syncs it.
        app.sessions[0].cwd = None;
    } else if let Some(ref info) = config.initial_ssh_info {
        // Restored session was over SSH but no live ssh_target was provided
        // (e.g. `--session` restore without `--target`). Surface the saved
        // host in the tab label; reconnecting is handled by the overlay.
        app.sessions[0].ssh_info = Some(info.clone());
        app.sessions[0].cwd = None;
    }
    if let Some(snapshot) = config.initial_kube.take() {
        app.restore_kube_snapshot(snapshot);
    }
    start_pending_kube_connects(&mut app, &mut kube_apis, &kube_tx);
    start_pending_kube_fleets(&mut app, &mut kube_fleets, &kube_fleet_tx);
    start_pending_kube_pod_connects(&mut app, &pod_tx);
    // The startup target may carry policy tags — tighten before the first
    // render so the bar and the confirm gate agree from the start (#414).
    app.sync_confirm_mode();

    // Crossterm event stream for async keyboard input.
    let mut events = EventStream::new();

    // Interactive terminal backends — one per session, keyed by SessionId.
    // Each entry holds the backend Arc and its reader task JoinHandle for
    // lifecycle management.
    let mut interactive_backends: std::collections::HashMap<
        crate::app::SessionId,
        (Arc<dyn InteractiveTerminal>, tokio::task::JoinHandle<()>),
    > = std::collections::HashMap::new();

    // Tagged channel: reader tasks push (SessionId, TermChunk); the event
    // loop receives and routes to the correct session model.
    let (term_tx, term_rx) =
        tokio::sync::mpsc::unbounded_channel::<(crate::app::SessionId, TermChunk)>();
    // Store in Option so we can disable polling when the channel closes
    // (same pattern as log_rx), avoiding a busy-loop.
    let mut term_rx_opt: Option<tokio::sync::mpsc::UnboundedReceiver<_>> = Some(term_rx);

    // Status-bar cwd of POSIX terminals (#499): when to ask each backend
    // where it is, and the answers on their way back.
    // Each watch and each answer names the backend it is about, so a
    // terminal reopened in the same tab starts clean and never takes an
    // answer meant for its predecessor.
    let mut cwd_watches: HashMap<crate::app::SessionId, (Arc<dyn InteractiveTerminal>, CwdWatch)> =
        HashMap::new();
    let (cwd_tx, mut cwd_rx) = tokio::sync::mpsc::unbounded_channel::<(
        crate::app::SessionId,
        Arc<dyn InteractiveTerminal>,
        Option<filar_transport::ForegroundCwd>,
    )>();
    let mut cwd_watch_interval = tokio::time::interval(Duration::from_millis(100));
    cwd_watch_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Draw initial UI.
    terminal.draw(|f| ui::render(f, &mut app)).ok();

    let mut prev_mode = app.mode;
    let mut prev_session = app.sessions[app.active].id;
    let mut needs_redraw = false;
    let mut render_interval = tokio::time::interval(Duration::from_millis(16));
    render_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Track last draw time for forced-frame logic below. The render tick
    // branch in select! can be starved by continuous output (interactive SSH
    // PTY); we draw at the end of the iteration body if a frame is pending
    // and a frame deadline has passed, avoiding competition with read_output.
    let mut last_draw = Instant::now();
    // Settle repaint (#366): one full repaint shortly after output stops.
    //
    // Sanitising command output removes the known source of buffer/screen
    // desync, but ratatui's diff cannot detect a screen that drifted for any
    // other reason — that is precisely why nudging the window used to "fix"
    // the display. This is the safety net: after the last draw settles, issue
    // a single `terminal.clear()` + draw. It costs one extra frame after a
    // burst of output and nothing at all while idle, unlike an unconditional
    // timer, which would repaint forever and keep the process awake (the
    // render tick is deliberately gated so CPU idle stays at zero).
    const SETTLE_DELAY: Duration = Duration::from_millis(250);
    let mut settle_pending = false;

    // Periodic auto-save (#272): one stable id for the whole run so each
    // 30s save overwrites the same file, plus the revision/session of the
    // last saved snapshot so unchanged sessions skip the write.
    let (session_id, session_timestamp) = filar_core::session::now_session_id();
    let mut auto_save_interval = tokio::time::interval(Duration::from_secs(30));
    auto_save_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_saved_rev = app.active_session().message_rev;
    let mut last_saved_session = app.active_session().id;
    // The fleet dialogue has a file of its own (#442), next to the tab's.
    let mut fleet_save = FleetSaveState::default();
    // Side-panel refresh (#431): re-read the background-job registry while a
    // job runs, so local output and completion show up without an agent turn.
    // In-memory only — nothing is sent to a host — and gated on a running
    // job (or a finished one whose output is still draining), so an idle TUI
    // keeps sleeping.
    let mut ops_interval = tokio::time::interval(Duration::from_millis(500));
    ops_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let in_interactive = app.mode == AppMode::Interactive;
        tokio::select! {
            biased;

            Some(msg) = panel_rx.recv() => {
                handle_panel_msg(&mut app, &mut panel_sides, &mut copy_tasks, msg);
                // An answer can queue work (a copy refreshes its target
                // panel): run it now, not on the next key.
                drive_file_panels(&mut app, &mut panel_sides, &executors, &panel_ctx);
                drive_copies(&mut app, &panel_sides, &mut copy_tasks, &panel_ctx);
                needs_redraw = true;
            }

            Some((token, result)) = path_picker_rx.recv() => {
                path_picker_load_in_flight = None;
                if app.path_picker_visible && app.path_picker_load_token == token {
                    match result {
                        Ok((entries, truncated)) => {
                            app.apply_path_picker_load(entries, truncated, None);
                        }
                        Err(e) => {
                            app.apply_path_picker_load(vec![], false, Some(e));
                        }
                    }
                }
                needs_redraw = true;
            }

            // Save progress updates (Ctrl+S, #235). Must be inside the
            // select so the progress bar updates even when the user is idle.
            // The drain runs the same handler as the branch: a `Done` that
            // queued up behind another event still has to start its runbook,
            // and no `TranscriptDone` may be dropped to the drain (#401).
            Some(progress) = save_rx.recv() => {
                apply_save_progress(
                    &mut app,
                    progress,
                    &config.llm_factory,
                    &config.secret_provider,
                    &save_tx,
                );
                while let Ok(p) = save_rx.try_recv() {
                    apply_save_progress(
                        &mut app,
                        p,
                        &config.llm_factory,
                        &config.secret_provider,
                        &save_tx,
                    );
                }
                needs_redraw = true;
            }

            // Periodic auto-save (#272): every 30 seconds, persist the active
            // session if it changed since the last save (message_rev or tab).
            // The write is synchronous but tiny (<100 KB, ~a few ms).
            _ = auto_save_interval.tick() => {
                let changed = session_changed(&app, last_saved_rev, last_saved_session);
                if changed {
                    let session = session_snapshot(
                        &app,
                        &config.target_name,
                        &session_id,
                        &session_timestamp,
                    );
                    match save_session_async(session, snapshot.clone()).await {
                        Ok(()) => {
                            last_saved_rev = app.active_session().message_rev;
                            last_saved_session = app.active_session().id;
                            info!("session auto-saved");
                        }
                        Err(e) => {
                            warn!(error = %e, "session auto-save failed");
                        }
                    }
                }
                if let Some((session, mark)) = fleet_save.due(&app, &session_id, &session_timestamp) {
                    match save_fleet_session_async(session).await {
                        Ok(()) => {
                            fleet_save.saved(mark);
                            info!("fleet session auto-saved");
                        }
                        Err(e) => warn!(error = %e, "fleet session auto-save failed"),
                    }
                }
            }

            // Terminal keyboard / resize event.
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press => {
                        // Note: Ctrl+= / Ctrl+- (terminal font zoom) are NOT
                        // consumed by filar. On Windows Terminal these are
                        // intercepted by the terminal emulator before crossterm
                        // sees them in raw mode; zoom works regardless. In
                        // interactive mode, they are not forwarded to the PTY
                        // because ctrl_key() only maps a-z and a few special
                        // chars (see crates/tui/src/terminal.rs:486-492).
                        app.handle_key(key);
                        needs_redraw = true;
                    }
                    Some(Ok(Event::Resize(cols, rows))) => {
                        let term_cols = cols;
                        let term_rows = ui::interactive_grid_rows(rows);
                        // Resize all session models — even background tabs.
                        resize_all_models(&mut app, term_cols, term_rows);
                        // Resize all live backends.
                        for (sid, (ref term, _)) in interactive_backends.iter() {
                            if let Err(e) = term.resize(term_cols, term_rows).await {
                                warn!(error = %e, sid = ?sid, "terminal resize failed");
                            }
                        }
                        needs_redraw = true;
                    }
                    Some(Ok(Event::Mouse(m))) => {
                        app.handle_mouse(m);
                        needs_redraw = true;
                    }
                    Some(Ok(Event::Paste(text))) => {
                        // Bracketed paste: forward to app's paste handler,
                        // which dispatches by mode (normal/confirm/password).
                        // In Interactive mode it goes to the PTY, re-wrapped
                        // as bracketed paste if the shell enabled it (#494).
                        if app.mode == AppMode::Interactive {
                            app.paste_to_terminal(&text);
                        } else {
                            app.paste_text(&text);
                        }
                        needs_redraw = true;
                    }
                    Some(Ok(_)) => {} // ignore other events
                    Some(Err(e)) => {
                        error!(error = %e, "terminal event error");
                    }
                    None => {} // stream ended
                }

                // In-TUI path picker (#351): open overlay instead of native dialog.
                if let Some(kind) = app.take_pending_path_picker() {
                    app.open_path_picker(kind);
                    needs_redraw = true;
                }

                // Path picker async directory listing.
                if app.path_picker_visible && app.path_picker_loading {
                    let token = app.path_picker_load_token;
                    if path_picker_load_in_flight != Some(token) {
                        path_picker_load_in_flight = Some(token);
                        let dir = app.path_picker_dir.clone();
                        let is_remote = app.sessions[app.active].ssh_info.is_some();
                        let sid = app.sessions[app.active].id;
                        let exec = executors.get(&sid).map(|e| e.executor.clone());
                        let tx = path_picker_tx_load.clone();
                        tokio::spawn(async move {
                            let result = load_path_picker_dir(is_remote, &dir, exec).await;
                            tx.send((token, result)).ok();
                        });
                    }
                }

                // File panels (#535): run their requests; drop the sources
                // once they close.
                drive_file_panels(&mut app, &mut panel_sides, &executors, &panel_ctx);
                drive_copies(&mut app, &panel_sides, &mut copy_tasks, &panel_ctx);

                // Handle mode toggle (Ctrl+T).
                if app.take_toggle_interactive() {
                    let toggle_sid = app.sessions[app.active].id;
                    if in_interactive {
                        // Exit interactive: capture PTY cwd, apply it to the
                        // agent executor, then close the backend.
                        if let Some((term, handle)) = interactive_backends.remove(&toggle_sid) {
                            sync_cwd_from_interactive(
                                &mut app,
                                toggle_sid,
                                &term,
                                &executors,
                                &mut term_rx_opt,
                            )
                            .await;
                            let _ = term.close().await;
                            handle.abort();
                        }
                        app.exit_interactive();
                    } else if interactive_backends.contains_key(&toggle_sid) {
                        // Session already has a live backend — just show its view.
                        app.show_interactive_view();
                    } else if !app.agent_running {
                        // Enter interactive mode.
                        let size = terminal.size().unwrap_or_default();
                        let cols = size.width;
                        let rows = ui::interactive_grid_rows(size.height);
                        // Use the tab's own SSH target, not the global config.
                        let ssh_guard = executors.get(&toggle_sid)
                            .map(|e| e.ssh_target.clone());
                        let ssh_target = match ssh_guard {
                            Some(g) => g.read().await.clone(),
                            None => None,
                        };
                        let mut tab_cwd = app
                            .sessions
                            .iter()
                            .find(|s| s.id == toggle_sid)
                            .and_then(|s| s.cwd.clone());
                        if tab_cwd.is_none() {
                            if let Some(entry) = executors.get(&toggle_sid) {
                                tab_cwd = entry.executor.current_cwd().await;
                                if let Some(ref c) = tab_cwd {
                                    if let Some(idx) = app.find_session_idx(toggle_sid) {
                                        app.sessions[idx].cwd = Some(c.clone());
                                    }
                                }
                            }
                        }
                        // A pod tab's terminal is a second exec in its pod
                        // (#577), never a local PTY under the pod's name.
                        // Only the shell of the tab's current, connected pod:
                        // a retained shell of a previous target never opens
                        // under the new tab's label.
                        let current = app
                            .find_session_idx(toggle_sid)
                            .and_then(|idx| app.sessions[idx].kube.as_ref())
                            .filter(|kube| kube.pod.as_ref().is_some_and(|pod| pod.pod.is_some()))
                            .map(|kube| kube.connection_id);
                        let pod_shell = pod_shells
                            .get(&toggle_sid)
                            .filter(|shell| Some(shell.connection_id) == current)
                            .map(|shell| shell.exec.clone());
                        let in_pod = app
                            .find_session_idx(toggle_sid)
                            .and_then(|idx| app.sessions[idx].kube.as_ref())
                            .is_some_and(|kube| kube.pod.is_some());
                        let term_result: Result<Arc<dyn InteractiveTerminal>> =
                            if let Some(exec) = pod_shell.as_ref() {
                                exec.open_terminal(cols, rows)
                                    .await
                                    .map(|t| Arc::new(t) as Arc<dyn InteractiveTerminal>)
                            } else if in_pod {
                                Err(CoreError::Unavailable("под ещё не подключён".into()))
                            } else if let Some(ref target) = ssh_target {
                                SshInteractive::connect(target, cols, rows)
                                    .await
                                    .map(|t| Arc::new(t) as Arc<dyn InteractiveTerminal>)
                            } else {
                                LocalInteractive::with_size_in(cols, rows, tab_cwd.as_deref())
                                    .await
                                    .map(|t| Arc::new(t) as Arc<dyn InteractiveTerminal>)
                            };
                        match term_result {
                            Ok(term) => {
                                // The agent's cd follows into the terminal, as
                                // over SSH (#493, #577).
                                if ssh_target.is_some() || pod_shell.is_some() {
                                    if let Some(input) = tab_cwd.as_deref().and_then(posix_cd_input)
                                    {
                                        let _ = term.write_input(input.as_bytes()).await;
                                    }
                                }
                                let model = TerminalModel::new(cols, rows);
                                let term_for_read = term.clone();
                                let sid = toggle_sid;
                                let tx = term_tx.clone();
                                let handle = tokio::spawn(async move {
                                    loop {
                                        match term_for_read.read_output().await {
                                            Ok(Some(b)) => {
                                                if tx.send((sid, TermChunk::Bytes(b))).is_err()
                                                {
                                                    break;
                                                }
                                            }
                                            Ok(None) => {
                                                let _ = tx.send((sid, TermChunk::Eof));
                                                break;
                                            }
                                            Err(e) => {
                                                let _ = tx.send((sid, TermChunk::Err(e)));
                                                break;
                                            }
                                        }
                                    }
                                });
                                interactive_backends.insert(toggle_sid, (term, handle));
                                app.enter_interactive(model);
                            }
                            Err(e) => {
                                warn!(error = %e, "failed to start interactive terminal");
                                app.push_error(format!("Failed to start terminal: {e}"));
                            }
                        }
                    }
                }

                // Ctrl+T show of a hidden PTY: carry an agent/`!` cd into it
                // (#493) — before forwarding keys, so keys typed right after
                // the toggle land after the `cd`, not in the middle of it.
                for sid in app.take_pending_cwd_carry() {
                    if let Some((term, _)) = interactive_backends.get(&sid) {
                        let term = term.clone();
                        carry_agent_cwd_into_pty(&mut app, sid, &term).await;
                    }
                }

                // Forward terminal input bytes to the backend.
                if let Some(bytes) = app.take_term_input() {
                    let write_sid = app.sessions[app.active].id;
                    note_pty_input(&mut app, write_sid, &bytes);
                    if let Some((ref term, _)) = interactive_backends.get(&write_sid) {
                        let _ = term.write_input(&bytes).await;
                        // cmd.exe and PowerShell report every prompt (#482);
                        // a POSIX shell is asked once the command settles.
                        if has_enter(&bytes) && !term.reports_cwd_itself() {
                            let now = Instant::now();
                            let fresh = cwd_watches
                                .get(&write_sid)
                                .is_none_or(|(of, _)| !same_backend(of, term));
                            if fresh {
                                cwd_watches.insert(write_sid, (term.clone(), CwdWatch::new(now)));
                            }
                            if let Some((_, watch)) = cwd_watches.get_mut(&write_sid) {
                                watch.note_enter(now);
                            }
                        }
                    }
                }

                // Start cluster or fleet work selected by this key before consuming input.
                start_pending_kube_connects(&mut app, &mut kube_apis, &kube_tx);
                start_pending_kube_fleets(&mut app, &mut kube_fleets, &kube_fleet_tx);
                start_pending_kube_pod_lists(&mut app, &kube_apis, &pod_tx);
                start_pending_kube_pod_connects(&mut app, &pod_tx);

                // Check if user sent input — spawn agent or execute shell escape.
                if let Some(user_input) = app.take_input() {
                    // Cluster tabs run the API agent; a pod tab has a shell
                    // and goes the ordinary way below (#565).
                    if let Some(kube) = app.kube.clone().filter(|kube| kube.pod.is_none()) {
                        if user_input.starts_with('!') {
                            app.push_message(ChatBlock::System(crate::app::KUBE_SHELL_REFUSAL.into()));
                            app.mode = AppMode::Normal;
                            app.agent_running = false;
                        } else {
                            let sid = app.sessions[app.active].id;
                            let Some(api) = kube_apis.get(&sid).cloned() else {
                                app.push_error("Kubernetes cluster is not connected yet".into());
                                app.mode = AppMode::Normal;
                                app.agent_running = false;
                                continue;
                            };
                            let pending_compaction = app.sessions[app.active].pending_compaction;
                            let profile_name = app.sessions[app.active]
                                .llm_profile
                                .as_deref()
                                .unwrap_or(&app.default_profile_name)
                                .to_string();
                            let session_llm = {
                                let profile = app.profiles.iter().find(|p| p.name == profile_name);
                                match profile {
                                    Some(p) => match (config.llm_factory)(p, &config.secret_provider) {
                                        Ok(client) => client,
                                        Err(error) => {
                                            app.push_error(format!("Failed to create LLM client: {error}"));
                                            app.mode = AppMode::Normal;
                                            app.agent_running = false;
                                            continue;
                                        }
                                    },
                                    None => {
                                        app.push_error(format!("Profile '{profile_name}' not found"));
                                        app.mode = AppMode::Normal;
                                        app.agent_running = false;
                                        continue;
                                    }
                                }
                            };
                            let cancel_token = CancellationToken::new();
                            app.cancellation = Some(cancel_token.clone());
                            spawn_kube_agent(
                                session_llm,
                                KubeAgentTarget::Cluster {
                                    api,
                                    target: kube.target,
                                    namespace: kube.namespace,
                                },
                                app.confirm_mode,
                                config.tag_policies.clone(),
                                user_input,
                                app.messages.clone(),
                                app.active_session().history_epoch,
                                agent_tx.clone(),
                                cancel_token,
                                config.secret_provider.clone(),
                                sid,
                                profile_name,
                                app.active_session().compaction_exhausted,
                                pending_compaction,
                            );
                            app.sessions[app.active].pending_compaction = None;
                        }
                    } else if app.in_kube_fleet() {
                        let sid = app.sessions[app.active].id;
                        let Some(fleet) = kube_fleets.get(&sid).cloned() else {
                            app.push_error("Kubernetes fleet is not connected yet".into());
                            app.mode = AppMode::Normal;
                            app.agent_running = false;
                            continue;
                        };
                        let pending_compaction = app.sessions[app.active].pending_compaction;
                        let profile_name = app.sessions[app.active]
                            .llm_profile
                            .as_deref()
                            .unwrap_or(&app.default_profile_name)
                            .to_string();
                        let session_llm = {
                            let profile = app.profiles.iter().find(|p| p.name == profile_name);
                            match profile {
                                Some(p) => match (config.llm_factory)(p, &config.secret_provider) {
                                    Ok(client) => client,
                                    Err(error) => {
                                        app.push_error(format!("Failed to create LLM client: {error}"));
                                        app.mode = AppMode::Normal;
                                        app.agent_running = false;
                                        continue;
                                    }
                                },
                                None => {
                                    app.push_error(format!("Profile '{profile_name}' not found"));
                                    app.mode = AppMode::Normal;
                                    app.agent_running = false;
                                    continue;
                                }
                            }
                        };
                        let cancel_token = CancellationToken::new();
                        app.cancellation = Some(cancel_token.clone());
                        spawn_kube_agent(
                            session_llm,
                            KubeAgentTarget::Fleet(fleet),
                            app.confirm_mode,
                            config.tag_policies.clone(),
                            user_input,
                            app.messages.clone(),
                            app.active_session().history_epoch,
                            agent_tx.clone(),
                            cancel_token,
                            config.secret_provider.clone(),
                            sid,
                            profile_name,
                            app.active_session().compaction_exhausted,
                            pending_compaction,
                        );
                        app.sessions[app.active].pending_compaction = None;
                    } else if let Some(stripped) = user_input.strip_prefix('!') {
                        // Shell escape: execute command directly without agent.
                        let cmd = stripped.trim().to_string();
                        if !cmd.is_empty() {
                            let sid = app.sessions[app.active].id;
                            let exec = match executors.get(&sid) {
                                Some(e) => e.executor.clone() as Arc<dyn CommandExecutor>,
                                None => {
                                    app.push_error("Tab executor not ready yet".into());
                                    continue;
                                }
                            };
                            let provider = config.secret_provider.clone();
                            let sid = app.sessions[app.active].id;
                            let tx = agent_tx.clone();
                            tokio::spawn(async move {
                                let wrapped = SecretSubstitutingExecutor::new(
                                    exec,
                                    provider as Arc<dyn SecretProvider>,
                                );
                                let succeeded = match wrapped.run(&cmd).await {
                                    Ok(result) => {
                                        let mut output = result.stdout.clone();
                                        if !result.stderr.is_empty() {
                                            output.push_str("\n[stderr] ");
                                            output.push_str(&result.stderr);
                                        }
                                        if let Some(code) = result.exit_code {
                                            if code != 0 {
                                                output.push_str(&format!("\n[exit code: {code}]"));
                                            }
                                        }
                                        let _ = tx.send(TuiEvent::Agent {
                                            session_id: sid,
                                            event: filar_agent::AgentEvent::CommandFinished {
                                                command: cmd.clone(),
                                                output,
                                                denied: false,
                                            }
                                        });
                                        true
                                    }
                                    Err(e) => {
                                        let _ = tx.send(TuiEvent::Agent {
                                            session_id: sid,
                                            event: filar_agent::AgentEvent::Error(
                                                format!("Shell command failed: {e}")
                                            )
                                        });
                                        false
                                    }
                                };
                                if succeeded {
                                    let _ = tx.send(TuiEvent::Agent {
                                        session_id: sid,
                                        event: filar_agent::AgentEvent::Finished(String::new())
                                    });
                                }
                            });
                        } else {
                            // Empty command after ! — just return to normal.
                            app.mode = crate::app::AppMode::Normal;
                            app.agent_running = false;
                        }
                    } else {
                        let sid = app.sessions[app.active].id;
                        let in_fleet = app.in_fleet();
                        let session = &app.sessions[app.active];
                        let (agent_exec, is_local, ssh_info) = if let (Some(fleet), Some(creds)) =
                            (session.fleet.as_ref(), session.fleet_credentials.as_ref())
                        {
                            // The fleet agent runs over the group-level
                            // executor and nothing else (#434, #435): a tab
                            // executor would be one host behind a fleet label.
                            let exec = match fleet_executor_for(
                                &mut fleet_executors,
                                sid,
                                fleet,
                                creds,
                                || fleet_connector(command_timeout),
                                // The view goes straight to this fleet's panel,
                                // on the UI's own event — never through the agent.
                                || {
                                    let tx = agent_tx.clone();
                                    let view_tx = tx.clone();
                                    Some((
                                        Arc::new(move |view| {
                                            let _ = view_tx.send(TuiEvent::FleetSummary { session_id: sid, view });
                                        }) as filar_agent::fleet_exec::FleetObserver,
                                        Arc::new(move |status| {
                                            let _ = tx.send(TuiEvent::FleetStatus { session_id: sid, status });
                                        }) as filar_agent::fleet_exec::FleetStatusObserver,
                                    ))
                                },
                            ) {
                                Ok(exec) => exec,
                                Err(e) => {
                                    app.push_error(format!("Fleet not ready: {e}"));
                                    continue;
                                }
                            };
                            let info = format!("fleet {} ({} hosts)", fleet.group_name(), fleet.len());
                            (exec as Arc<dyn CommandExecutor>, false, Some(info))
                        } else if app.in_pod_tab() {
                            // The tab's executor is the pod's `KubeExecutor`;
                            // until it is connected nothing may run (#565).
                            match (executors.get(&sid), app.pod_agent_target()) {
                                (Some(e), Some(_)) => {
                                    (e.executor.clone() as Arc<dyn CommandExecutor>, false, None)
                                }
                                _ => {
                                    app.push_error("Под ещё не подключён".into());
                                    app.mode = AppMode::Normal;
                                    app.agent_running = false;
                                    continue;
                                }
                            }
                        } else {
                            match executors.get(&sid) {
                                Some(e) => {
                                    let info = app.sessions[app.active].ssh_info.clone();
                                    (e.executor.clone() as Arc<dyn CommandExecutor>, info.is_none(), info)
                                }
                                None => {
                                    app.push_error("Tab executor not ready yet".into());
                                    continue;
                                }
                            }
                        };
                        // Create a cancellation token for this agent run.
                        let cancel_token = CancellationToken::new();
                        app.cancellation = Some(cancel_token.clone());
                        // Whether the history must be folded before this
                        // request. Taken here so a stale flag cannot survive
                        // into a later turn if the run fails.
                        let pending_compaction = app.sessions[app.active].pending_compaction;
                        // Resolve the LLM client for this session's profile.
                        let profile_name = app.sessions[app.active]
                            .llm_profile
                            .as_deref()
                            .unwrap_or(&app.default_profile_name)
                            .to_string();
                        let session_llm = {
                            let profile = app.profiles.iter()
                                .find(|p| p.name == profile_name);
                            match profile {
                                Some(p) => match (config.llm_factory)(p, &config.secret_provider) {
                                    Ok(c) => c,
                                    Err(e) => {
                                        app.push_error(format!("Failed to create LLM client: {e}"));
                                        continue;
                                    }
                                },
                                None => {
                                    app.push_error(format!("Profile '{profile_name}' not found"));
                                    continue;
                                }
                            }
                        };
                        let arbiter_cfg = filar_core::Config {
                            arbiter_profile: config.arbiter_profile.clone(),
                            arbiter_enabled: config.arbiter_enabled,
                            ..Default::default()
                        };
                        let (arbiter_profile_name, fallback_msg) = arbiter_cfg
                            .resolve_arbiter_profile(&profile_name, &app.profiles);
                        let arbiter_profile_name = arbiter_profile_name.to_string();
                        if let Some(msg) = fallback_msg {
                            app.push_message(ChatBlock::System(msg));
                        }
                        let (arbiter_llm, arbiter_model_name) = if config.arbiter_enabled {
                            match app.profiles.iter().find(|p| p.name == arbiter_profile_name) {
                                Some(p) => match (config.llm_factory)(p, &config.secret_provider) {
                                    Ok(c) => (Some(c), p.name.clone()),
                                    Err(e) => {
                                        app.push_message(ChatBlock::System(format!(
                                            "Arbiter LLM unavailable ({e}) — confirmation proceeds without audit."
                                        )));
                                        (None, arbiter_profile_name.clone())
                                    }
                                },
                                None => (None, profile_name.clone()),
                            }
                        } else {
                            (None, profile_name.clone())
                        };
                        spawn_agent(
                            session_llm,
                            config.risk_scorer.clone(),
                            arbiter_llm,
                            config.arbiter_enabled,
                            arbiter_model_name,
                            agent_exec,
                            app.confirm_mode,
                            user_input,
                            app.messages.clone(),
                            app.active_session().history_epoch,
                            agent_tx.clone(),
                            is_local,
                            ssh_info,
                            cancel_token,
                            config.secret_provider.clone(),
                            sid,
                            profile_name.clone(),
                            app.active_session().compaction_exhausted,
                            pending_compaction,
                            in_fleet,
                            app.pod_agent_target(),
                        );
                        // Consumed by this run: the summary comes back as an
                        // event, and a leftover flag would fold the history a
                        // second time on the next turn.
                        app.sessions[app.active].pending_compaction = None;
                    }

                    // Ctrl+T changes what's on screen — force immediate redraw.
                    needs_redraw = true;
                }

                // Check if user entered an SSH password — perform connection.
                if let Some(password) = app.pending_ssh_password.take() {
                    if let Some((user, host, port)) = app.pending_ssh.take() {
                        let sid = app.sessions[app.active].id;
                        let tx = agent_tx.clone();
                        let exec_entry = executors.get(&sid)
                            .map(|e| (e.executor.clone(), e.ssh_target.clone(), e.ssh_exec.clone()));
                        // Cancel any previous in-flight attempt for this tab so
                        // a stale connection can't overwrite a newer one (race
                        // between `!ssh` and F3 restore).
                        if let Some(handle) = app.pending_ssh_handle.take() {
                            handle.abort();
                        }
                        if let Some(tok) = app.pending_ssh_cancel.take() {
                            tok.cancel();
                        }
                        let token = CancellationToken::new();
                        app.pending_ssh_cancel = Some(token.clone());
                        // Read-only wrap targets for this session (#419).
                        let host_groups = host_groups.clone();
                        let handle = tokio::spawn(async move {
                            let _ = tx.send(TuiEvent::Thinking);
                            let target = filar_core::SshTarget {
                                name: "dynamic".into(),
                                host: host.clone(),
                                port,
                                user: user.clone(),
                                auth: filar_core::SshAuth::Password {
                                    password: Some(password),
                                },
                                host_key_policy: filar_core::HostKeyPolicy::Tofu,
                                // Ad-hoc `!ssh` connections are not a configured
                                // target, so they carry no tags (#413).
                                tags: Vec::new(),
                            };
                            let new_ssh_info = format!("{user}@{host}:{port}");
                            match SshExecutor::connect_with_config(
                                &target,
                                ssh_transport_config(command_timeout),
                            )
                            .await {
                                Ok(ssh_exec) => {
                                    // A newer attempt superseded this one while
                                    // we were connecting — drop the result.
                                    if token.is_cancelled() {
                                        return;
                                    }
                                    // Swap the executor for this session only.
                                    // Ad-hoc `!ssh` targets carry no tags, so the
                                    // read-only wrap can't fire here — routed
                                    // through the helper regardless (#419).
                                    if let Some((ref exec, ref st, ref se)) = exec_entry {
                                        let ssh_exec = Arc::new(ssh_exec);
                                        let exec_arc: Arc<dyn CommandExecutor> = ssh_exec.clone();
                                        let exec_arc =
                                            wrap_if_read_only(exec_arc, &target, &host_groups);
                                        exec.swap_executor(sid, exec_arc).await;
                                        // Store the SshTarget so Ctrl+T can open a PTY
                                        // on the same host for this tab.
                                        *st.write().await = Some(target.clone());
                                        *se.write().await = Some(ssh_exec);
                                    }
                                    // Notify runner to update per-session info.
                                    let _ = tx.send(TuiEvent::TransportChanged {
                                        session_id: sid,
                                        is_local: false,
                                        ssh_info: Some(new_ssh_info),
                                        alias: None,
                                    });
                                    let _ = tx.send(TuiEvent::Agent {
                                        session_id: sid,
                                        event: filar_agent::AgentEvent::Finished(format!(
                                            "Connected to {user}@{host}:{port} via SSH. \
                                             You are now operating on the remote machine."
                                        ))
                                    });
                                }
                                Err(e) => {
                                    // Don't surface a stale error from an
                                    // attempt that a newer one superseded.
                                    if token.is_cancelled() {
                                        return;
                                    }
                                    let _ = tx.send(TuiEvent::Agent {
                                        session_id: sid,
                                        event: filar_agent::AgentEvent::Error(format!(
                                            "SSH connection failed: {e}"
                                        ))
                                    });
                                }
                            }
                        });
                        app.pending_ssh_handle = Some(handle);
                    }
                }

        // Create local executors for new tabs signalled via new_tab().
        // Before the Ctrl+O connect below: a tab can be created and pointed
        // at a host in the same keypress (drilling into a host from the
        // fleet, #433). Connecting first would find no executor to swap, and
        // the local one created afterwards would run the tab's commands on
        // this machine under the host's name.
        for sid in app.take_pending_local_executors() {
            match filar_transport::LocalExecutor::with_timeout(command_timeout).await {
                Ok(local) => {
                    executors.insert(
                        sid,
                        ExecutorEntry {
                            executor: Arc::new(TuiExecutor {
                                inner: Arc::new(tokio::sync::RwLock::new(Arc::new(local))),
                            }),
                            ssh_target: Arc::new(tokio::sync::RwLock::new(None)),
                            ssh_exec: Arc::new(tokio::sync::RwLock::new(None)),
                        },
                    );
                }
                Err(e) => {
                    warn!(error = %e, "failed to create local executor for sid={sid:?}");
                    if let Some(s) = app.sessions.iter_mut().find(|s| s.id == sid) {
                        s.ssh_info = None;
                    }
                    app.push_error(format!(
                        "Failed to create local executor for new tab: {e}"
                    ));
                    app.pending_local_executors.push(sid);
                }
            }
        }

        // ── Ctrl+O delayed host connection ─────────────────────────
        if app.ctrl_o_needs_connect {
            app.ctrl_o_needs_connect = false;
            // If we have a pending password entry target, use it directly.
            if let (Some(mut target), Some(password)) = (app.ctrl_o_pending_target.take(), app.pending_ssh_password.take()) {
                // What was typed is the key's passphrase for a key target
                // (#480), the password otherwise.
                let is_key = matches!(target.auth, filar_core::SshAuth::Key { .. });
                match &mut target.auth {
                    filar_core::SshAuth::Key { passphrase, .. } => *passphrase = Some(password),
                    auth => *auth = filar_core::SshAuth::Password { password: Some(password) },
                }
                let sid = app.ctrl_o_pending_session_id.take().unwrap_or(app.sessions[app.active].id);
                let exec_entry = executors.get(&sid)
                    .map(|e| (e.executor.clone(), e.ssh_target.clone(), e.ssh_exec.clone()));
                // No executor to swap means nothing would carry the new
                // transport: announcing the host anyway is a wrong-target
                // label (#433). Refuse instead.
                if exec_entry.is_none() {
                    app.push_error("Tab executor not ready yet — host not switched".into());
                    app.settle_pending_swap();
                    app.connect_settled(sid, false);
                    continue;
                }
                let tx = agent_tx.clone();
                let alias = target.name.clone();
                // Read-only wrap targets for this session (#419).
                let host_groups = host_groups.clone();
                if let Some(handle) = app.ctrl_o_handle.take() {
                    handle.abort();
                }
                if let Some(tok) = app.ctrl_o_cancel.take() {
                    tok.cancel();
                }
                let token = CancellationToken::new();
                app.ctrl_o_cancel = Some(token.clone());
                let handle = tokio::spawn(async move {
                    tokio::select! {
                        _ = token.cancelled() => return,
                        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                    }
                    // A typed passphrase is checked locally first: a wrong one
                    // asks again instead of failing the connect (#480).
                    if is_key && !typed_passphrase_opens_key(&target).await {
                        let _ = tx.send(TuiEvent::Agent {
                            session_id: sid,
                            event: filar_agent::AgentEvent::Error(
                                "Wrong passphrase for the SSH key — try again".into(),
                            ),
                        });
                        let _ = tx.send(TuiEvent::PasswordNeeded { session_id: sid, target });
                        return;
                    }
                    let new_info = format!("{}@{}:{}", target.user, target.host, target.port);
                    match SshExecutor::connect_with_config(
                        &target,
                        ssh_transport_config(command_timeout),
                    )
                    .await {
                        Ok(ssh_exec) => {
                            if let Some((ref exec, ref st, ref se)) = exec_entry {
                                let ssh_exec = Arc::new(ssh_exec);
                                let exec_arc: Arc<dyn CommandExecutor> = ssh_exec.clone();
                                let exec_arc =
                                    wrap_if_read_only(exec_arc, &target, &host_groups);
                                exec.swap_executor(sid, exec_arc).await;
                                *st.write().await = Some(target);
                                *se.write().await = Some(ssh_exec);
                            }
                            let _ = tx.send(TuiEvent::TransportChanged {
                                session_id: sid, is_local: false, ssh_info: Some(new_info), alias: Some(alias.clone()),
                            });
                            let how = if is_key { "key" } else { "password" };
                            let _ = tx.send(TuiEvent::Agent {
                                session_id: sid,
                                event: filar_agent::AgentEvent::Finished(format!("Connected to {} ({how})", alias)),
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(TuiEvent::Agent {
                                session_id: sid,
                                event: filar_agent::AgentEvent::Error(format!("SSH connection failed: {e}")),
                            });
                            let _ = tx.send(TuiEvent::TransportSwapFailed { session_id: sid });
                        }
                    }
                });
                app.ctrl_o_handle = Some(handle);
                continue;
            }
            if let Some(handle) = app.ctrl_o_handle.take() {
                handle.abort();
            }
            if let Some(tok) = app.ctrl_o_cancel.take() {
                tok.cancel();
            }
            let token = CancellationToken::new();
            app.ctrl_o_cancel = Some(token.clone());
            let selection = app.ctrl_o_selection;
            let targets = app.ssh_targets.clone();
            // Read-only wrap targets for this session (#419).
            let host_groups = host_groups.clone();
            let sid = app.sessions[app.active].id;
            let exec_entry = executors.get(&sid)
                .map(|e| (e.executor.clone(), e.ssh_target.clone(), e.ssh_exec.clone()));
            if exec_entry.is_none() {
                app.push_error("Tab executor not ready yet — host not switched".into());
                app.settle_pending_swap();
                app.connect_settled(sid, false);
                continue;
            }
            let tx = agent_tx.clone();
            let handle = tokio::spawn(async move {
                tokio::select! {
                    _ = token.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                }
                if let Some(idx) = selection {
                    if idx == 0 {
                        // Switch to local.
                        let local_exec = match filar_transport::LocalExecutor::with_timeout(
                            command_timeout,
                        )
                        .await {
                            Ok(exec) => exec,
                            Err(e) => {
                                let _ = tx.send(TuiEvent::Agent {
                                    session_id: sid,
                                    event: filar_agent::AgentEvent::Error(format!("Failed to create local executor: {e}")),
                                });
                                let _ = tx.send(TuiEvent::TransportSwapFailed { session_id: sid });
                                return;
                            }
                        };
                        if let Some((ref exec, ref st, ref se)) = exec_entry {
                            exec.swap_executor(sid, Arc::new(local_exec) as Arc<dyn CommandExecutor>).await;
                            *st.write().await = None;
                            *se.write().await = None;
                        }
                        let _ = tx.send(TuiEvent::TransportChanged {
                            session_id: sid, is_local: true, ssh_info: None, alias: Some("local".into()),
                        });
                        return;
                    }
                    if let Some(t) = targets.get(idx - 1) {
                        // Saved secrets first: config, OS credential store,
                        // env (#480); ask only when none has it.
                        let target = match fill_saved_secret(t.clone()).await {
                            Ok(SavedSecret::Ready(target)) => target,
                            Ok(SavedSecret::Missing(target)) => {
                                let _ = tx.send(TuiEvent::PasswordNeeded { session_id: sid, target });
                                return;
                            }
                            Err(e) => {
                                let _ = tx.send(TuiEvent::Agent {
                                    session_id: sid,
                                    event: filar_agent::AgentEvent::Error(format!("SSH connection failed: {e}")),
                                });
                                let _ = tx.send(TuiEvent::TransportSwapFailed { session_id: sid });
                                return;
                            }
                        };
                        let new_info = format!("{}@{}:{}", target.user, target.host, target.port);
                        match SshExecutor::connect_with_config(
                            &target,
                            ssh_transport_config(command_timeout),
                        )
                        .await {
                            Ok(ssh_exec) => {
                                if let Some((ref exec, ref st, ref se)) = exec_entry {
                                    let ssh_exec = Arc::new(ssh_exec);
                                    let exec_arc: Arc<dyn CommandExecutor> = ssh_exec.clone();
                                    let exec_arc =
                                        wrap_if_read_only(exec_arc, &target, &host_groups);
                                    exec.swap_executor(sid, exec_arc).await;
                                    *st.write().await = Some(target);
                                    *se.write().await = Some(ssh_exec);
                                }
                                let alias = t.name.clone();
                                let _ = tx.send(TuiEvent::TransportChanged {
                                    session_id: sid, is_local: false, ssh_info: Some(new_info), alias: Some(alias.clone()),
                                });
                                let _ = tx.send(TuiEvent::Agent {
                                    session_id: sid,
                                    event: filar_agent::AgentEvent::Finished(format!("Connected to {}", alias)),
                                });
                            }
                            Err(e) => {
                                let _ = tx.send(TuiEvent::Agent {
                                    session_id: sid,
                                    event: filar_agent::AgentEvent::Error(format!("SSH connection failed: {e}")),
                                });
                                let _ = tx.send(TuiEvent::TransportSwapFailed { session_id: sid });
                            }
                        }
                    }
                }
            });
            app.ctrl_o_handle = Some(handle);
        }

        // Teardown backends for tabs closed via Ctrl+W / close_tab.
        // App only signals the SessionId; runner executes the async close.
        for sid in app.take_closed_ids() {
            if let Some((term, handle)) = interactive_backends.remove(&sid) {
                let _ = term.close().await;
                handle.abort();
            }
            // Release the tab's executor so SSH connections don't leak.
            executors.remove(&sid);
            // A closed fleet drops its group-level executor and with it
            // every host connection it held (#435).
            fleet_executors.remove(&sid);
            kube_apis.remove(&sid);
            kube_fleets.remove(&sid);
            if let Some(shell) = pod_shells.remove(&sid) {
                shell.release().await;
            }
        }

        // A pod shell outlives its pod tab when the tab went back to its
        // cluster (Ctrl+O, failed reconnect) or to SSH/local (#565): close
        // it. A cluster tab gets a local placeholder executor back — it never
        // runs commands, but must not keep the pod's exec stream open.
        let stale_shells: Vec<SessionId> = pod_shells
            .keys()
            .copied()
            .filter(|sid| {
                app.find_session_idx(*sid)
                    .and_then(|idx| app.sessions[idx].kube.as_ref())
                    .is_none_or(|kube| kube.pod.is_none())
            })
            .collect();
        for sid in stale_shells {
            let back_to_cluster = app
                .find_session_idx(sid)
                .is_some_and(|idx| app.sessions[idx].kube.is_some());
            if back_to_cluster {
                if let Some(entry) = executors.get(&sid) {
                    match filar_transport::LocalExecutor::with_timeout(command_timeout).await {
                        Ok(local) => entry.executor.swap_executor(sid, Arc::new(local)).await,
                        // A cluster tab never runs commands through its
                        // executor (`!` is refused, the agent uses the API),
                        // so a closed pod shell left behind is inert.
                        Err(error) => warn!(error = %error, "no local placeholder for a cluster tab"),
                    }
                }
            }
            if let Some(shell) = pod_shells.remove(&sid) {
                shell.release().await;
            }
        }

        // Teardown interactive backends for tabs reset by session restore
        // (F3) — the tab stays open, but its PTY/SSH reader must stop.
        for sid in app.take_pending_term_teardown() {
            if let Some((term, handle)) = interactive_backends.remove(&sid) {
                let _ = term.close().await;
                handle.abort();
            }
        }

        // Ctrl+T hide: refresh session.cwd + agent executor from the live PTY
        // without tearing it down (#338).
        for sid in app.take_pending_cwd_sync() {
            if let Some((term, _)) = interactive_backends.get(&sid) {
                let term = term.clone();
                sync_cwd_from_interactive(
                    &mut app,
                    sid,
                    &term,
                    &executors,
                    &mut term_rx_opt,
                )
                .await;
                needs_redraw = true;
            }
        }


        if app.should_quit {
                    break;
                }
            }

            // Cluster connection and namespace discovery results (#575).
            maybe_kube = kube_rx.recv() => {
                if let Some(completed) = maybe_kube {
                    match completed.result {
                        Ok((api, namespaces)) => {
                            if app.kube_connection_is_current(
                                completed.session_id,
                                completed.connection_id,
                            ) {
                                kube_apis.insert(completed.session_id, api);
                                app.complete_kube_connection(
                                    completed.session_id,
                                    completed.connection_id,
                                    namespaces,
                                );
                                needs_redraw = true;
                            }
                        }
                        Err(error) => {
                            app.fail_kube_connection(
                                completed.session_id,
                                completed.connection_id,
                                error,
                            );
                            needs_redraw = true;
                        }
                    }
                }
            }

            // Pod tabs (#565): chooser lists, shell connections, pod changes.
            maybe_pod = pod_rx.recv() => {
                if let Some(event) = maybe_pod {
                    match event {
                        PodRunnerEvent::PodList { session_id, select_id, result } => {
                            app.complete_pod_list(session_id, select_id, result);
                        }
                        PodRunnerEvent::Connected { session_id, connection_id, exec } => {
                            if !app.kube_pod_connection_is_current(session_id, connection_id) {
                                // The tab moved on (closed, other target): drop it.
                                let _ = exec.close().await;
                            } else if let Some(entry) = executors.get(&session_id) {
                                entry
                                    .executor
                                    .swap_executor(session_id, exec.clone() as Arc<dyn CommandExecutor>)
                                    .await;
                                *entry.ssh_target.write().await = None;
                                *entry.ssh_exec.write().await = None;
                                let events =
                                    forward_pod_events(&exec, session_id, connection_id, pod_tx.clone());
                                let previous = pod_shells
                                    .insert(session_id, PodShell { exec: exec.clone(), connection_id, events });
                                if let Some(previous) = previous {
                                    previous.release().await;
                                }
                                app.complete_kube_pod_connection(
                                    session_id,
                                    connection_id,
                                    exec.pod(),
                                    exec.container(),
                                    exec.pod_limits(),
                                );
                            } else {
                                let _ = exec.close().await;
                                app.fail_kube_pod_connection(
                                    session_id,
                                    connection_id,
                                    "у вкладки нет исполнителя".into(),
                                );
                            }
                        }
                        PodRunnerEvent::Failed { session_id, connection_id, error } => {
                            app.fail_kube_pod_connection(session_id, connection_id, error);
                        }
                        PodRunnerEvent::Shell { session_id, connection_id, event } => match event {
                            filar_transport::KubeExecEvent::PodChanged { old, new } => {
                                // Another pod: its writable dirs are its own (#599).
                                if app.kube_pod_changed(session_id, connection_id, &old, &new) {
                                    filar_agent::background::reset_log_dir(&session_id.0.to_string());
                                }
                            }
                            filar_transport::KubeExecEvent::Connected { pod, container, limits } => {
                                // A reconnect is a fresh shell, even in the same pod (#599).
                                if app.kube_pod_identity(session_id, connection_id, pod, container, limits) {
                                    filar_agent::background::reset_log_dir(&session_id.0.to_string());
                                }
                            }
                        },
                    }
                    needs_redraw = true;
                }
            }

            // Kubernetes fleet discovery and restored API binding results (#569).
            maybe_kube_fleet = kube_fleet_rx.recv() => {
                if let Some(completed) = maybe_kube_fleet {
                    match completed.result {
                        Ok(fleet) => {
                            if app.kube_fleet_connection_is_current(
                                completed.session_id,
                                completed.connection_id,
                            ) {
                                let operation = fleet.fleet().clone();
                                kube_fleets.insert(completed.session_id, fleet);
                                app.complete_kube_fleet(
                                    completed.session_id,
                                    completed.connection_id,
                                    &operation,
                                );
                                needs_redraw = true;
                            }
                        }
                        Err(error) => {
                            app.fail_kube_fleet(
                                completed.session_id,
                                completed.connection_id,
                                error,
                            );
                            needs_redraw = true;
                        }
                    }
                }
            }

            // Agent event (only when not in interactive mode).
            maybe_agent_event = async {
                if in_interactive {
                    std::future::pending::<Option<TuiEvent>>().await
                } else {
                    agent_rx.recv().await
                }
            } => {
                if let Some(event) = maybe_agent_event {
                    // Intercept TransportChanged to update per-session info.
                    if let TuiEvent::TransportChanged { session_id, is_local, ref ssh_info, ref alias, .. } = &event {
                        kube_apis.remove(session_id);
                        if app.kube_namespace_select.as_ref().is_some_and(|select| {
                            select.session_id == *session_id
                        }) {
                            app.kube_namespace_select = None;
                        }
                        if let Some(idx) = app.find_session_idx(*session_id) {
                            app.sessions[idx].kube = None;
                            app.sessions[idx].ssh_info = ssh_info.clone();
                            app.sessions[idx].target_name =
                                alias.clone().unwrap_or_else(|| {
                                    ssh_info.clone().unwrap_or_else(|| {
                                        format!("local-{}", idx + 1)
                                    })
                                });
                            if *is_local {
                                app.sessions[idx].cwd = std::env::current_dir()
                                    .ok()
                                    .map(|p| p.display().to_string());
                            } else {
                                // Unknown until OSC 7 / command marker / #313 sync.
                                app.sessions[idx].cwd = None;
                            }
                        }
                        // The actual swap happened: the pending target's
                        // floor (if any) settles here — only now may the
                        // mode release, recomputed from the transport that
                        // is actually active (#414 review).
                        app.settle_pending_swap();
                        app.connect_settled(*session_id, true);
                    }
                    if let TuiEvent::TransportSwapFailed { session_id } = &event {
                        // The connect died without a swap: stop folding the
                        // abandoned target's floor (#414 review), and give
                        // the tab back the label of where it still runs.
                        app.settle_pending_swap();
                        app.connect_settled(*session_id, false);
                    }
                    if let TuiEvent::Agent {
                        session_id,
                        event: filar_agent::AgentEvent::CommandFinished { denied: false, .. },
                    } = &event
                    {
                        if let Some(entry) = executors.get(session_id) {
                            if let Some(cwd) = entry.executor.current_cwd().await {
                                if let Some(idx) = app.find_session_idx(*session_id) {
                                    app.sessions[idx].cwd = Some(cwd);
                                }
                            }
                        }
                    }
                    // All agent events just need a redraw — the borderless
                    // layout handles transitions cleanly without full clear.
                    // Full clear is only needed on mode change (see below).
                    // A streamed token cannot start, poll or cancel a job;
                    // skip the registry read for it.
                    let may_touch_jobs = !matches!(
                        &event,
                        TuiEvent::Agent { event: filar_agent::AgentEvent::TextDelta(_), .. }
                    );
                    app.handle_agent_event(event);
                    // A background-job tool call may have started, polled or
                    // cancelled a job: pick that up for the side panel.
                    if may_touch_jobs {
                        app.refresh_operations();
                    }
                    needs_redraw = true;
                }
            }

            // Ask the terminals whose command has settled where they are
            // (#499). Gated on an armed watch, so an idle TUI keeps sleeping.
            _ = cwd_watch_interval.tick(), if cwd_watches.values().any(|(_, w)| w.armed) => {
                let now = Instant::now();
                // A watch outlives neither its terminal nor its replacement.
                cwd_watches.retain(|sid, (of, _)| {
                    interactive_backends.get(sid).is_some_and(|(term, _)| same_backend(of, term))
                });
                for (sid, (term, watch)) in cwd_watches.iter_mut() {
                    if !watch.due(now) {
                        continue;
                    }
                    watch.asked(now);
                    let (term, tx, sid) = (term.clone(), cwd_tx.clone(), *sid);
                    tokio::spawn(async move {
                        let answer = term.foreground_cwd().await;
                        let _ = tx.send((sid, term, answer));
                    });
                }
            }

            Some((sid, from, answer)) = cwd_rx.recv() => {
                // Only the backend that was asked may answer: one closed or
                // replaced meanwhile has no say over the tab's cwd.
                let current = interactive_backends
                    .get(&sid)
                    .is_some_and(|(term, _)| same_backend(term, &from));
                if current {
                    if let Some((of, watch)) = cwd_watches.get_mut(&sid) {
                        if same_backend(of, &from) {
                            watch.answered(answer.as_ref().map(|a| a.at_prompt));
                        }
                    }
                    if let Some(answer) = answer {
                        needs_redraw |= apply_foreground_cwd(&mut app, sid, answer.path);
                    }
                }
            }

            _ = ops_interval.tick(), if app.operations_need_refresh() => {
                if app.refresh_operations() {
                    needs_redraw = true;
                }
            }

            // WARN/ERROR log line forwarded from the tracing subscriber.
            // Polled in every mode so disconnects during interactive sessions
            // still surface once the user returns to the chat. `recv_log_line`
            // disables further polling once the channel closes.
            maybe_log = recv_log_line(&mut log_rx) => {
                if let Some(line) = maybe_log {
                    app.push_system_log(line);
                    needs_redraw = true;
                }
            }

            // Terminal output from reader tasks (all sessions — including
            // background tabs). Each chunk carries its own SessionId so
            // routing works correctly regardless of the active tab.
            // When the channel closes (all senders dropped), switch to
            // pending to avoid a busy-loop — same pattern as recv_log_line.
            maybe_chunk = recv_term_chunk(&mut term_rx_opt) => {
                if let Some((sid, chunk)) = maybe_chunk {
                    let outcome = route_term_chunk(&mut app, sid, chunk);
                    if matches!(outcome, RouteOutcome::Fed) {
                        if let Some((_, watch)) = cwd_watches.get_mut(&sid) {
                            watch.note_output(Instant::now());
                        }
                    }
                    match outcome {
                        RouteOutcome::Eof => {
                            if let Some((term, handle)) = interactive_backends.remove(&sid) {
                                let _ = term.close().await;
                                handle.abort();
                            }
                            // Clear the terminal model for the dying session.
                            if let Some(s) = app.sessions.iter_mut().find(|s| s.id == sid) {
                                s.terminal = None;
                            }
                            if app.sessions.get(app.active).map(|s| s.id) == Some(sid)
                                && app.mode == AppMode::Interactive
                            {
                                app.exit_interactive();
                            }
                        }
                        RouteOutcome::Error(e) => {
                            error!(error = %e, "terminal read error, sid={sid:?}");
                            if let Some((term, handle)) = interactive_backends.remove(&sid) {
                                let _ = term.close().await;
                                handle.abort();
                            }
                            if let Some(s) = app.sessions.iter_mut().find(|s| s.id == sid) {
                                s.terminal = None;
                            }
                            if app.sessions.get(app.active).map(|s| s.id) == Some(sid)
                                && app.mode == AppMode::Interactive
                            {
                                app.exit_interactive();
                            }
                        }
                        _ => {}
                    }
                    needs_redraw = true;
                }
            }

            // Render at most 60fps — batches multiple events into one draw.
            // Also tick when in Thinking mode so the spinner animates, while
            // a toast is pending so it disappears on its own timer (~1.5s)
            // without requiring further input, and while the save overlay's
            // runbook bar animates (#409) so its indeterminate window keeps
            // moving.
            //
            // NB: the guard tests `app.toast.is_some()` (the field), not
            // `toast_text()` (which already applies the expiry). Gating on
            // `toast_text()` — as the issue text literally suggests — would stop
            // ticking the instant the toast expires, so the frame that *erases*
            // the toast would never be drawn and it would linger until the next
            // input. Instead we keep ticking while the field is set, and drop the
            // expired toast right after the erasing draw below; the next tick's
            // guard is then false and ticking stops (CPU idle stays at zero).
            _ = render_interval.tick(), if needs_redraw
                || app.mode == AppMode::Thinking
                || app.toast.is_some()
                || app.runbook_bar_animating() => {
                let tab_changed = prev_session != app.sessions[app.active].id;
                if app.mode == AppMode::Thinking || app.runbook_bar_animating() {
                    app.tick = app.tick.wrapping_add(1);
                }
                if prev_mode != app.mode || tab_changed {
                    terminal.clear().ok();
                    prev_mode = app.mode;
                }
                if tab_changed {
                    prev_session = app.sessions[app.active].id;
                }
                terminal.draw(|f| ui::render(f, &mut app)).ok();
                needs_redraw = false;
                last_draw = Instant::now();
                // Arm the settle repaint only while output is actually
                // streaming (agent running commands). Arming it after every
                // frame would repaint the whole screen after each typing
                // pause — a visible flash over SSH, for no benefit: plain
                // keystroke rendering does not desync the screen.
                if app.mode == AppMode::Thinking {
                    settle_pending = true;
                }
                // Clear an expired toast so the next tick's guard goes false and
                // ticking stops after this erasing frame.
                if app.toast_text().is_none() {
                    app.toast = None;
                }
            }

            // Settle repaint (#366). Fires once, SETTLE_DELAY after the last
            // frame, and only when no newer frame arrived in between — while
            // output streams, draws keep resetting `last_draw` and this branch
            // never becomes ready. `settle_pending` is cleared here, so a quiet
            // session performs exactly one repaint and then stops: the guard is
            // false afterwards and the process goes back to sleep.
            _ = tokio::time::sleep(SETTLE_DELAY.saturating_sub(last_draw.elapsed())),
                if settle_pending && !app.should_quit => {
                terminal.clear().ok();
                terminal.draw(|f| ui::render(f, &mut app)).ok();
                settle_pending = false;
                last_draw = Instant::now();
                render_interval.reset();
            }
        }

        // Non-terminal events can also queue a restored or selected target.
        start_pending_kube_connects(&mut app, &mut kube_apis, &kube_tx);
        start_pending_kube_fleets(&mut app, &mut kube_fleets, &kube_fleet_tx);
        start_pending_kube_pod_lists(&mut app, &kube_apis, &pod_tx);
        start_pending_kube_pod_connects(&mut app, &pod_tx);

        if app.should_quit {
            break;
        }

        // Force a draw after the iteration if a frame is pending and the
        // frame deadline (16 ms) has passed, regardless of which select!
        // branch was chosen. This decouples redraw from branch competition:
        // continuous output from an SSH interactive PTY starves the render
        // tick because read_output always resolves first; without this
        // fallback draw the screen would only update on key/resize events.
        //
        // The render tick above still batches updates (< 16 ms intervals),
        // so 60 fps batching in Normal/Thinking is preserved.
        if needs_redraw && last_draw.elapsed() >= Duration::from_millis(16) {
            if app.mode == AppMode::Thinking {
                app.tick = app.tick.wrapping_add(1);
            }
            if prev_mode != app.mode || prev_session != app.sessions[app.active].id {
                terminal.clear().ok();
                prev_mode = app.mode;
                prev_session = app.sessions[app.active].id;
            }
            terminal.draw(|f| ui::render(f, &mut app)).ok();
            needs_redraw = false;
            last_draw = Instant::now();
            // Same condition as the render tick: only a run producing output
            // needs the settle repaint. This fallback also fires for ordinary
            // keystroke redraws when the 16 ms deadline has passed, and arming
            // it there would clear and repaint the screen after every typing
            // pause (#373 review).
            if app.mode == AppMode::Thinking {
                settle_pending = true;
            }
            // Prevent the normal render tick from firing immediately on
            // the next iteration — the frame we just drew is current.
            render_interval.reset();
            if app.toast_text().is_none() {
                app.toast = None;
            }
        }
    }

    // Clean up interactive terminals — close all remaining backends, abort readers.
    for (_, (term, handle)) in interactive_backends.drain() {
        let _ = term.close().await;
        handle.abort();
    }

    // The fleet dialogue goes to its own file (#442), before the layer is
    // left below.
    if let Some((session, _)) = fleet_save.due(&app, &session_id, &session_timestamp) {
        if let Err(e) = save_fleet_session_async(session).await {
            eprintln!("\nFailed to save fleet session: {e}");
        }
    }

    // Save session to disk for future restore — an ordinary tab, never the
    // fleet dialogue (#432).
    app.leave_fleet_view();
    let session = session_snapshot(&app, &config.target_name, &session_id, &session_timestamp);
    let msg_count = session.messages.len();
    match save_session_async(session, snapshot.clone()).await {
        Ok(()) => {
            eprintln!("\nSession saved ({msg_count} messages).");
        }
        Err(e) => {
            eprintln!("\nFailed to save session: {e}");
        }
    }

    info!("TUI session ended");
    Ok(())
}

/// Persist an already-built session snapshot off the event loop, refresh the
/// shared panic-safe snapshot, and prune old sessions. The file write runs on
/// a blocking thread and is serialised with the panic hook via the shared
/// mutex (see [`PanicHookGuard`]).
async fn save_session_async(
    session: filar_core::Session,
    snapshot: SessionSnapshot,
) -> std::result::Result<(), CoreError> {
    tokio::task::spawn_blocking(move || {
        let store = filar_core::SessionStore::with_default_dir()?;
        let mut guard = snapshot.lock().unwrap_or_else(|e| e.into_inner());
        store.save(&session)?;
        let _ = store.prune_to(filar_core::session::MAX_SESSIONS);
        *guard = Some(session);
        Ok(())
    })
    .await
    .map_err(|e| CoreError::Other(format!("session save task panicked: {e}")))?
}

/// Build a serialisable [`filar_core::Session`] snapshot from the active TUI
/// session, including launch context (ssh_info, model, api_base_url,
/// confirm_mode) so a later restore can re-select the same host and model.
/// Build the persisted snapshot of the active session: its compacted context,
/// the heads compaction folded away, and the launch metadata that goes with
/// them.
pub(crate) fn session_snapshot(
    app: &App,
    target_name: &str,
    id: &str,
    timestamp: &str,
) -> filar_core::Session {
    session_snapshot_of(app, app.active_session(), target_name, id, timestamp)
}

/// Snapshot of the fleet session, if one is open (#442): the same fields as
/// an ordinary tab plus the frozen composition, so a restore rebuilds the
/// fleet from the hosts it had rather than from today's tags. No ssh_info —
/// a fleet has no single host.
pub(crate) fn fleet_session_snapshot(
    app: &App,
    id: &str,
    timestamp: &str,
) -> Option<filar_core::Session> {
    let fleet = &app.sessions[app.fleet_index()?];
    let mut session = session_snapshot_of(app, fleet, &fleet.target_name, id, timestamp);
    session.ssh_info = None;
    Some(session)
}

/// Which fleet session was last written, and under which file id (#442).
///
/// A run has one id for its tab session; each fleet opened during the run
/// gets its own `{run id}-fleet{n}`, so a second fleet does not overwrite
/// the first one's file and the ids still sort by time.
#[derive(Default)]
struct FleetSaveState {
    /// The fleet session the current file id belongs to, and that id.
    file: Option<(crate::app::SessionId, String)>,
    /// Revision of that session at its last successful save.
    saved_rev: Option<u64>,
    /// Fleets seen so far in this run.
    count: u32,
}

impl FleetSaveState {
    /// The open fleet's snapshot, when it has something worth restoring and
    /// changed since its last save, plus the mark to pass to [`saved`].
    ///
    /// A fleet nobody asked anything yet is only its intro lines; saving it
    /// would fill F3 with empty entries.
    ///
    /// [`saved`]: Self::saved
    fn due(
        &mut self,
        app: &App,
        run_id: &str,
        timestamp: &str,
    ) -> Option<(filar_core::Session, u64)> {
        let fleet = &app.sessions[app.fleet_index()?];
        if !fleet
            .messages
            .iter()
            .any(|b| matches!(b, ChatBlock::User(_)))
        {
            return None;
        }
        if self.file.as_ref().map(|(sid, _)| *sid) != Some(fleet.id) {
            // A restored fleet writes back to the file it came from, so a
            // restore does not add a copy per run (review).
            let id = match &fleet.fleet_saved_id {
                Some(id) => id.clone(),
                None => {
                    self.count += 1;
                    format!("{run_id}-fleet{}", self.count)
                }
            };
            self.file = Some((fleet.id, id));
            self.saved_rev = None;
        }
        if self.saved_rev == Some(fleet.message_rev) {
            return None;
        }
        let id = self.file.as_ref().map(|(_, id)| id.clone())?;
        let session = fleet_session_snapshot(app, &id, timestamp)?;
        Some((session, fleet.message_rev))
    }

    /// Record that the snapshot `due` handed out with `rev` is on disk.
    fn saved(&mut self, rev: u64) {
        self.saved_rev = Some(rev);
    }
}

/// Write a fleet session off the event loop (#442). Not the panic-safe
/// snapshot: that one slot belongs to the tab session.
async fn save_fleet_session_async(
    session: filar_core::Session,
) -> std::result::Result<(), CoreError> {
    tokio::task::spawn_blocking(move || {
        let store = filar_core::SessionStore::with_default_dir()?;
        store.save(&session)?;
        let _ = store.prune_to(filar_core::session::MAX_SESSIONS);
        Ok(())
    })
    .await
    .map_err(|e| CoreError::Other(format!("fleet session save task panicked: {e}")))?
}

/// Build the persisted snapshot of one TUI session.
fn session_snapshot_of(
    app: &App,
    tab: &crate::app::Session,
    target_name: &str,
    id: &str,
    timestamp: &str,
) -> filar_core::Session {
    let active_profile_name = tab
        .llm_profile
        .clone()
        .unwrap_or_else(|| app.default_profile_name.clone());
    let (model, api_base_url) = app
        .profiles
        .iter()
        .find(|p| p.name == active_profile_name)
        .map(|p| (Some(p.model.clone()), Some(p.api_base_url.clone())))
        .unwrap_or((None, None));
    let mut session = filar_core::Session {
        id: id.to_string(),
        timestamp: timestamp.to_string(),
        target: target_name.to_string(),
        llm_profile: tab.llm_profile.clone(),
        messages: tab.messages.clone(),
        // Persisted separately from `messages` so a reopened session keeps the
        // context compaction left it with, while still holding every turn it
        // ever had (#379).
        folded_history: tab.folded_history.clone(),
        input_history: tab.input_history().to_vec(),
        tokens_in: tab.tokens_in,
        tokens_out: tab.tokens_out,
        cost_usd: tab.cost_usd,
        per_profile: tab.per_profile.clone(),
        last_served_model: tab.last_served_model.clone(),
        model_per_profile: tab.model_per_profile.clone(),
        ssh_info: tab.ssh_info.clone(),
        model,
        api_base_url,
        confirm_mode: Some(tab.confirm_mode),
        fleet: tab.fleet_snapshot(),
        kube: tab
            .kube
            .as_ref()
            .map(|kube| filar_core::KubeSessionSnapshot {
                target: kube.target.name.clone(),
                namespace: kube.namespace.clone(),
                pod: kube.pod.as_ref().map(App::pod_snapshot),
            }),
    };
    session.truncate_history();
    session
}

/// Whether the active session changed since the last auto-save: either its
/// message revision advanced or the user switched to a different tab.
fn session_changed(app: &App, last_rev: u64, last_session: crate::app::SessionId) -> bool {
    // The fleet dialogue is not a restorable single-host session (#432):
    // saving it here would overwrite the tab's own save with it.
    if app.in_fleet() {
        return false;
    }
    app.active_session().message_rev != last_rev || app.active_session().id != last_session
}

/// Await the next forwarded log line from the optional receiver.
///
/// Returns the next line, or `None` when the channel has closed. On closure it
/// also sets `log_rx` to `None` so the caller's `select!` branch stops polling
/// — otherwise a closed [`mpsc::UnboundedReceiver`] would resolve immediately
/// forever and spin the event loop at 100% CPU. When `log_rx` is already
/// `None`, this future stays pending (the branch is effectively disabled).
async fn recv_log_line(log_rx: &mut Option<mpsc::UnboundedReceiver<String>>) -> Option<String> {
    let line = match log_rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending::<Option<String>>().await,
    };
    if line.is_none() {
        // Channel closed: disable further polling.
        *log_rx = None;
    }
    line
}

/// Receive a terminal chunk from the tagged channel, disabling polling
/// when the channel closes to avoid a busy-loop (same pattern as `recv_log_line`).
async fn recv_term_chunk(
    rx_opt: &mut Option<mpsc::UnboundedReceiver<(SessionId, TermChunk)>>,
) -> Option<(SessionId, TermChunk)> {
    let chunk = match rx_opt.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending::<Option<(SessionId, TermChunk)>>().await,
    };
    if chunk.is_none() {
        *rx_opt = None;
    }
    chunk
}

/// Resize every session's terminal model — both active and background tabs.
pub fn resize_all_models(app: &mut App, cols: u16, rows: u16) {
    for session in app.sessions.iter_mut() {
        if let Some(ref mut model) = session.terminal {
            model.resize(cols, rows);
        }
    }
}

/// Flatten the chat history into the messages sent to the model.
///
/// Extracted from `spawn_agent` so the mapping can be tested directly: whether
/// a block reaches the model is a correctness property, not a detail. Note the
/// two deliberate asymmetries — `System` blocks are feed-only chrome and are
/// dropped, while `Summary` blocks stand in for the turns they replaced and
/// must be sent (#377).
///
/// The agent's commands are replayed as what they were: an assistant message
/// carrying `tool_calls` (with the turn's preamble text as its content), then
/// one `tool` message per call (#525). Writing them as assistant text —
/// `Command: …\nOutput: …` — taught the model to "run" commands by typing
/// that text, and splitting the preamble from its calls taught it to stop
/// after "Let me check:". A command the user ran directly (`!cmd`) is the
/// user's action and is replayed as a user message.
fn history_to_messages(blocks: &[ChatBlock]) -> Vec<filar_agent::ChatMessage> {
    let mut out = Vec::new();
    let mut next_id = 0usize;
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            ChatBlock::User(text) => out.push(filar_agent::ChatMessage::user(text)),
            ChatBlock::Agent(text) => {
                let end = agent_command_run_end(blocks, i + 1);
                if end > i + 1 {
                    push_tool_turn(&mut out, text, &blocks[i + 1..end], &mut next_id);
                    i = end;
                    continue;
                }
                out.push(filar_agent::ChatMessage::assistant(text));
            }
            block @ ChatBlock::Command { .. } if is_agent_command(block) => {
                let end = agent_command_run_end(blocks, i);
                push_tool_turn(&mut out, "", &blocks[i..end], &mut next_id);
                i = end;
                continue;
            }
            ChatBlock::Command {
                command, output, ..
            } => {
                let output_text = output.as_deref().unwrap_or("(no output)");
                out.push(filar_agent::ChatMessage::user(format!(
                    "[filar] I ran this command myself:\n$ {command}\n{output_text}"
                )));
            }
            ChatBlock::Error(text) => out.push(filar_agent::ChatMessage::user(format!(
                "[filar] The previous request ended with an error: {text}"
            ))),
            ChatBlock::System(_) => {}
            ChatBlock::Summary { text, .. } => out.push(filar_agent::ChatMessage::user(format!(
                "Summary of earlier turns in this session:\n{text}"
            ))),
        }
        i += 1;
    }
    out
}

/// The history without the request being sent now (#526).
///
/// The user's message is already in the chat when the agent starts, and the
/// agent adds the prompt itself as the last user message — sent from the
/// history as well, it reached the model twice in a row. Only a trailing
/// `User` block equal to `prompt` is cut (feed-only `System` lines after it
/// do not count); anything else is left as is.
fn without_current_prompt<'a>(blocks: &'a [ChatBlock], prompt: &str) -> &'a [ChatBlock] {
    match blocks
        .iter()
        .rposition(|b| !matches!(b, ChatBlock::System(_)))
    {
        Some(i) if matches!(&blocks[i], ChatBlock::User(text) if text == prompt) => &blocks[..i],
        _ => blocks,
    }
}

/// Whether a command block was proposed by the agent (as opposed to the
/// user's own `!cmd`). Blocks saved before #525 carry no tool; they count as
/// the agent's unless marked as a shell escape.
fn is_agent_command(block: &ChatBlock) -> bool {
    match block {
        ChatBlock::Command { tool: Some(_), .. } => true,
        ChatBlock::Command {
            explanation,
            tool: None,
            ..
        } => explanation != filar_core::SHELL_ESCAPE_EXPLANATION,
        _ => false,
    }
}

/// End (exclusive) of the run of agent command blocks starting at `start`.
/// Feed-only `System` lines between them (a denial note) do not break it.
fn agent_command_run_end(blocks: &[ChatBlock], start: usize) -> usize {
    let mut end = start;
    let mut j = start;
    while let Some(block) = blocks.get(j) {
        match block {
            ChatBlock::System(_) => {}
            b if is_agent_command(b) => end = j + 1,
            _ => break,
        }
        j += 1;
    }
    end
}

/// Push one assistant message with `text` and a tool call per agent command
/// in `run`, followed by the matching `tool` results.
fn push_tool_turn(
    out: &mut Vec<filar_agent::ChatMessage>,
    text: &str,
    run: &[ChatBlock],
    next_id: &mut usize,
) {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for block in run {
        let ChatBlock::Command {
            command,
            explanation,
            output,
            approved,
            tool,
        } = block
        else {
            continue;
        };
        // Synthetic, 9 alphanumerics: unique within the request, never
        // clashing with a provider's own ids, accepted by strict providers.
        let id = format!("h{:08}", *next_id);
        *next_id += 1;
        let (name, arguments) = match tool {
            Some(t) => (t.name.clone(), t.arguments.clone()),
            // Saved before #525: the shell command is all that is known.
            None => (
                filar_agent::tools::TOOL_RUN_COMMAND.to_string(),
                serde_json::json!({ "command": command, "explanation": explanation }),
            ),
        };
        let result = match output {
            Some(o) => o.clone(),
            None if *approved => "(no output)".to_string(),
            None => "(denied by user)".to_string(),
        };
        calls.push(filar_agent::ToolCall {
            id: id.clone(),
            name,
            arguments,
        });
        results.push(filar_agent::ChatMessage::tool(id, result));
    }
    out.push(filar_agent::ChatMessage::assistant_with_tools(text, calls));
    out.extend(results);
}

/// Apply one save-progress event to the app (#235, #401).
///
/// Shared by the select branch and its drain: a `Done` that queued up behind
/// another event still starts the runbook, and a `TranscriptDone` arriving in
/// a burst still clears `transcript_saving` — the old drain matched only
/// three variants and silently swallowed the rest.
///
/// Takes the LLM plumbing as separate borrows rather than `&TuiConfig`:
/// `run_app` has already moved `config`'s launch fields into the app, so the
/// struct itself is not borrowable there.
fn apply_save_progress(
    app: &mut App,
    progress: SaveProgress,
    llm_factory: &RunbookLlmFactory,
    secret_provider: &Arc<StaticSecretProvider>,
    save_tx: &mpsc::UnboundedSender<SaveProgress>,
) {
    match progress {
        SaveProgress::Started => {
            app.save_progress = 0;
            app.save_error = None;
        }
        SaveProgress::Writing => {
            app.save_progress = 50;
        }
        SaveProgress::Done(filename) => {
            app.save_progress = 100;
            if !app.save_overlay_visible {
                app.save_overlay_visible = true;
            }
            app.finish_save();
            let msg = format!("Saved to {filename}");
            app.toast = Some((msg, Instant::now() + Duration::from_secs(3)));
            app.push_system_log(format!("Session saved to {filename}"));
            // Still asking whether to make a runbook (#481): remember the
            // file, and a later `y` starts the runbook for it.
            if app.runbook_offer.is_some() {
                app.runbook_offer_md = Some(filename);
                return;
            }
            // The `.md` is on disk — only now may the runbook start (#401):
            // a failed export must never spend a call on a session whose
            // record was never written.
            spawn_runbook_generation(app, &filename, llm_factory, secret_provider, save_tx);
        }
        SaveProgress::RunbookRequested(filename) => {
            // `y` after the `.md` was written (#481): the job is armed now.
            spawn_runbook_generation(app, &filename, llm_factory, secret_provider, save_tx);
        }
        SaveProgress::Error(err) => {
            // The export failed: drop the armed job (#401) so a later save
            // cannot fold a session whose `.md` was never written.
            app.runbook_armed = None;
            app.runbook_offer = None;
            app.runbook_offer_md = None;
            app.save_error = Some(err);
            app.save_progress = 0;
            if !app.save_overlay_visible {
                app.save_overlay_visible = true;
            }
            app.finish_save();
        }
        SaveProgress::TranscriptDone(sid, result) => {
            if let Some(idx) = app.find_session_idx(sid) {
                app.sessions[idx].transcript_saving = false;
                if let Some(err) = result {
                    if !app.sessions[idx].transcript_error_shown {
                        app.sessions[idx].transcript_error_shown = true;
                        app.sessions[idx]
                            .messages
                            .push(filar_core::ChatBlock::Error(format!(
                                "Transcript write failed: {err}"
                            )));
                        app.sessions[idx].message_rev =
                            app.sessions[idx].message_rev.wrapping_add(1);
                    }
                }
            }
        }
        SaveProgress::RunbookDone {
            sid,
            outcome,
            usage,
            profile,
        } => {
            app.finish_runbook(sid, outcome, usage, profile);
        }
        SaveProgress::RunbookCancelled { sid } => {
            app.finish_runbook_cancelled(sid);
        }
    }
}

/// The LLM-client factory the runner uses, spelled once for the runbook
/// helpers that must borrow it out of a partially moved [`TuiConfig`].
type RunbookLlmFactory = Arc<
    dyn Fn(
            &filar_core::LlmProfile,
            &StaticSecretProvider,
        ) -> std::result::Result<Arc<dyn LlmClient>, CoreError>
        + Send
        + Sync,
>;

/// Start the runbook the export armed, if the session earned one (#401).
///
/// Reached only from `SaveProgress::Done` — the `.md` is already on disk, so
/// nothing here can cost the record. A session with no executed commands is
/// refused without a call ([`App::take_runbook_job`]); the LLM is resolved
/// for the profile captured when Ctrl+S was pressed, not the current one —
/// the runbook is part of that export.
fn spawn_runbook_generation(
    app: &mut App,
    markdown_name: &str,
    llm_factory: &RunbookLlmFactory,
    secret_provider: &Arc<StaticSecretProvider>,
    save_tx: &mpsc::UnboundedSender<SaveProgress>,
) {
    let Some(armed) = app.take_runbook_job() else {
        return;
    };

    let llm = match app.profiles.iter().find(|p| p.name == armed.profile) {
        Some(profile) => match (llm_factory)(profile, secret_provider) {
            Ok(client) => client,
            Err(e) => {
                app.finish_runbook(
                    armed.session_id,
                    Err(e.to_string()),
                    None,
                    armed.profile.clone(),
                );
                return;
            }
        },
        None => {
            let reason = format!("profile '{}' not found", armed.profile);
            app.finish_runbook(armed.session_id, Err(reason), None, armed.profile.clone());
            return;
        }
    };

    let (path, display) = runbook_path(&armed.target_dir, markdown_name);
    let transcript = messages_to_markdown(&armed.messages, &armed.session_name, &armed.ssh_info);
    let token = CancellationToken::new();
    app.runbook_cancel = Some(token.clone());
    app.runbook_state = Some(RunbookState::Generating);

    let provider = Arc::clone(secret_provider);
    let sid = armed.session_id;
    let profile = armed.profile.clone();
    let fleet = armed.fleet.clone();
    let tx = save_tx.clone();
    tokio::spawn(async move {
        // A panic inside the call (a client's `unwrap`, say) would otherwise
        // strand the runbook in `Generating` with a live token — tokio parks
        // the panic in a `JoinHandle` nobody awaits. Report it like any
        // other failure instead (review follow-up).
        let outcome = std::panic::AssertUnwindSafe(build_runbook_file(
            llm.as_ref(),
            &token,
            &transcript,
            &path,
            &display,
            provider.as_ref(),
            fleet.as_ref(),
        ))
        .catch_unwind()
        .await;
        let report = match outcome {
            Ok(Some((outcome, usage))) => SaveProgress::RunbookDone {
                sid,
                outcome,
                usage,
                profile,
            },
            // Cancelled mid-call: acknowledge the token so the note lands
            // in the session that started the job, not the active tab
            // (review follow-up).
            Ok(None) => SaveProgress::RunbookCancelled { sid },
            Err(_) => SaveProgress::RunbookDone {
                sid,
                outcome: Err("the runbook task panicked — see the log".into()),
                usage: None,
                profile,
            },
        };
        let _ = tx.send(report);
    });
}

/// Path and display name for the runbook beside a just-written export.
///
/// `{stem}.md` becomes `{stem}.runbook.md` in the same per-target folder
/// (#400), sharing the export's timestamped stem so the pair sorts together.
/// `markdown_name` is the folder-relative form `Done` carries (`folder/file.md`).
fn runbook_path(target_dir: &std::path::Path, markdown_name: &str) -> (std::path::PathBuf, String) {
    let (folder, file_name) = match markdown_name.rsplit_once('/') {
        Some((folder, file)) => (Some(folder), file),
        None => (None, markdown_name),
    };
    let runbook_name = match file_name.strip_suffix(".md") {
        Some(stem) => format!("{stem}.runbook.md"),
        None => format!("{file_name}.runbook.md"),
    };
    let display = match folder {
        Some(folder) => format!("{folder}/{runbook_name}"),
        None => runbook_name.clone(),
    };
    (target_dir.join(&runbook_name), display)
}

/// Generate the runbook and write it, as one cancellable step (#401).
///
/// Returns `None` when the user cancelled; the spawner then reports
/// [`SaveProgress::RunbookCancelled`], so the stop is named in the session
/// the job was armed from. Otherwise the outcome for the feed (display path
/// or reason) and the usage — billed either way, since the provider
/// answered before the reply was judged.
///
/// Redaction covers both doors: the transcript is the input, and the model's
/// own reply is the output. The executor sanitises command output, but a
/// value the user typed into the chat, and anything the model writes back,
/// never passed through it.
async fn build_runbook_file(
    llm: &dyn LlmClient,
    cancellation: &CancellationToken,
    transcript: &str,
    path: &std::path::Path,
    display: &str,
    provider: &dyn SecretProvider,
    // A fleet session's host identifiers (#443): the runbook becomes a
    // procedure for the group, with every one of them scrubbed.
    fleet: Option<&filar_agent::FleetIdentifiers>,
) -> Option<(
    std::result::Result<String, String>,
    Option<filar_agent::TokenUsage>,
)> {
    let redacted = filar_core::redact_secrets(transcript, provider);
    let generate = async {
        match fleet {
            Some(ids) => filar_agent::generate_fleet_runbook(llm, &redacted, ids).await,
            None => filar_agent::generate_runbook(llm, &redacted).await,
        }
    };
    let outcome = tokio::select! {
        outcome = generate => outcome,
        _ = cancellation.cancelled() => return None,
    };
    let result = match outcome.runbook {
        Ok(text) => {
            let text = filar_core::redact_secrets(&text, provider);
            match tokio::fs::write(path, text).await {
                Ok(()) => Ok(display.to_string()),
                Err(e) => Err(format!("cannot write the runbook file: {e}")),
            }
        }
        Err(e) => Err(e.to_string()),
    };
    Some((result, outcome.usage))
}

/// Spawn the agent in a tokio task to process the user's input.
#[allow(clippy::too_many_arguments)]
fn spawn_agent(
    llm: Arc<dyn LlmClient>,
    risk_scorer: Option<Arc<dyn filar_agent::RiskScorer>>,
    arbiter_llm: Option<Arc<dyn LlmClient>>,
    arbiter_enabled: bool,
    arbiter_model_name: String,
    executor: Arc<dyn CommandExecutor>,
    confirm_mode: CommandConfirmMode,
    user_input: String,
    chat_history: Vec<ChatBlock>,
    history_epoch: u64,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    is_local: bool,
    ssh_info: Option<String>,
    cancellation: CancellationToken,
    secret_provider: Arc<dyn SecretProvider>,
    sid: SessionId,
    // The profile this run was resolved from. Travels with the summary result
    // so the cost of compaction is attributed to the profile that computed it,
    // even if the session has moved on to another one by the time it lands
    // (#387).
    profile_name: String,
    // The session has already been told that compaction cannot shrink it any
    // further. The reactive path honours that rather than quietly compacting
    // behind the notice (review of #390).
    compaction_exhausted: bool,
    // Boundary index when the history must be compacted before this request
    // (#377). `None` means send it as is.
    pending_compaction: Option<usize>,
    // The run is the fleet's (#435): `executor` is the group-level one and
    // the agent gets the fleet tool set, prompt and gate.
    fleet: bool,
    // A pod tab (#565): `executor` is the pod's shell and the agent gets the
    // pod prompt, arbiter context and safety target.
    pod_target: Option<filar_agent::KubePodTarget>,
) {
    let tx = event_tx.clone();
    let confirmer = Arc::new(TuiConfirmer::new(event_tx.clone(), sid)) as Arc<dyn CommandConfirmer>;

    tokio::spawn(async move {
        let _ = tx.send(TuiEvent::Thinking);

        // `AgentEvent::Error` is emitted exactly once, by `Agent::run`, right
        // before it returns the same error. Holding it here rather than
        // forwarding it lets the reactive path below decide whether the user
        // ever needs to see it: an overflow that is fixed by compacting and
        // retrying is not a failure worth reporting (#378).
        let held_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let tx_for_sink = tx.clone();
        let held_for_sink = Arc::clone(&held_error);
        let sink: filar_agent::EventSink = Arc::new(move |event: filar_agent::AgentEvent| {
            if let filar_agent::AgentEvent::Error(msg) = &event {
                *held_for_sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(msg.clone());
                return;
            }
            let _ = tx_for_sink.send(TuiEvent::Agent {
                session_id: sid,
                event,
            });
        });

        // Compaction runs here, before the agent is built, because this is
        // where the LLM client lives and the spinner is already up. The
        // summary uses the session's own profile, so its cost lands on the
        // profile the user is actually working with.
        let before_compaction = chat_history.len();
        let mut chat_history = compact_for_request(
            chat_history,
            pending_compaction,
            llm.as_ref(),
            &tx,
            sid,
            &profile_name,
            &cancellation,
        )
        .await;

        // At most two attempts, and at most one compaction per turn. The
        // proactive pass counts: if the threshold already folded the head, a
        // reactive fold would be the second compaction in a row, which the
        // history cannot survive usefully and the user was not promised.
        // A *failed* proactive summary leaves the history untouched and does
        // not count, so the reactive path is still available then (#378).
        let mut already_compacted = chat_history.len() < before_compaction;
        loop {
            let history = history_to_messages(without_current_prompt(&chat_history, &user_input));

            let mut builder = AgentBuilder::new()
                .llm(Arc::clone(&llm))
                .executor(Arc::clone(&executor))
                .confirmer(Arc::clone(&confirmer))
                .confirm_mode(confirm_mode)
                .event_sink(Arc::clone(&sink))
                .cancellation(cancellation.clone())
                .secret_provider(Arc::clone(&secret_provider))
                .arbiter_enabled(arbiter_enabled)
                .arbiter_llm(arbiter_llm.clone())
                .arbiter_model_name(arbiter_model_name.clone())
                .risk_scorer(risk_scorer.clone());
            if let Some(target) = pod_target.as_ref() {
                builder = builder.kube_pod_mode(target);
            } else if is_local {
                builder = builder.local_mode().arbiter_local_context();
            } else {
                builder = builder
                    .ssh_mode(ssh_info.as_deref())
                    .arbiter_ssh_context(ssh_info.clone());
            }
            builder = builder.session_id(sid.0.to_string()).fleet(fleet);

            let agent = match builder.build() {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(TuiEvent::Agent {
                        session_id: sid,
                        event: filar_agent::AgentEvent::Error(e.to_string()),
                    });
                    return;
                }
            };

            *held_error.lock().unwrap_or_else(|p| p.into_inner()) = None;
            // Run the agent loop. All events (Started, TextDelta,
            // CommandProposed, CommandFinished, Finished) are emitted via the
            // EventSink; Error is held back until the outcome is known.
            let outcome = agent.run(&user_input, &history).await;

            let overflow = matches!(outcome, Err(CoreError::ContextOverflow(_)));
            if !should_retry_after_overflow(
                overflow,
                already_compacted,
                cancellation.is_cancelled(),
                compaction_exhausted,
            ) {
                break;
            }

            // The threshold was set too high for this model's window, or no
            // usage figure ever arrived. Compact and try once more — this is
            // what keeps the feature useful when the threshold is wrong.
            let boundary =
                filar_core::compaction_boundary(&chat_history, filar_core::DEFAULT_KEEP_TURNS);
            if boundary == 0 {
                // Nothing but the verbatim tail: compacting cannot shorten it,
                // so the overflow stands.
                break;
            }
            let _ = tx.send(TuiEvent::Notice {
                session_id: sid,
                text: "The context window overflowed. Compacting the history and retrying once."
                    .to_string(),
            });
            let before = chat_history.len();
            // Arm the session before the summary goes out, or the result that
            // comes back is discarded as stale and only this local copy is
            // ever compacted (review of #390).
            let _ = tx.send(TuiEvent::CompactionStarted {
                session_id: sid,
                boundary,
                epoch: history_epoch,
            });
            chat_history = compact_for_request(
                chat_history,
                Some(boundary),
                llm.as_ref(),
                &tx,
                sid,
                &profile_name,
                &cancellation,
            )
            .await;
            if chat_history.len() >= before {
                // The summary failed, so the history is unchanged and the
                // retry would send exactly what just overflowed.
                break;
            }
            already_compacted = true;
        }

        let final_error = held_error.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(msg) = final_error {
            let _ = tx.send(TuiEvent::Agent {
                session_id: sid,
                event: filar_agent::AgentEvent::Error(msg),
            });
        }
    });
}

/// Spawn the typed Kubernetes agent, reusing chat history and compaction (#575/#569).
#[allow(clippy::too_many_arguments)]
fn spawn_kube_agent(
    llm: Arc<dyn LlmClient>,
    target: KubeAgentTarget,
    confirm_mode: CommandConfirmMode,
    tag_policies: Vec<filar_core::TagPolicy>,
    user_input: String,
    chat_history: Vec<ChatBlock>,
    history_epoch: u64,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    cancellation: CancellationToken,
    secret_provider: Arc<StaticSecretProvider>,
    sid: SessionId,
    profile_name: String,
    compaction_exhausted: bool,
    pending_compaction: Option<usize>,
) {
    let tx = event_tx.clone();
    let confirmer = Arc::new(TuiConfirmer::new(event_tx, sid)) as Arc<dyn CommandConfirmer>;
    tokio::spawn(async move {
        let _ = tx.send(TuiEvent::Thinking);
        let held_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let tx_for_sink = tx.clone();
        let held_for_sink = Arc::clone(&held_error);
        let sink: filar_agent::EventSink = Arc::new(move |event: filar_agent::AgentEvent| {
            if let filar_agent::AgentEvent::Error(message) = &event {
                *held_for_sink
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = Some(message.clone());
                return;
            }
            let _ = tx_for_sink.send(TuiEvent::Agent {
                session_id: sid,
                event,
            });
        });

        let before_compaction = chat_history.len();
        let mut chat_history = compact_for_request(
            chat_history,
            pending_compaction,
            llm.as_ref(),
            &tx,
            sid,
            &profile_name,
            &cancellation,
        )
        .await;
        let mut already_compacted = chat_history.len() < before_compaction;
        let secrets: Arc<dyn SecretProvider> = secret_provider;
        let fleet_observer = match &target {
            KubeAgentTarget::Fleet(_) => {
                let observer_tx = tx.clone();
                Some(Arc::new(move |view, status| {
                    let _ = observer_tx.send(TuiEvent::FleetSummary {
                        session_id: sid,
                        view,
                    });
                    let _ = observer_tx.send(TuiEvent::FleetStatus {
                        session_id: sid,
                        status,
                    });
                }) as filar_agent::KubeFleetObserver)
            }
            KubeAgentTarget::Cluster { .. } => None,
        };

        loop {
            let history = history_to_messages(without_current_prompt(&chat_history, &user_input));
            let agent = match &target {
                KubeAgentTarget::Cluster {
                    api,
                    target,
                    namespace,
                } => filar_agent::KubeAgent::new(
                    Arc::clone(&llm),
                    filar_agent::KubeToolExecutor::new(
                        Arc::clone(api),
                        target.clone(),
                        Arc::clone(&secrets),
                    ),
                    target.clone(),
                    namespace.clone(),
                    Arc::clone(&confirmer),
                    confirm_mode,
                    tag_policies.clone(),
                ),
                KubeAgentTarget::Fleet(fleet) => {
                    let agent = filar_agent::KubeAgent::new_fleet(
                        Arc::clone(&llm),
                        Arc::clone(fleet),
                        Arc::clone(&confirmer),
                        confirm_mode,
                        tag_policies.clone(),
                    );
                    match &fleet_observer {
                        Some(observer) => agent.fleet_observer(Arc::clone(observer)),
                        None => agent,
                    }
                }
            };
            let agent = agent
                .event_sink(Arc::clone(&sink))
                .cancellation(cancellation.clone());

            *held_error
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = None;
            let outcome = agent.run(&user_input, &history).await;
            let overflow = matches!(outcome, Err(CoreError::ContextOverflow(_)));
            if !should_retry_after_overflow(
                overflow,
                already_compacted,
                cancellation.is_cancelled(),
                compaction_exhausted,
            ) {
                break;
            }

            let boundary =
                filar_core::compaction_boundary(&chat_history, filar_core::DEFAULT_KEEP_TURNS);
            if boundary == 0 {
                break;
            }
            let _ = tx.send(TuiEvent::Notice {
                session_id: sid,
                text: "The context window overflowed. Compacting the history and retrying once."
                    .to_string(),
            });
            let before = chat_history.len();
            let _ = tx.send(TuiEvent::CompactionStarted {
                session_id: sid,
                boundary,
                epoch: history_epoch,
            });
            chat_history = compact_for_request(
                chat_history,
                Some(boundary),
                llm.as_ref(),
                &tx,
                sid,
                &profile_name,
                &cancellation,
            )
            .await;
            if chat_history.len() >= before {
                break;
            }
            already_compacted = true;
        }

        let final_error = held_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(message) = final_error {
            let _ = tx.send(TuiEvent::Agent {
                session_id: sid,
                event: filar_agent::AgentEvent::Error(message),
            });
        }
    });
}

/// Whether an attempt that just finished should be retried over a compacted
/// history.
///
/// The whole reactive rule lives here rather than inline in the task so it can
/// be tested: the loop around it ends in `agent.run`, and a lost condition
/// there would either retry forever or never retry at all, with nothing to
/// catch it (#378).
///
/// - only a context overflow is worth retrying — every other failure returns
///   the same result over a shorter history, and a success has nothing to fix;
/// - only once: the second attempt already ran on a compacted history, so a
///   third would send exactly what the second did;
/// - never after a cancellation, which is the user asking for it to stop;
/// - never once the session is exhausted. `already_retried` covers a single
///   run, so without this a later turn would start clean and compact again
///   after the user had been told the history cannot be reduced further
///   (review of #390). Checked here rather than at the compaction itself so
///   the retry notice is not emitted either.
fn should_retry_after_overflow(
    overflowed: bool,
    already_retried: bool,
    cancelled: bool,
    compaction_exhausted: bool,
) -> bool {
    overflowed && !already_retried && !cancelled && !compaction_exhausted
}

/// Summarise the head of `chat_history` and fold it, reporting the outcome.
///
/// Returns the history to send. A failed or unusable summary returns it
/// **unchanged**: losing the user's turn because the summariser misbehaved
/// would be a worse outcome than a long context (#377, #378).
/// Fold the head of `chat_history` into a summary before the request goes out.
///
/// Returns the history to send. On cancellation, on a summary the model refuses
/// to produce, or on one too short to use, that is the history it was given:
/// compaction is an optimisation, and failing at it must never cost the user
/// their turn (#378).
async fn compact_for_request(
    chat_history: Vec<ChatBlock>,
    boundary: Option<usize>,
    llm: &dyn LlmClient,
    tx: &mpsc::UnboundedSender<TuiEvent>,
    sid: SessionId,
    profile: &str,
    cancellation: &CancellationToken,
) -> Vec<ChatBlock> {
    let Some(boundary) = boundary else {
        return chat_history;
    };
    if boundary == 0 || boundary > chat_history.len() {
        return chat_history;
    }
    // Ctrl+Z between arming the compaction and getting here: do not start a
    // request the user has already called off (#394).
    if cancellation.is_cancelled() {
        return chat_history;
    }
    let transcript = filar_core::transcript_for_summary(&chat_history[..boundary]);
    // The usage goes back in both branches: the call was billed whether or not
    // the reply turned out to be usable. So does the profile that computed it,
    // which is this run's, not whichever one the session holds by the time the
    // result lands (#387).
    let outcome = tokio::select! {
        outcome = filar_agent::summarise_history(llm, &transcript) => outcome,
        _ = cancellation.cancelled() => {
            // Deliberately silent. The result is abandoned, so no
            // `HistoryCompacted` goes out: the stale guard would discard it
            // anyway, and a second feed line about a failed summary underneath
            // the user's own "Cancelled." is noise about a thing they stopped.
            //
            // Nothing is charged either. The provider returned no `usage`, and
            // inventing a zero or an estimate would be worse than recording
            // nothing — the figures the user reads are meant to be measured.
            return chat_history;
        }
    };
    let usage = outcome.usage;
    match outcome.summary {
        Ok(summary) => {
            let compacted = filar_core::compact_history(&chat_history, boundary, &summary);
            let _ = tx.send(TuiEvent::HistoryCompacted {
                session_id: sid,
                boundary,
                summary: Ok(summary),
                usage,
                profile: profile.to_string(),
            });
            compacted
        }
        Err(e) => {
            // The request still goes out on the full history: a failed summary
            // must not cost the user their turn. An empty or too-short reply
            // arrives here as an error too (#378).
            let _ = tx.send(TuiEvent::HistoryCompacted {
                session_id: sid,
                boundary,
                summary: Err(e.to_string()),
                usage,
                profile: profile.to_string(),
            });
            chat_history
        }
    }
}

type PathPickerLoadResult = std::result::Result<(Vec<PathEntry>, bool), String>;

async fn load_path_picker_dir(
    is_remote: bool,
    dir: &str,
    executor: Option<Arc<TuiExecutor>>,
) -> PathPickerLoadResult {
    if is_remote {
        let exec = executor.ok_or_else(|| "No executor for remote session".to_string())?;
        let cmd = path_picker::remote_ls_command(dir);
        let result = exec.run(&cmd).await.map_err(|e| e.to_string())?;
        if result.stdout.trim().is_empty() && result.exit_code != Some(0) {
            let detail = if result.stderr.trim().is_empty() {
                format!("ls failed (exit {:?})", result.exit_code)
            } else {
                result.stderr.trim().to_string()
            };
            return Err(detail);
        }
        let entries = path_picker::parse_ls_output(&result.stdout);
        let truncated = entries.len() >= path_picker::MAX_ENTRIES
            || result.stdout.lines().count() >= path_picker::MAX_ENTRIES;
        Ok((entries, truncated))
    } else {
        let dir = dir.to_string();
        tokio::task::spawn_blocking(move || {
            path_picker::list_local_dir(&dir).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fleet_executor_is_reused_only_for_the_operation_it_was_built_from() {
        let targets: Vec<filar_core::SshTarget> = ["web-1", "web-2", "db-1"]
            .iter()
            .map(|n| filar_core::SshTarget {
                name: (*n).into(),
                host: format!("{n}.example"),
                port: 22,
                user: "admin".into(),
                auth: filar_core::SshAuth::Key {
                    path: None,
                    passphrase: None,
                },
                host_key_policy: Default::default(),
                tags: vec![if n.starts_with("web") {
                    "web".into()
                } else {
                    "db".into()
                }],
            })
            .collect();
        let group = |tag: &str| filar_core::HostGroup {
            name: tag.into(),
            match_tags: vec![tag.into()],
            ..Default::default()
        };
        let connector = || -> filar_agent::fleet_exec::HostConnector {
            Arc::new(|_t| Box::pin(async { Err(CoreError::Other("unused".into())) }))
        };
        let mut map = HashMap::new();
        let sid = SessionId(7);

        let store = StaticSecretProvider::new();
        let creds = |op: &filar_core::FleetOperation| {
            filar_agent::fleet_creds::FleetCredentials::resolve(op, &store)
        };
        let web = filar_core::FleetOperation::open(&group("web"), &targets);
        let web_creds = creds(&web);
        let first =
            fleet_executor_for(&mut map, sid, &web, &web_creds, connector, || None).expect("web");
        let again =
            fleet_executor_for(&mut map, sid, &web, &web_creds, connector, || None).expect("web");
        assert!(
            Arc::ptr_eq(&first, &again),
            "same fleet: connections are kept"
        );

        // Any other operation under the same key — however it got there —
        // gets its own executor over its own composition.
        let db = filar_core::FleetOperation::open(&group("db"), &targets);
        assert!(
            fleet_executor_for(&mut map, sid, &db, &web_creds, connector, || None).is_err(),
            "another fleet's credentials are refused"
        );
        let swapped =
            fleet_executor_for(&mut map, sid, &db, &creds(&db), connector, || None).expect("db");
        assert!(!Arc::ptr_eq(&first, &swapped));
        assert_eq!(
            swapped.radius(),
            ["db-1"],
            "runs where the gate says it runs"
        );
        assert_eq!(
            map.len(),
            1,
            "the stale executor is dropped with its connections"
        );
    }

    #[test]
    fn an_overflow_is_retried_exactly_once() {
        // The reactive path exists because the threshold is set by hand and
        // can be wrong: without it the very case the feature was written for
        // is the one it cannot handle (#378).
        assert!(
            should_retry_after_overflow(true, false, false, false),
            "the first overflow must be compacted and retried"
        );
        assert!(
            !should_retry_after_overflow(true, true, false, false),
            "a second overflow must not start a third attempt"
        );
    }

    #[test]
    fn only_an_overflow_triggers_the_retry() {
        assert!(
            !should_retry_after_overflow(false, false, false, false),
            "a success"
        );
        // Any other failure would fail identically over a shorter history, so
        // retrying it just costs the user another request.
        assert!(
            !should_retry_after_overflow(false, true, false, false),
            "another error"
        );
    }

    #[test]
    fn a_proactive_compaction_uses_up_the_turn_s_one_compaction() {
        // The threshold path folds the head before the request goes out. If
        // that request still overflows, a reactive fold would be the second
        // compaction in a row - the thing the feature explicitly refuses to
        // do (review of #390). The runner seeds `already_compacted` from
        // whether the history actually got shorter, so this is the same rule.
        assert!(
            !should_retry_after_overflow(true, true, false, false),
            "a turn that already compacted must not compact again"
        );
        // A *failed* proactive summary leaves the history unchanged and so
        // does not use the turn up.
        assert!(should_retry_after_overflow(true, false, false, false));
    }

    #[test]
    fn an_exhausted_session_is_not_compacted_behind_the_notice() {
        // `already_retried` only covers one run, so a later turn would start
        // clean and compact again even though the user had been told the
        // session cannot be reduced further and to start a new one (review of
        // #390). Every other condition here says "retry" - the exhausted flag
        // alone must stop it.
        assert!(
            !should_retry_after_overflow(true, false, false, true),
            "an exhausted session must not be compacted again"
        );
    }

    #[test]
    fn a_cancelled_run_is_not_retried() {
        assert!(
            !should_retry_after_overflow(true, false, true, false),
            "cancellation is the user asking for it to stop"
        );
    }

    /// An `LlmClient` that hangs until released, so a test can cancel it
    /// mid-flight the way `Ctrl+Z` does.
    struct HangingLlm {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl LlmClient for HangingLlm {
        async fn chat(
            &self,
            _request: &filar_agent::ChatRequest,
        ) -> filar_core::Result<filar_agent::ChatResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(filar_agent::ChatResponse::text(
                "A real summary of the earlier turns of this session.".to_string(),
            ))
        }
    }

    fn history(n: usize) -> Vec<ChatBlock> {
        (0..n)
            .map(|i| ChatBlock::User(format!("turn {i}")))
            .collect()
    }

    #[tokio::test]
    async fn cancelling_mid_summary_abandons_the_request_and_says_nothing() {
        // Ctrl+Z used to leave the summary running to completion: the user
        // stopped the work and went on paying for a result that the stale
        // guard would then throw away (#394).
        let (tx, mut rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let llm = HangingLlm {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let blocks = history(6);

        let cancel = token.clone();
        let waiter = tokio::spawn(async move {
            started.notified().await;
            cancel.cancel();
        });

        // Bounded: without the guard this call never returns, and a hung
        // suite is a much worse regression signal than a failed assertion.
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            compact_for_request(
                blocks.clone(),
                Some(4),
                &llm,
                &tx,
                SessionId(1),
                "p",
                &token,
            ),
        )
        .await
        .expect("cancellation must abandon the request, not wait it out");
        waiter.await.unwrap();
        release.notify_waiters();

        assert_eq!(
            format!("{out:?}"),
            format!("{blocks:?}"),
            "the turn goes out on the history it already had"
        );
        assert!(
            rx.try_recv().is_err(),
            "no HistoryCompacted: the user stopped this, and the feed already says so"
        );
    }

    #[tokio::test]
    async fn a_summary_is_not_even_started_after_a_cancel() {
        // Cancelling between arming the compaction and reaching it must not
        // open a request at all — the cheapest possible outcome, and the one
        // the user asked for.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        token.cancel();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let llm = HangingLlm {
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            calls: Arc::clone(&calls),
        };
        let blocks = history(6);

        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            compact_for_request(
                blocks.clone(),
                Some(4),
                &llm,
                &tx,
                SessionId(1),
                "p",
                &token,
            ),
        )
        .await
        .expect("an already-cancelled compaction must return at once");

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no request sent"
        );
        assert_eq!(
            format!("{out:?}"),
            format!("{blocks:?}"),
            "history untouched"
        );
        assert!(rx.try_recv().is_err(), "and nothing announced");
    }

    #[tokio::test]
    async fn an_uncancelled_summary_still_compacts_and_reports() {
        // The guard must not have turned compaction off in the normal case.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let release = Arc::new(tokio::sync::Notify::new());
        release.notify_waiters();
        let llm = HangingLlm {
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::clone(&release),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let blocks = history(6);

        let releaser = {
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                release.notify_waiters();
            })
        };
        let out = compact_for_request(
            blocks.clone(),
            Some(4),
            &llm,
            &tx,
            SessionId(1),
            "p",
            &token,
        )
        .await;
        releaser.await.unwrap();

        assert!(out.len() < blocks.len(), "the head was folded");
        assert!(
            matches!(
                rx.try_recv(),
                Ok(TuiEvent::HistoryCompacted { summary: Ok(_), .. })
            ),
            "and the result was reported"
        );
    }

    #[test]
    fn the_summary_reaches_the_model_and_feed_chrome_does_not() {
        // The trap this feature had to avoid: `System` blocks are filtered out
        // when the history is flattened, so putting the summary in one would
        // have made compaction a silent loss of the whole head (#377).
        let blocks = vec![
            ChatBlock::Summary {
                text: "Restarted nginx, still 502. Config at /etc/nginx.".into(),
                replaced_blocks: 12,
            },
            ChatBlock::System("History compacted: 14 blocks to 3.".into()),
            ChatBlock::User("what next".into()),
        ];

        let messages = history_to_messages(&blocks);
        let joined: String = messages.iter().map(|m| m.content.clone()).collect();

        assert!(
            joined.contains("Restarted nginx, still 502."),
            "the summary must be part of what the model is sent"
        );
        assert!(
            !joined.contains("History compacted: 14 blocks to 3."),
            "feed-only system lines must stay out of the request"
        );
        assert_eq!(messages.len(), 2, "summary + user turn");
    }

    fn run_command_block(command: &str, output: Option<&str>, approved: bool) -> ChatBlock {
        ChatBlock::Command {
            command: command.into(),
            explanation: format!("why {command}"),
            output: output.map(Into::into),
            approved,
            tool: Some(filar_core::ToolInvocation {
                name: "run_command".into(),
                arguments: serde_json::json!({ "command": command, "explanation": "why" }),
            }),
        }
    }

    /// No command reaches the model as assistant text it could imitate (#525).
    #[test]
    fn agent_commands_are_replayed_as_tool_calls_with_their_results() {
        let blocks = vec![
            ChatBlock::User("restart nginx".into()),
            run_command_block("systemctl restart nginx", Some("done"), true),
            run_command_block("rm -rf /var/log", None, false),
        ];
        let messages = history_to_messages(&blocks);
        assert_eq!(
            messages.len(),
            4,
            "user, assistant with calls, two results: {messages:?}"
        );
        let calls = &messages[1].tool_calls;
        assert_eq!(messages[1].role, filar_agent::MessageRole::Assistant);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "run_command");
        assert_eq!(calls[0].arguments["command"], "systemctl restart nginx");
        assert_ne!(calls[0].id, calls[1].id);
        assert_eq!(
            messages[2].tool_call_id.as_deref(),
            Some(calls[0].id.as_str())
        );
        assert_eq!(messages[2].content, "done");
        assert_eq!(
            messages[3].tool_call_id.as_deref(),
            Some(calls[1].id.as_str())
        );
        assert_eq!(messages[3].content, "(denied by user)");
        for m in &messages {
            assert!(!m.content.contains("Command:"), "no command as text: {m:?}");
            assert!(!m.content.contains("Output:"), "no output as text: {m:?}");
        }
    }

    /// The preamble and the calls it introduced are one assistant message, as
    /// when they were generated — not a message that ends at "…:" (#525).
    #[test]
    fn the_preamble_and_its_commands_are_one_assistant_message() {
        let blocks = vec![
            ChatBlock::User("check the logs".into()),
            ChatBlock::Agent("Let me look at the logs:".into()),
            run_command_block("tail log", Some("a"), true),
            ChatBlock::System("Denied: x".into()),
            run_command_block("tail log2", Some("b"), true),
            ChatBlock::Agent("The log shows a timeout.".into()),
        ];
        let messages = history_to_messages(&blocks);
        assert_eq!(messages.len(), 5, "{messages:?}");
        assert_eq!(messages[1].content, "Let me look at the logs:");
        assert_eq!(
            messages[1].tool_calls.len(),
            2,
            "a system line does not split the turn"
        );
        assert_eq!(messages[2].role, filar_agent::MessageRole::Tool);
        assert_eq!(messages[3].role, filar_agent::MessageRole::Tool);
        assert_eq!(messages[4].content, "The log shows a timeout.");
        assert!(messages[4].tool_calls.is_empty());
    }

    /// Read-only tools keep their own name and arguments.
    #[test]
    fn other_tools_keep_their_name_and_arguments() {
        let blocks = vec![ChatBlock::Command {
            command: "cat /etc/hosts".into(),
            explanation: "Read file: /etc/hosts".into(),
            output: Some("127.0.0.1 localhost".into()),
            approved: true,
            tool: Some(filar_core::ToolInvocation {
                name: "read_file".into(),
                arguments: serde_json::json!({ "path": "/etc/hosts" }),
            }),
        }];
        let messages = history_to_messages(&blocks);
        assert_eq!(messages[0].tool_calls[0].name, "read_file");
        assert_eq!(messages[0].tool_calls[0].arguments["path"], "/etc/hosts");
    }

    /// A block from a session saved before #525 has no tool: it is replayed
    /// as a `run_command` call, and an approved block that never got output
    /// (interrupted) still gets a result, so every call is answered.
    #[test]
    fn blocks_without_a_tool_fall_back_to_run_command() {
        let blocks = vec![ChatBlock::Command {
            command: "uptime".into(),
            explanation: "load".into(),
            output: None,
            approved: true,
            tool: None,
        }];
        let messages = history_to_messages(&blocks);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tool_calls[0].name, "run_command");
        assert_eq!(messages[0].tool_calls[0].arguments["command"], "uptime");
        assert_eq!(messages[1].content, "(no output)");
    }

    /// The prompt being sent is not also sent from the history (#526).
    #[test]
    fn the_current_prompt_is_cut_from_the_history() {
        let blocks = vec![
            ChatBlock::User("first".into()),
            ChatBlock::Agent("ok".into()),
            ChatBlock::User("поменяй на 22.20".into()),
            ChatBlock::System("Context: 40%".into()),
        ];
        let history = without_current_prompt(&blocks, "поменяй на 22.20");
        assert_eq!(history.len(), 2, "{history:?}");
        let messages = history_to_messages(history);
        let users = messages
            .iter()
            .filter(|m| m.role == filar_agent::MessageRole::User)
            .count();
        assert_eq!(users, 1, "only the earlier question: {messages:?}");

        // Nothing to cut: a different last message, or an empty history.
        assert_eq!(without_current_prompt(&blocks[..2], "first").len(), 2);
        assert_eq!(without_current_prompt(&blocks, "other").len(), 4);
        assert!(without_current_prompt(&[], "x").is_empty());
    }

    /// The user's own `!cmd` is the user's action, not an assistant call.
    #[test]
    fn a_shell_escape_is_the_users_message() {
        let blocks = vec![ChatBlock::Command {
            command: "df -h".into(),
            explanation: filar_core::SHELL_ESCAPE_EXPLANATION.into(),
            output: Some("/dev/sda1 50%".into()),
            approved: true,
            tool: None,
        }];
        let messages = history_to_messages(&blocks);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, filar_agent::MessageRole::User);
        assert!(messages[0].tool_calls.is_empty());
        assert!(messages[0].content.contains("df -h"));
        assert!(messages[0].content.contains("/dev/sda1 50%"));
    }

    /// Errors are filar's notes, not something the assistant said.
    #[test]
    fn an_error_is_not_assistant_text() {
        let messages = history_to_messages(&[ChatBlock::Error("timeout".into())]);
        assert_eq!(messages[0].role, filar_agent::MessageRole::User);
        assert!(messages[0].content.contains("timeout"));
    }

    #[tokio::test]
    async fn recv_log_line_returns_sent_line_and_keeps_channel() {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let mut log_rx = Some(rx);
        tx.send("warn: boom".to_string()).unwrap();

        let line = recv_log_line(&mut log_rx).await;
        assert_eq!(line.as_deref(), Some("warn: boom"));
        // Channel still open (sender alive) — polling stays enabled.
        assert!(log_rx.is_some());
    }

    #[tokio::test]
    async fn recv_log_line_disables_polling_when_channel_closes() {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let mut log_rx = Some(rx);
        drop(tx); // Close the channel.

        let line = recv_log_line(&mut log_rx).await;
        assert!(line.is_none());
        // Closed channel must disable further polling to avoid a busy-loop.
        assert!(log_rx.is_none());
    }

    #[test]
    fn route_feeds_correct_session_and_marks_background() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        app.new_tab(); // active = 1
        let sid0 = app.sessions[0].id;
        app.sessions[0].terminal = Some(crate::terminal::TerminalModel::new(80, 24));

        let outcome = route_term_chunk(&mut app, sid0, TermChunk::Bytes(b"hi".to_vec()));
        assert!(matches!(outcome, RouteOutcome::Fed));
        assert!(app.sessions[0].has_new, "background tab must be marked");
    }

    #[test]
    fn route_osc7_updates_session_cwd() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let sid = app.sessions[0].id;
        app.sessions[0].terminal = Some(crate::terminal::TerminalModel::new(80, 24));
        let outcome = route_term_chunk(
            &mut app,
            sid,
            TermChunk::Bytes(b"\x1b]7;file://host/opt/app\x07".to_vec()),
        );
        assert!(matches!(outcome, RouteOutcome::Fed));
        assert_eq!(app.sessions[0].cwd.as_deref(), Some("/opt/app"));
    }

    /// #499: Enter arms the watch; the question goes out once the output is
    /// quiet, and not again until something happens.
    #[test]
    fn the_cwd_watch_asks_after_enter_once_output_is_quiet() {
        let t0 = Instant::now();
        let mut watch = CwdWatch::new(t0);
        assert!(
            !watch.due(t0 + Duration::from_secs(5)),
            "an idle terminal is not asked"
        );
        watch.note_output(t0);
        assert!(
            !watch.due(t0 + Duration::from_secs(5)),
            "typing a line is not a command"
        );

        watch.note_enter(t0);
        assert!(
            !watch.due(t0 + Duration::from_millis(50)),
            "the command is still printing"
        );
        watch.note_output(t0 + Duration::from_millis(150));
        assert!(!watch.due(t0 + Duration::from_millis(250)));
        let t1 = t0 + Duration::from_millis(150) + CWD_WATCH_QUIET;
        assert!(watch.due(t1));

        watch.asked(t1);
        assert!(
            !watch.due(t1 + Duration::from_secs(5)),
            "one question at a time"
        );
        watch.answered(Some(true));
        watch.note_output(t1 + Duration::from_secs(6));
        assert!(
            !watch.due(t1 + Duration::from_secs(20)),
            "back at the prompt: rest"
        );
    }

    /// #499: `sleep 5; cd /x` — the first answer comes while the command
    /// runs, so its later output asks again, at a growing interval.
    #[test]
    fn the_cwd_watch_keeps_asking_while_a_program_runs() {
        let t0 = Instant::now();
        let mut watch = CwdWatch::new(t0);
        watch.note_enter(t0);
        let t1 = t0 + CWD_WATCH_QUIET;
        assert!(watch.due(t1));
        watch.asked(t1);
        watch.answered(Some(false));
        assert_eq!(watch.gap, CWD_WATCH_MIN_GAP * 2);

        // Output right away: armed, but not before the gap has passed.
        watch.note_output(t1 + Duration::from_millis(300));
        assert!(!watch.due(t1 + Duration::from_millis(600)));
        assert!(watch.due(t1 + CWD_WATCH_MIN_GAP * 2));

        // The gap stops growing, and the next Enter resets it.
        for _ in 0..10 {
            watch.answered(Some(false));
        }
        assert_eq!(watch.gap, CWD_WATCH_MAX_GAP);
        watch.note_enter(t1);
        assert_eq!(watch.gap, CWD_WATCH_MIN_GAP);

        // A backend that cannot tell is not asked over and over.
        watch.asked(t1);
        watch.answered(None);
        watch.note_output(t1 + Duration::from_secs(30));
        assert!(!watch.due(t1 + Duration::from_secs(60)));
    }

    /// #499 review: an answer is matched to the backend that was asked.
    #[test]
    fn backends_are_told_apart_by_identity() {
        let (_a, a) = recording(false);
        let (_b, b) = recording(false);
        assert!(same_backend(&a, &a.clone()));
        assert!(!same_backend(&a, &b));
    }

    #[test]
    fn enter_is_found_in_pty_input() {
        assert!(has_enter(b"cd /\r"));
        assert!(has_enter(b"\n"));
        assert!(!has_enter(b"cd /"));
        assert!(!has_enter(b""));
    }

    /// #499: the answer moves the status-bar cwd of a POSIX terminal after
    /// `cd`, and leaves the #493 prompt bookkeeping alone.
    #[test]
    fn a_foreground_cwd_answer_updates_the_status_cwd() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let sid = app.sessions[0].id;
        app.sessions[0].cwd = Some("/root".into());
        assert!(
            !apply_foreground_cwd(&mut app, sid, "/".into()),
            "no terminal open: a late answer is dropped"
        );
        app.sessions[0].terminal = Some(crate::terminal::TerminalModel::new(80, 24));
        app.sessions[0].pty_prompt_cwd = Some("/root".into());

        assert!(apply_foreground_cwd(&mut app, sid, "/".into()));
        assert_eq!(app.sessions[0].cwd.as_deref(), Some("/"));
        assert_eq!(app.sessions[0].pty_prompt_cwd.as_deref(), Some("/root"));
        assert!(
            !apply_foreground_cwd(&mut app, sid, "/".into()),
            "unchanged: no redraw"
        );
    }

    /// A PTY stand-in that records what is written to it (#482).
    struct RecordingTerm {
        reports_cwd: bool,
        writes: std::sync::Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait::async_trait]
    impl InteractiveTerminal for RecordingTerm {
        async fn read_output(&self) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn write_input(&self, data: &[u8]) -> Result<()> {
            self.writes.lock().unwrap().push(data.to_vec());
            Ok(())
        }
        async fn resize(&self, _: u16, _: u16) -> Result<()> {
            Ok(())
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
        fn reports_cwd_itself(&self) -> bool {
            self.reports_cwd
        }
    }

    fn recording(reports_cwd: bool) -> (Arc<RecordingTerm>, Arc<dyn InteractiveTerminal>) {
        let term = Arc::new(RecordingTerm {
            reports_cwd,
            writes: Default::default(),
        });
        let dyn_term: Arc<dyn InteractiveTerminal> = term.clone();
        (term, dyn_term)
    }

    #[tokio::test]
    async fn a_self_reporting_shell_is_not_probed_and_its_prompt_gives_the_cwd() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let sid = app.sessions[0].id;
        app.sessions[0].terminal = Some(crate::terminal::TerminalModel::new(80, 24));
        app.sessions[0].cwd = Some(r"C:\old".into());
        let (tx, rx) = mpsc::unbounded_channel();
        // cmd.exe's #482 prompt after `cd`, still queued when the user hides.
        tx.send((
            sid,
            TermChunk::Bytes(
                "\x1b]7;file://filar-raw/C:\\Работа\\My Dir\x1b\\C:\\Работа\\My Dir>"
                    .as_bytes()
                    .to_vec(),
            ),
        ))
        .unwrap();
        let mut term_rx = Some(rx);
        let (rec, term) = recording(true);

        sync_cwd_from_interactive(&mut app, sid, &term, &HashMap::new(), &mut term_rx).await;

        assert!(
            rec.writes.lock().unwrap().is_empty(),
            "no POSIX probe for cmd/PowerShell"
        );
        assert_eq!(app.sessions[0].cwd.as_deref(), Some(r"C:\Работа\My Dir"));
    }

    #[tokio::test]
    async fn a_prompt_arriving_just_after_the_hide_still_counts() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let sid = app.sessions[0].id;
        app.sessions[0].terminal = Some(crate::terminal::TerminalModel::new(80, 24));
        app.sessions[0].cwd = Some(r"C:\old".into());
        let (tx, rx) = mpsc::unbounded_channel();
        let mut term_rx = Some(rx);
        // `cd` ran; its prompt reaches the channel 30 ms after the hide.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = tx.send((
                sid,
                TermChunk::Bytes(b"\x1b]7;file://filar-raw/C:\\new\x1b\\C:\\new>".to_vec()),
            ));
        });
        let (_rec, term) = recording(true);

        sync_cwd_from_interactive(&mut app, sid, &term, &HashMap::new(), &mut term_rx).await;

        assert_eq!(app.sessions[0].cwd.as_deref(), Some(r"C:\new"));
    }

    #[tokio::test]
    async fn a_posix_shell_is_probed() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let sid = app.sessions[0].id;
        app.sessions[0].cwd = Some("/srv".into());
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut term_rx = Some(rx);
        let (rec, term) = recording(false);

        sync_cwd_from_interactive(&mut app, sid, &term, &HashMap::new(), &mut term_rx).await;

        assert_eq!(
            rec.writes.lock().unwrap().as_slice(),
            [OSC7_PWD_PROBE.to_vec()]
        );
        // No answer within the window: the last known cwd stays.
        assert_eq!(app.sessions[0].cwd.as_deref(), Some("/srv"));
    }

    /// #493: the agent moved on while the terminal was hidden at its prompt —
    /// showing it again types a `cd` there, once.
    #[tokio::test]
    async fn showing_an_idle_terminal_carries_the_agent_cwd() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let sid = app.sessions[0].id;
        app.sessions[0].terminal = Some(crate::terminal::TerminalModel::new(80, 24));
        // The shell reported /home/u from its prompt (or the hide probe).
        route_term_chunk(
            &mut app,
            sid,
            TermChunk::Bytes(b"\x1b]7;file://filar-raw/home/u\x07".to_vec()),
        );
        assert_eq!(app.sessions[0].pty_prompt_cwd.as_deref(), Some("/home/u"));
        // The agent then ran `cd /var/log`.
        app.sessions[0].cwd = Some("/var/log".into());
        let (rec, term) = recording(false);

        carry_agent_cwd_into_pty(&mut app, sid, &term).await;
        carry_agent_cwd_into_pty(&mut app, sid, &term).await;

        assert_eq!(
            rec.writes.lock().unwrap().as_slice(),
            [b"cd /var/log\n".to_vec()]
        );
    }

    #[tokio::test]
    async fn a_terminal_in_step_with_the_agent_gets_no_cd() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let sid = app.sessions[0].id;
        app.sessions[0].cwd = Some("/srv".into());
        app.sessions[0].pty_prompt_cwd = Some("/srv".into());
        let (rec, term) = recording(false);

        carry_agent_cwd_into_pty(&mut app, sid, &term).await;

        assert!(rec.writes.lock().unwrap().is_empty());
    }

    /// After any keystroke the shell may be running a program (`top`, an
    /// editor) or hold a half-typed line a `cd` would join: nothing is typed
    /// into it until it reports a prompt again.
    #[tokio::test]
    async fn a_terminal_typed_into_gets_no_cd() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let sid = app.sessions[0].id;
        app.sessions[0].pty_prompt_cwd = Some("/home/u".into());
        note_pty_input(&mut app, sid, b"");
        assert!(
            app.sessions[0].pty_prompt_cwd.is_some(),
            "no input, no change"
        );
        note_pty_input(&mut app, sid, b"ls");
        assert!(app.sessions[0].pty_prompt_cwd.is_none(), "half-typed line");
        app.sessions[0].cwd = Some("/var/log".into());
        let (rec, term) = recording(false);

        carry_agent_cwd_into_pty(&mut app, sid, &term).await;

        assert!(rec.writes.lock().unwrap().is_empty());
    }

    #[test]
    fn route_ignores_closed_session() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let outcome = route_term_chunk(
            &mut app,
            crate::app::SessionId(9999),
            TermChunk::Bytes(b"ghost".to_vec()),
        );
        assert!(matches!(outcome, RouteOutcome::Ignored));
    }

    #[test]
    fn route_eof_returns_eof_outcome() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let sid = app.sessions[0].id;

        let outcome = route_term_chunk(&mut app, sid, TermChunk::Eof);
        assert!(matches!(outcome, RouteOutcome::Eof));
    }

    #[test]
    fn resize_applies_to_all_session_models() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        app.new_tab();
        for s in app.sessions.iter_mut() {
            s.terminal = Some(crate::terminal::TerminalModel::new(80, 24));
        }
        resize_all_models(&mut app, 100, 30);
        for s in &app.sessions {
            let t = s.terminal.as_ref().expect("terminal model must be set");
            assert_eq!(t.rows(), 30);
            assert_eq!(t.cols(), 100);
        }
    }

    #[test]
    fn session_snapshot_captures_launch_context() {
        use filar_core::{CommandConfirmMode, LlmProfile};
        let mut app = App::new("prod".into(), CommandConfirmMode::Always);
        app.profiles = vec![
            LlmProfile {
                name: "glm".into(),
                model: "glm-5.1".into(),
                api_base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
                max_tokens: 1024,
                key_env: "GLM_API_KEY".into(),
                temperature: None,
                top_p: None,
                extra_body: None,
                compact_at_tokens: filar_core::DEFAULT_COMPACT_AT_TOKENS,
            },
            LlmProfile {
                name: "other".into(),
                model: "other-model".into(),
                api_base_url: "https://other.example.com".into(),
                max_tokens: 1024,
                key_env: "OTHER_API_KEY".into(),
                temperature: None,
                top_p: None,
                extra_body: None,
                compact_at_tokens: filar_core::DEFAULT_COMPACT_AT_TOKENS,
            },
        ];
        // Default differs from the session's selected profile, so the test
        // proves the snapshot resolves model/api_base_url from the session
        // profile, not from default_profile_name.
        app.default_profile_name = "other".into();
        {
            let s = &mut app.sessions[0];
            s.ssh_info = Some("root@10.0.0.5:22".into());
            s.llm_profile = Some("glm".into());
            s.confirm_mode = CommandConfirmMode::Explain;
        }

        let snapshot = session_snapshot(&app, "prod", "123456", "2026-08-14 00:00:00");

        assert_eq!(snapshot.id, "123456");
        assert_eq!(snapshot.timestamp, "2026-08-14 00:00:00");
        assert_eq!(snapshot.ssh_info.as_deref(), Some("root@10.0.0.5:22"));
        assert_eq!(snapshot.model.as_deref(), Some("glm-5.1"));
        assert_eq!(
            snapshot.api_base_url.as_deref(),
            Some("https://open.bigmodel.cn/api/paas/v4")
        );
        assert_eq!(snapshot.confirm_mode, Some(CommandConfirmMode::Explain));
        assert_eq!(snapshot.target, "prod");
        assert_eq!(snapshot.llm_profile.as_deref(), Some("glm"));
    }

    #[test]
    fn session_snapshot_captures_only_the_configured_kubernetes_identity() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("local".into(), CommandConfirmMode::Always);
        app.sessions[0].kube = Some(crate::app::KubeTabState {
            target: filar_core::KubeCluster {
                name: "prod-eu".into(),
                context: "private-context".into(),
                kubeconfig: Some("secret-path/kubeconfig".into()),
                ..Default::default()
            },
            namespace: Some("shop".into()),
            connection_id: 7,
            pod: None,
        });

        let snapshot = session_snapshot(&app, "prod-eu", "123456", "2026-10-30 00:00:00");
        assert_eq!(
            snapshot.kube,
            Some(filar_core::KubeSessionSnapshot {
                target: "prod-eu".into(),
                namespace: Some("shop".into()),
                pod: None,
            })
        );
        let json = serde_json::to_string(&snapshot).expect("snapshot serialises");
        assert!(!json.contains("private-context"));
        assert!(!json.contains("secret-path"));
    }

    #[tokio::test]
    async fn restored_cluster_connect_starts_without_terminal_input() {
        let target = filar_core::KubeCluster {
            name: "missing-cluster".into(),
            context: "missing-context".into(),
            kubeconfig: Some(
                std::env::temp_dir().join("filar-575-definitely-missing-kubeconfig.yaml"),
            ),
            ..Default::default()
        };
        let mut app = App::new("local".into(), CommandConfirmMode::Always);
        app.kube_clusters = vec![target];
        app.restore_kube_snapshot(filar_core::KubeSessionSnapshot {
            target: "missing-cluster".into(),
            namespace: Some("shop".into()),
            pod: None,
        });
        let sid = app.active_session().id;
        let (kube_tx, mut kube_rx) = mpsc::unbounded_channel();
        let mut kube_apis: HashMap<SessionId, Arc<dyn ClusterApi>> = HashMap::new();

        start_pending_kube_connects(&mut app, &mut kube_apis, &kube_tx);

        assert!(app.take_pending_kube_connects().is_empty());
        let completed = tokio::time::timeout(Duration::from_secs(2), kube_rx.recv())
            .await
            .expect("connection attempt starts immediately")
            .expect("connection attempt reports its result");
        assert_eq!(completed.session_id, sid);
        assert!(completed.result.is_err());
    }

    #[test]
    fn session_snapshot_nulls_launch_context_when_absent() {
        use filar_core::CommandConfirmMode;
        let app = App::new("local".into(), CommandConfirmMode::Allowlist);
        let snapshot = session_snapshot(&app, "local", "123456", "2026-08-14 00:00:00");
        assert_eq!(snapshot.ssh_info, None);
        assert_eq!(snapshot.model, None);
        assert_eq!(snapshot.api_base_url, None);
        assert_eq!(snapshot.confirm_mode, Some(CommandConfirmMode::Allowlist));
        assert_eq!(snapshot.llm_profile, None);
    }

    #[test]
    fn auto_save_skips_the_fleet_layer() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Allowlist);
        app.host_groups = vec![filar_core::HostGroup {
            name: "g".into(),
            match_tags: vec!["x".into()],
            ..Default::default()
        }];
        let sid0 = app.active_session().id;
        let rev0 = app.active_session().message_rev;
        app.enter_fleet("g");
        assert!(
            !session_changed(&app, rev0, sid0),
            "the fleet never overwrites the tab's save"
        );
        app.leave_fleet_view();
        assert!(!session_changed(&app, rev0, sid0));
    }

    #[test]
    fn the_fleet_session_is_saved_to_its_own_file_with_its_composition() {
        let mut app = App::new("local".into(), CommandConfirmMode::Always);
        let target = |name: &str| filar_core::SshTarget {
            name: name.into(),
            host: format!("{name}.example"),
            port: 22,
            user: "ops".into(),
            auth: filar_core::SshAuth::Key {
                path: None,
                passphrase: None,
            },
            host_key_policy: filar_core::HostKeyPolicy::Tofu,
            tags: vec!["g".into()],
        };
        app.ssh_targets = vec![target("a"), target("b")];
        app.host_groups = vec![filar_core::HostGroup {
            name: "g".into(),
            match_tags: vec!["g".into()],
            ..Default::default()
        }];
        let mut state = FleetSaveState::default();
        assert!(
            state.due(&app, "1790", "ts").is_none(),
            "no fleet, nothing to save"
        );

        app.enter_fleet("g");
        assert!(
            state.due(&app, "1790", "ts").is_none(),
            "intro lines only: not worth a file"
        );

        app.push_message(ChatBlock::User("uptime?".into()));
        let (session, rev) = state
            .due(&app, "1790", "ts")
            .expect("a fleet with a question is saved");
        assert_eq!(session.id, "1790-fleet1");
        assert_eq!(session.ssh_info, None);
        let fleet = session.fleet.clone().expect("the composition is saved");
        assert_eq!(fleet.group, "g");
        let names: Vec<_> = fleet
            .targets
            .iter()
            .map(filar_core::FleetTarget::name)
            .collect();
        assert_eq!(names, ["a", "b"]);
        state.saved(rev);
        assert!(
            state.due(&app, "1790", "ts").is_none(),
            "unchanged since the save"
        );

        // Retag the config, restore the saved session — same hosts.
        app.exit_fleet();
        app.ssh_targets[1].tags.clear();
        app.restore_fleet(session);
        let names: Vec<_> = app
            .fleet()
            .unwrap()
            .members()
            .iter()
            .map(|m| m.name().to_string())
            .collect();
        assert_eq!(names, ["a", "b"]);

        // The restored fleet writes back to the file it came from — no copy.
        app.push_message(ChatBlock::User("again".into()));
        let (second, _) = state
            .due(&app, "1790", "ts")
            .expect("the restored fleet is saved too");
        assert_eq!(second.id, "1790-fleet1");

        // A fleet entered fresh in the same run gets a file of its own.
        app.exit_fleet();
        app.enter_fleet("g");
        app.push_message(ChatBlock::User("third".into()));
        let (third, _) = state.due(&app, "1790", "ts").expect("a new fleet is saved");
        assert_eq!(third.id, "1790-fleet2");
    }

    #[test]
    fn session_changed_detects_rev_and_tab() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t0".into(), CommandConfirmMode::Allowlist);
        let sid0 = app.active_session().id;
        let rev0 = app.active_session().message_rev;

        // Unchanged: same tab, same revision.
        assert!(!session_changed(&app, rev0, sid0));

        // Revision advanced.
        app.push_error("hi".into());
        assert!(session_changed(&app, rev0, sid0));

        // Same revision, but a different active tab.
        app.new_tab();
        let sid1 = app.active_session().id;
        let rev1 = app.active_session().message_rev;
        assert!(session_changed(&app, rev1, sid0));
        assert!(!session_changed(&app, rev1, sid1));
    }

    // ── Runbook generation (#401) ─────────────────────────────────────

    /// An `LlmClient` that answers with a fixed text and records the request,
    /// so a test can inspect what would have gone over the wire.
    struct ScriptedLlm {
        reply: String,
        usage: Option<filar_agent::TokenUsage>,
        captured: std::sync::Mutex<Option<filar_agent::ChatRequest>>,
    }

    impl ScriptedLlm {
        fn answering(reply: &str) -> Self {
            Self {
                reply: reply.to_string(),
                usage: Some(filar_agent::TokenUsage {
                    prompt_tokens: Some(200),
                    completion_tokens: Some(90),
                    total_tokens: Some(290),
                    cost: Some(0.0007),
                }),
                captured: std::sync::Mutex::new(None),
            }
        }

        fn captured_prompt(&self) -> String {
            let request = self
                .captured
                .lock()
                .unwrap()
                .clone()
                .expect("request captured");
            request
                .messages
                .into_iter()
                .map(|m| m.content)
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for ScriptedLlm {
        async fn chat(
            &self,
            request: &filar_agent::ChatRequest,
        ) -> filar_core::Result<filar_agent::ChatResponse> {
            *self.captured.lock().unwrap() = Some(request.clone());
            let response = filar_agent::ChatResponse::text(self.reply.clone());
            Ok(match &self.usage {
                Some(usage) => response.with_usage(usage.clone()),
                None => response,
            })
        }
    }

    /// An `LlmClient` that always fails, standing in for a provider outage.
    struct FailingLlm;

    #[async_trait::async_trait]
    impl LlmClient for FailingLlm {
        async fn chat(
            &self,
            _request: &filar_agent::ChatRequest,
        ) -> filar_core::Result<filar_agent::ChatResponse> {
            Err(filar_core::CoreError::Other("provider is down".into()))
        }
    }

    /// An `LlmClient` that panics inside the call — a stand-in for a client
    /// bug, which must not strand the runbook in `Generating`.
    struct PanickingLlm;

    #[async_trait::async_trait]
    impl LlmClient for PanickingLlm {
        async fn chat(
            &self,
            _request: &filar_agent::ChatRequest,
        ) -> filar_core::Result<filar_agent::ChatResponse> {
            panic!("client bug");
        }
    }

    /// A runbook long enough for `MIN_RUNBOOK_CHARS`; `with` is interpolated.
    fn long_runbook(with: &str) -> String {
        format!(
            "1. Symptom: the service answers with 502 after a deploy. \
             2. Preconditions: root on <host>. \
             3. Steps: systemctl status nginx, read the error log. {with} \
             4. Exit criteria: 200 responses return. \
             5. Next actions: restart the unit or escalate."
        )
    }

    fn runbook_temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("filar_runbook_{tag}_{}", std::process::id()))
    }

    #[test]
    fn runbook_path_names_the_sibling_of_the_export() {
        let dir = std::path::Path::new("exports/prod-web");
        let (path, display) = runbook_path(dir, "prod-web/db-1.md");
        assert_eq!(path, dir.join("db-1.runbook.md"));
        assert_eq!(display, "prod-web/db-1.runbook.md");
        // A bare name (defensive — `Done` normally carries the folder) still
        // gets the suffix and a usable path.
        let (path, display) = runbook_path(dir, "db-1.md");
        assert_eq!(path, dir.join("db-1.runbook.md"));
        assert_eq!(display, "db-1.runbook.md");
    }

    #[tokio::test]
    async fn a_secret_in_the_transcript_never_reaches_the_runbook() {
        // The user typed the password into the chat, so the executor never
        // saw it — redaction at the runbook door is the only thing that can
        // keep it out of a file meant to be shared (#401).
        let dir = runbook_temp_dir("secret");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let provider = filar_core::StaticSecretProvider::new();
        provider.insert("$FILAR_SECRET_1", "hunter2-password");
        let transcript = "User: поменяй пароль hunter2-password на базе\n\
                          $ mysql -p'hunter2-password' -e 'select 1'";
        // The model echoes the secret back — the output door must catch it too.
        let llm = ScriptedLlm::answering(&long_runbook(
            "Never type hunter2-password into a shared file; take it from the vault.",
        ));

        let path = dir.join("db-1.runbook.md");
        let outcome = build_runbook_file(
            &llm,
            &CancellationToken::new(),
            transcript,
            &path,
            "prod-web/db-1.runbook.md",
            &provider,
            None,
        )
        .await;
        let (result, usage) = outcome.expect("no cancellation — must return");
        result.expect("the write must succeed");
        assert!(usage.is_some(), "usage rides back for billing");

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            !written.contains("hunter2-password"),
            "the secret must not reach the file"
        );
        assert!(
            written.contains("$FILAR_SECRET_1"),
            "the placeholder is kept instead"
        );
        let sent = llm.captured_prompt();
        assert!(
            !sent.contains("hunter2-password"),
            "the secret must not leave the machine"
        );
        assert!(
            sent.contains("$FILAR_SECRET_1"),
            "the placeholder replaces it in the request"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_fleet_runbook_file_names_no_host_address_or_account() {
        let dir = runbook_temp_dir("fleet");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let provider = filar_core::StaticSecretProvider::new();
        let ids = filar_agent::FleetIdentifiers {
            hosts: vec!["web-1".into(), "10.0.0.11".into(), "web-2".into()],
            users: vec!["ops".into()],
        };
        // The model names hosts anyway — the output door scrubs them.
        let llm = ScriptedLlm::answering(&long_runbook(
            "Run `uname -r` across the group; web-2 (10.0.0.11) usually differs. Log in as ops.",
        ));
        let path = dir.join("fleet-web.runbook.md");
        let outcome = build_runbook_file(
            &llm,
            &CancellationToken::new(),
            "User: which kernel?\nweb-1 and web-2 answered; ops@10.0.0.11",
            &path,
            "web/fleet-web.runbook.md",
            &provider,
            Some(&ids),
        )
        .await;
        outcome
            .expect("no cancellation")
            .0
            .expect("the write must succeed");

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        let sent = llm.captured_prompt();
        for leaked in ["web-1", "web-2", "10.0.0.11", "ops@", "as ops"] {
            assert!(
                !written.contains(leaked),
                "{leaked:?} reached the file:\n{written}"
            );
            assert!(
                !sent.contains(leaked),
                "{leaked:?} reached the model:\n{sent}"
            );
        }
        assert!(
            sent.contains("for the group, not for one machine"),
            "the fleet prompt was used"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn the_runbook_is_written_beside_the_export_and_reports_its_path() {
        let dir = runbook_temp_dir("write");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let provider = filar_core::StaticSecretProvider::new();
        let llm = ScriptedLlm::answering(&long_runbook(""));
        let path = dir.join("db-1.runbook.md");
        let outcome = build_runbook_file(
            &llm,
            &CancellationToken::new(),
            "User: почини nginx",
            &path,
            "prod-web/db-1.runbook.md",
            &provider,
            None,
        )
        .await;
        let (result, usage) = outcome.expect("no cancellation — must return");
        assert_eq!(
            result.unwrap(),
            "prod-web/db-1.runbook.md",
            "the feed gets the display path"
        );
        assert!(usage.is_some());
        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("Symptom"), "the file lands on disk");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn cancelling_mid_runbook_writes_nothing_and_stays_silent() {
        // Ctrl+Z must abort the request itself (#394 precedent): the reply
        // is abandoned and no file appears. The function stays silent by
        // design — the spawner turns the `None` into `RunbookCancelled`,
        // the one line that names how the job ended.
        let dir = runbook_temp_dir("cancel");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let token = CancellationToken::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let llm = HangingLlm {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let provider = filar_core::StaticSecretProvider::new();
        let path = dir.join("db-1.runbook.md");

        let cancel = token.clone();
        let waiter = tokio::spawn(async move {
            started.notified().await;
            cancel.cancel();
        });

        // Bounded: without the guard this never returns, and a hung suite is
        // a much worse regression signal than a failed assertion.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            build_runbook_file(&llm, &token, "t", &path, "d", &provider, None),
        )
        .await
        .expect("cancellation must abandon the request, not wait it out");
        waiter.await.unwrap();
        release.notify_waiters();

        assert!(outcome.is_none(), "cancelled — nothing to report");
        assert!(
            !tokio::fs::try_exists(&path).await.unwrap_or(false),
            "a cancelled runbook must not leave a file"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_runbook_failure_leaves_the_markdown_untouched() {
        // The export is already on disk when generation starts; whatever the
        // call does, that file must be exactly as it was.
        let dir = runbook_temp_dir("fail");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let markdown = dir.join("db-1.md");
        tokio::fs::write(&markdown, "# Session: db-1")
            .await
            .unwrap();

        let provider = filar_core::StaticSecretProvider::new();
        let path = dir.join("db-1.runbook.md");
        let outcome = build_runbook_file(
            &FailingLlm,
            &CancellationToken::new(),
            "t",
            &path,
            "d",
            &provider,
            None,
        )
        .await;
        let (result, usage) = outcome.expect("failure is still reported, not cancelled");
        let err = result.expect_err("the provider error must come back");
        assert!(err.contains("provider is down"), "got: {err}");
        assert!(usage.is_none(), "no reply, no usage");
        assert_eq!(
            tokio::fs::read_to_string(&markdown).await.unwrap(),
            "# Session: db-1",
            "the export is untouched"
        );
        assert!(!tokio::fs::try_exists(&path).await.unwrap_or(false));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_short_runbook_reply_is_a_reported_failure() {
        let dir = runbook_temp_dir("short");
        let provider = filar_core::StaticSecretProvider::new();
        let llm = ScriptedLlm::answering("too short");
        let path = dir.join("db-1.runbook.md");
        let outcome = build_runbook_file(
            &llm,
            &CancellationToken::new(),
            "t",
            &path,
            "d",
            &provider,
            None,
        )
        .await;
        let (result, usage) = outcome.expect("not cancelled");
        let err = result.expect_err("a stub reply must not be written");
        assert!(err.contains("too short"), "got: {err}");
        assert!(
            usage.is_some(),
            "the call was billed even though the reply was unusable"
        );
        assert!(
            !tokio::fs::try_exists(&path).await.unwrap_or(false),
            "nothing written"
        );
    }

    /// A `TuiConfig` with one `glm` profile served by `llm`, for tests that
    /// exercise the runner's own wiring (never `run_app` itself).
    fn test_tui_config(
        llm: Arc<dyn LlmClient>,
        secret_provider: Arc<StaticSecretProvider>,
    ) -> TuiConfig {
        TuiConfig {
            target_name: "prod-web".into(),
            confirm_mode: CommandConfirmMode::Always,
            global_confirm_mode: CommandConfirmMode::Always,
            tag_policies: Vec::new(),
            host_groups: Vec::new(),
            initial_group: None,
            initial_fleet: None,
            llm_profile: "glm".into(),
            initial_messages: Vec::new(),
            initial_input_history: Vec::new(),
            initial_llm_profile: None,
            initial_tokens_in: 0,
            initial_tokens_out: 0,
            initial_cost_usd: None,
            initial_per_profile: HashMap::new(),
            initial_last_served_model: None,
            initial_model_per_profile: HashMap::new(),
            ssh_target: None,
            initial_ssh_executor: None,
            initial_ssh_info: None,
            is_local: true,
            secret_provider,
            profiles: vec![filar_core::LlmProfile {
                name: "glm".into(),
                model: "glm-5.1".into(),
                api_base_url: "http://localhost".into(),
                max_tokens: 1024,
                key_env: String::new(),
                temperature: None,
                top_p: None,
                extra_body: None,
                compact_at_tokens: filar_core::DEFAULT_COMPACT_AT_TOKENS,
            }],
            default_profile_name: "glm".into(),
            llm_factory: Arc::new(move |_, _| Ok(Arc::clone(&llm))),
            key_checker: Arc::new(|_| None),
            ssh_targets: Vec::new(),
            kube_clusters: Vec::new(),
            initial_kube: None,
            log_rx: None,
            save_dir: None,
            save_runbook: true,
            command_timeout: std::time::Duration::from_secs(30),
            arbiter_enabled: false,
            arbiter_profile: None,
            risk_scorer: None,
            risk_thresholds: (70, 40),
            risk_scorer_note: None,
        }
    }

    #[tokio::test]
    async fn a_done_export_starts_the_runbook_and_reports_through_the_channel() {
        let dir = runbook_temp_dir("done");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let provider = Arc::new(filar_core::StaticSecretProvider::new());
        let llm: Arc<dyn LlmClient> = Arc::new(ScriptedLlm::answering(&long_runbook("")));
        let config = test_tui_config(llm, Arc::clone(&provider));

        let mut app = App::new("prod-web".into(), CommandConfirmMode::Always);
        // The runner wires these from the config after construction; the
        // runbook resolves its LLM through `app.profiles`.
        app.profiles = config.profiles.clone();
        app.push_message(ChatBlock::Command {
            command: "systemctl status nginx".into(),
            explanation: "check the service".into(),
            output: Some("active (running)".into()),
            approved: true,
            tool: None,
        });
        app.runbook_armed = Some(crate::app::RunbookArmed {
            session_id: app.sessions[0].id,
            target_dir: dir.clone(),
            messages: app.sessions[0].messages.clone(),
            session_name: "prod-web".into(),
            ssh_info: None,
            profile: "glm".into(),
            fleet: None,
        });
        let (tx, mut rx) = mpsc::unbounded_channel();

        apply_save_progress(
            &mut app,
            SaveProgress::Done("prod-web/db-1.md".into()),
            &config.llm_factory,
            &config.secret_provider,
            &tx,
        );

        assert_eq!(
            app.runbook_state,
            Some(RunbookState::Generating),
            "the call is in flight"
        );
        let report = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the runbook must report back")
            .expect("channel is open");
        match report {
            SaveProgress::RunbookDone {
                sid,
                outcome,
                usage,
                profile,
            } => {
                assert_eq!(sid, app.sessions[0].id);
                assert_eq!(outcome.unwrap(), "prod-web/db-1.runbook.md");
                assert!(usage.is_some(), "billing data rides along");
                assert_eq!(profile, "glm");
            }
            other => panic!("expected RunbookDone, got {other:?}"),
        }
        let written = tokio::fs::read_to_string(dir.join("db-1.runbook.md"))
            .await
            .unwrap();
        assert!(
            written.contains("Symptom"),
            "the runbook lands beside the export"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn an_unanswered_question_holds_the_runbook_until_y() {
        let dir = runbook_temp_dir("asked");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let provider = Arc::new(filar_core::StaticSecretProvider::new());
        let llm: Arc<dyn LlmClient> = Arc::new(ScriptedLlm::answering(&long_runbook("")));
        let config = test_tui_config(llm, Arc::clone(&provider));
        let mut app = App::new("prod-web".into(), CommandConfirmMode::Always);
        app.profiles = config.profiles.clone();
        app.push_message(ChatBlock::Command {
            command: "systemctl status nginx".into(),
            explanation: "check the service".into(),
            output: Some("active (running)".into()),
            approved: true,
            tool: None,
        });
        app.runbook_offer = Some(crate::app::RunbookArmed {
            session_id: app.sessions[0].id,
            target_dir: dir.clone(),
            messages: app.sessions[0].messages.clone(),
            session_name: "prod-web".into(),
            ssh_info: None,
            profile: "glm".into(),
            fleet: None,
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.save_tx = Some(tx.clone());

        // The export lands while the question is open: no model call yet.
        apply_save_progress(
            &mut app,
            SaveProgress::Done("prod-web/db-1.md".into()),
            &config.llm_factory,
            &config.secret_provider,
            &tx,
        );
        assert_eq!(app.runbook_state, None, "nothing starts before the answer");
        assert_eq!(app.runbook_offer_md.as_deref(), Some("prod-web/db-1.md"));

        // `y`: the app asks the runner, which starts the runbook for that file.
        app.answer_runbook_offer(true);
        let request = rx.try_recv().expect("y after Done sends a request");
        apply_save_progress(
            &mut app,
            request,
            &config.llm_factory,
            &config.secret_provider,
            &tx,
        );
        assert_eq!(app.runbook_state, Some(RunbookState::Generating));
        let report = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the runbook must report back")
            .expect("channel is open");
        match report {
            SaveProgress::RunbookDone { outcome, .. } => {
                assert_eq!(outcome.unwrap(), "prod-web/db-1.runbook.md");
            }
            other => panic!("expected RunbookDone, got {other:?}"),
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_failed_export_drops_the_armed_runbook() {
        let mut app = App::new("prod-web".into(), CommandConfirmMode::Always);
        app.runbook_armed = Some(crate::app::RunbookArmed {
            session_id: app.sessions[0].id,
            target_dir: std::path::PathBuf::from("exports/prod-web"),
            messages: app.sessions[0].messages.clone(),
            session_name: "prod-web".into(),
            ssh_info: None,
            profile: "glm".into(),
            fleet: None,
        });
        let provider = Arc::new(filar_core::StaticSecretProvider::new());
        let llm: Arc<dyn LlmClient> = Arc::new(ScriptedLlm::answering(&long_runbook("")));
        let config = test_tui_config(llm, provider);
        let (tx, _rx) = mpsc::unbounded_channel();

        apply_save_progress(
            &mut app,
            SaveProgress::Error("disk full".into()),
            &config.llm_factory,
            &config.secret_provider,
            &tx,
        );

        assert!(
            app.runbook_armed.is_none(),
            "a failed export must drop the job"
        );
        assert!(app.save_error.is_some());
    }

    #[tokio::test]
    async fn a_cancelled_runbook_acknowledges_through_the_channel() {
        // The stop settles when the task says so, not when the key is
        // pressed: the ack carries the session id, so the note lands in
        // the tab the job was armed from even after a tab switch, and a
        // `RunbookDone` already in flight is never overwritten (review
        // follow-up).
        let dir = runbook_temp_dir("cancelled");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let provider = Arc::new(filar_core::StaticSecretProvider::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let llm: Arc<dyn LlmClient> = Arc::new(HangingLlm {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let config = test_tui_config(llm, Arc::clone(&provider));

        let mut app = App::new("prod-web".into(), CommandConfirmMode::Always);
        app.profiles = config.profiles.clone();
        app.push_message(ChatBlock::Command {
            command: "uptime".into(),
            explanation: "check load".into(),
            output: Some("0.1".into()),
            approved: true,
            tool: None,
        });
        app.runbook_armed = Some(crate::app::RunbookArmed {
            session_id: app.sessions[0].id,
            target_dir: dir.clone(),
            messages: app.sessions[0].messages.clone(),
            session_name: "prod-web".into(),
            ssh_info: None,
            profile: "glm".into(),
            fleet: None,
        });
        let (tx, mut rx) = mpsc::unbounded_channel();

        apply_save_progress(
            &mut app,
            SaveProgress::Done("prod-web/db-1.md".into()),
            &config.llm_factory,
            &config.secret_provider,
            &tx,
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .expect("the call must start before it can be cancelled");

        app.cancel_work();

        let report = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the stopped task must acknowledge")
            .expect("channel is open");
        match &report {
            SaveProgress::RunbookCancelled { sid } => assert_eq!(*sid, app.sessions[0].id),
            other => panic!("expected RunbookCancelled, got {other:?}"),
        }
        apply_save_progress(
            &mut app,
            report,
            &config.llm_factory,
            &config.secret_provider,
            &tx,
        );
        release.notify_waiters();

        assert_eq!(app.runbook_state, Some(RunbookState::Cancelled));
        assert!(
            app.runbook_cancel.is_none(),
            "the in-flight token is cleared"
        );
        let last = app.sessions[0]
            .messages
            .last()
            .expect("the stop must be named");
        match last {
            ChatBlock::System(text) => {
                assert!(text.contains("Runbook generation cancelled"), "got: {text}")
            }
            other => panic!("expected a system note, got {other:?}"),
        }
        assert!(
            !tokio::fs::try_exists(dir.join("db-1.runbook.md"))
                .await
                .unwrap_or(false),
            "a cancelled runbook leaves no file"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_panicking_runbook_task_still_reports_back() {
        // A panic inside the call would otherwise strand the runbook in
        // `Generating` with a live token — the `JoinHandle` is never
        // awaited, so nothing would ever clear it (review follow-up).
        let dir = runbook_temp_dir("panic");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let provider = Arc::new(filar_core::StaticSecretProvider::new());
        let llm: Arc<dyn LlmClient> = Arc::new(PanickingLlm);
        let config = test_tui_config(llm, Arc::clone(&provider));

        let mut app = App::new("prod-web".into(), CommandConfirmMode::Always);
        app.profiles = config.profiles.clone();
        app.push_message(ChatBlock::Command {
            command: "uptime".into(),
            explanation: "check load".into(),
            output: Some("0.1".into()),
            approved: true,
            tool: None,
        });
        app.runbook_armed = Some(crate::app::RunbookArmed {
            session_id: app.sessions[0].id,
            target_dir: dir.clone(),
            messages: app.sessions[0].messages.clone(),
            session_name: "prod-web".into(),
            ssh_info: None,
            profile: "glm".into(),
            fleet: None,
        });
        let (tx, mut rx) = mpsc::unbounded_channel();

        apply_save_progress(
            &mut app,
            SaveProgress::Done("prod-web/db-1.md".into()),
            &config.llm_factory,
            &config.secret_provider,
            &tx,
        );

        let report = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a panicking task must still report back")
            .expect("channel is open");
        match &report {
            SaveProgress::RunbookDone { outcome, usage, .. } => {
                let err = outcome.as_ref().expect_err("a panic is not a runbook");
                assert!(err.contains("panicked"), "got: {err}");
                assert!(usage.is_none(), "nothing was billed through the panic");
            }
            other => panic!("expected RunbookDone, got {other:?}"),
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
