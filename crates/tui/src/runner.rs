//! Main TUI runner — sets up the terminal and runs the event loop.
//!
//! The runner uses `tokio::select!` to poll both crossterm terminal events
//! (keyboard) and agent events (from the agent task). The agent runs in a
//! separate tokio task, and communication happens via channels.

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{EnableBracketedPaste, DisableBracketedPaste, EnableMouseCapture, DisableMouseCapture, Event, EventStream};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use filar_agent::{AgentBuilder, CommandConfirmer, LlmClient};
use filar_core::{CommandConfirmMode, CoreError, Result, SecretProvider, StaticSecretProvider};
use filar_transport::{
    posix_cd_input, CommandExecutor, InteractiveTerminal, LocalInteractive,
    SecretSubstitutingExecutor, SshExecutor, SshInteractive, SshTransportConfig, OSC7_PWD_PROBE,
};
use tokio_util::sync::CancellationToken;

use crate::app::{App, AppMode, SaveProgress, SessionId};
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

/// Probe OSC 7 on a live interactive PTY, update `session.cwd`, and `set_cwd`
/// on the agent executor. Does not close the backend (#338).
///
/// Always probes on Unix/SSH so a stale `session.cwd` from enter-time does not
/// skip refresh. Restores the previous cwd if the probe times out.
async fn sync_cwd_from_interactive(
    app: &mut App,
    sid: SessionId,
    term: &Arc<dyn InteractiveTerminal>,
    executors: &HashMap<SessionId, ExecutorEntry>,
    term_rx: &mut Option<mpsc::UnboundedReceiver<(SessionId, TermChunk)>>,
) {
    let is_ssh = match executors.get(&sid) {
        Some(e) => e.ssh_target.read().await.is_some(),
        None => false,
    };
    let prev = app
        .sessions
        .iter()
        .find(|s| s.id == sid)
        .and_then(|s| s.cwd.clone());
    if is_ssh || cfg!(unix) {
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
    /// Swap the inner executor to a new one (e.g. local → SSH).
    async fn swap_executor(&self, new: Arc<dyn CommandExecutor>) {
        let mut guard = self.inner.write().await;
        *guard = new;
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
    pub llm_factory: Arc<dyn Fn(&filar_core::LlmProfile, &StaticSecretProvider) -> std::result::Result<Arc<dyn LlmClient>, CoreError> + Send + Sync>,
    /// For validating a profile's API key at Ctrl+L time without building a client.
    pub key_checker: Arc<dyn Fn(&filar_core::LlmProfile) -> Option<String> + Send + Sync>,
    /// Named SSH targets from config for Ctrl+O cycling.
    pub ssh_targets: Vec<filar_core::SshTarget>,
    /// Receiver for WARN/ERROR log lines forwarded from the tracing subscriber
    /// (see [`crate::log_layer`]). The runner polls it and shows each line as a
    /// `System` block, so important logs surface in the chat instead of being
    /// painted over the interface. `None` disables the feature (e.g. in tests).
    pub log_rx: Option<mpsc::UnboundedReceiver<String>>,
    /// Directory where Ctrl+S session exports are written (`None` = CWD).
    pub save_dir: Option<std::path::PathBuf>,
    /// Per-command execution timeout from `[timeouts].command_secs`.
    /// Applied to SSH marker wait and local subprocess execution.
    pub command_timeout: Duration,
    /// When true, run the command arbiter before each confirmation gate.
    pub arbiter_enabled: bool,
    /// Optional named profile for the arbiter (`None` = session profile).
    pub arbiter_profile: Option<String>,
}

/// SSH transport tunables with the app-configured command timeout.
fn ssh_transport_config(command_timeout: Duration) -> SshTransportConfig {
    SshTransportConfig::default().with_command_timeout(command_timeout)
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
        return Err(CoreError::Other(format!("failed to enter alternate screen: {e}")));
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
    crossterm::execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen).ok();

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
    let profiles_for_restore = std::mem::take(&mut config.profiles);
    let default_for_restore = std::mem::take(&mut config.default_profile_name);
    let has_history = !config.initial_messages.is_empty()
        || !config.initial_input_history.is_empty()
        || config.initial_llm_profile.as_ref().is_some_and(|p| !p.is_empty())
        || config.initial_tokens_in > 0
        || config.initial_tokens_out > 0;
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
        a.save_dir = config.save_dir.clone();
        a
    } else {
        let mut a = App::new(config.target_name.clone(), config.confirm_mode);
        a.profiles = profiles_for_restore;
        a.default_profile_name = default_for_restore;
        a.ssh_targets = config.ssh_targets.clone();
        a.save_dir = config.save_dir.clone();
        if let Some(s) = a.sessions.first_mut() {
            s.llm_profile = Some(config.llm_profile.clone());
        }
        a
    };
    // Load available LLM profiles and default profile name.
    app.key_checker = Some(config.key_checker.clone());
    // Wire the App to the same StaticSecretProvider instance used by the
    // agent's SecretSubstitutingExecutor, so Ctrl+P inserts are visible to
    // command substitution and output sanitisation.
    app.secrets = config.secret_provider.clone();

    // Channel for agent → UI events.
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<TuiEvent>();

    // Channel for session-save progress (Ctrl+S, #234/#235).
    let (save_tx, mut save_rx) = tokio::sync::mpsc::unbounded_channel::<SaveProgress>();
    app.save_tx = Some(save_tx);

    // Channel for in-TUI path picker directory listings (#351).
    let (path_picker_tx, mut path_picker_rx) =
        tokio::sync::mpsc::unbounded_channel::<(u64, PathPickerLoadResult)>();
    let path_picker_tx_load = path_picker_tx.clone();
    let mut path_picker_load_in_flight: Option<u64> = None;

    // Receiver for WARN/ERROR log lines mirrored into the chat.
    let mut log_rx = config.log_rx.take();

    // Per-session executors. The initial executor (from main.rs) is stored
    // for the start-up session; new tabs get a LocalExecutor created on demand.
    let initial_tui = Arc::new(TuiExecutor {
        inner: Arc::new(tokio::sync::RwLock::new(executor)),
    });
    let initial_sid = app.sessions[0].id;
    let mut executors: std::collections::HashMap<
        crate::app::SessionId,
        ExecutorEntry,
    > = std::collections::HashMap::new();
    executors.insert(
        initial_sid,
        ExecutorEntry {
            executor: initial_tui,
            ssh_target: Arc::new(tokio::sync::RwLock::new(config.ssh_target.clone())),
        },
    );
    // Set initial ssh_info on the first session.
    if let Some(ref target) = config.ssh_target {
        app.sessions[0].ssh_info =
            Some(format!("{}@{}:{}", target.user, target.host, target.port));
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

    loop {
        let in_interactive = app.mode == AppMode::Interactive;
        tokio::select! {
            biased;

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
            Some(progress) = save_rx.recv() => {
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
                    }
                    SaveProgress::Error(err) => {
                        app.save_error = Some(err.clone());
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
                                    app.sessions[idx].messages.push(filar_core::ChatBlock::Error(
                                        format!("Transcript write failed: {err}")
                                    ));
                                    app.sessions[idx].message_rev =
                                        app.sessions[idx].message_rev.wrapping_add(1);
                                }
                            }
                        }
                    }
                }
                // Drain any remaining messages delivered during this iteration.
                while let Ok(p) = save_rx.try_recv() {
                    match p {
                        SaveProgress::Done(filename) => {
                            app.save_progress = 100;
                            if !app.save_overlay_visible {
                                app.save_overlay_visible = true;
                            }
                            let msg = format!("Saved to {filename}");
                            app.toast = Some((msg, Instant::now() + Duration::from_secs(3)));
                            app.push_system_log(format!("Session saved to {filename}"));
                            app.finish_save();
                        }
                        SaveProgress::Error(err) => {
                            app.save_error = Some(err);
                            app.save_progress = 0;
                            if !app.save_overlay_visible {
                                app.save_overlay_visible = true;
                            }
                            app.finish_save();
                        }
                        SaveProgress::Writing => {
                            app.save_progress = 50;
                        }
                        _ => {}
                    }
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
                        // In Interactive mode, paste manually writes to PTY.
                        if app.mode == AppMode::Interactive {
                            app.push_term_input(text.as_bytes());
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
                        let term_result: Result<Arc<dyn InteractiveTerminal>> =
                            if let Some(ref target) = ssh_target {
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
                                if ssh_target.is_some() {
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

                // Forward terminal input bytes to the backend.
                if let Some(bytes) = app.take_term_input() {
                    let write_sid = app.sessions[app.active].id;
                    if let Some((ref term, _)) = interactive_backends.get(&write_sid) {
                        let _ = term.write_input(&bytes).await;
                    }
                }

                // Check if user sent input — spawn agent or execute shell escape.
                if let Some(user_input) = app.take_input() {
                    if let Some(stripped) = user_input.strip_prefix('!') {
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
                        let (agent_exec, is_local, ssh_info) = match executors.get(&sid) {
                            Some(e) => {
                                let info = app.sessions[app.active].ssh_info.clone();
                                (e.executor.clone() as Arc<dyn CommandExecutor>, info.is_none(), info)
                            }
                            None => {
                                app.push_error("Tab executor not ready yet".into());
                                continue;
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
                            .map(|e| (e.executor.clone(), e.ssh_target.clone()));
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
                                    if let Some((ref exec, ref st)) = exec_entry {
                                        exec.swap_executor(Arc::new(ssh_exec)
                                            as Arc<dyn CommandExecutor>)
                                            .await;
                                        // Store the SshTarget so Ctrl+T can open a PTY
                                        // on the same host for this tab.
                                        *st.write().await = Some(target.clone());
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

        // ── Ctrl+O delayed host connection ─────────────────────────
        if app.ctrl_o_needs_connect {
            app.ctrl_o_needs_connect = false;
            // If we have a pending password entry target, use it directly.
            if let (Some(mut target), Some(password)) = (app.ctrl_o_pending_target.take(), app.pending_ssh_password.take()) {
                target.auth = filar_core::SshAuth::Password { password: Some(password) };
                let sid = app.ctrl_o_pending_session_id.take().unwrap_or(app.sessions[app.active].id);
                let exec_entry = executors.get(&sid)
                    .map(|e| (e.executor.clone(), e.ssh_target.clone()));
                let tx = agent_tx.clone();
                let alias = target.name.clone();
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
                    let new_info = format!("{}@{}:{}", target.user, target.host, target.port);
                    match SshExecutor::connect_with_config(
                        &target,
                        ssh_transport_config(command_timeout),
                    )
                    .await {
                        Ok(ssh_exec) => {
                            if let Some((ref exec, ref st)) = exec_entry {
                                exec.swap_executor(Arc::new(ssh_exec) as Arc<dyn CommandExecutor>).await;
                                *st.write().await = Some(target);
                            }
                            let _ = tx.send(TuiEvent::TransportChanged {
                                session_id: sid, is_local: false, ssh_info: Some(new_info), alias: Some(alias.clone()),
                            });
                            let _ = tx.send(TuiEvent::Agent {
                                session_id: sid,
                                event: filar_agent::AgentEvent::Finished(format!("Connected to {} (password)", alias)),
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(TuiEvent::Agent {
                                session_id: sid,
                                event: filar_agent::AgentEvent::Error(format!("SSH connection failed: {e}")),
                            });
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
            let sid = app.sessions[app.active].id;
            let exec_entry = executors.get(&sid)
                .map(|e| (e.executor.clone(), e.ssh_target.clone()));
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
                                return;
                            }
                        };
                        if let Some((ref exec, ref st)) = exec_entry {
                            exec.swap_executor(Arc::new(local_exec) as Arc<dyn CommandExecutor>).await;
                            *st.write().await = None;
                        }
                        let _ = tx.send(TuiEvent::TransportChanged {
                            session_id: sid, is_local: true, ssh_info: None, alias: Some("local".into()),
                        });
                        return;
                    }
                    if let Some(t) = targets.get(idx - 1) {
                        let mut target = t.clone();
                        // Resolve password for Password-auth targets.
                        if let filar_core::SshAuth::Password { ref password } = target.auth {
                            let keyring_name = format!("ssh_target:{}", target.name);
                            let resolved = password.clone()
                                .or_else(|| {
                                    let kr = filar_core::KeyringSecretProvider::new();
                                    kr.get(&keyring_name).ok().filter(|v| !v.is_empty())
                                })
                                .or_else(|| std::env::var("SSH_PASSWORD").ok().filter(|v| !v.is_empty()));
                            if let Some(pw) = resolved {
                                if password.is_some() {
                                    warn!(target = %target.name, "password in config.toml for SSH target — consider moving to OS credential store");
                                }
                                target.auth = filar_core::SshAuth::Password { password: Some(pw) };
                            } else {
                                let _ = tx.send(TuiEvent::PasswordNeeded {
                                    session_id: sid, target: target.clone(),
                                });
                                return;
                            }
                        }
                        let new_info = format!("{}@{}:{}", target.user, target.host, target.port);
                        match SshExecutor::connect_with_config(
                            &target,
                            ssh_transport_config(command_timeout),
                        )
                        .await {
                            Ok(ssh_exec) => {
                                if let Some((ref exec, ref st)) = exec_entry {
                                    exec.swap_executor(Arc::new(ssh_exec) as Arc<dyn CommandExecutor>).await;
                                    *st.write().await = Some(target);
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

        // Create local executors for new tabs signalled via new_tab().
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

        if app.should_quit {
                    break;
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
                        if let Some(idx) = app.find_session_idx(*session_id) {
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
                    app.handle_agent_event(event);
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
            // Also tick when in Thinking mode so the spinner animates, and while
            // a toast is pending so it disappears on its own timer (~1.5s)
            // without requiring further input.
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
                || app.toast.is_some() => {
                let tab_changed = prev_session != app.sessions[app.active].id;
                if app.mode == AppMode::Thinking {
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

    // Save session to disk for future restore.
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
    let active_profile_name = app
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
        llm_profile: app.llm_profile.clone(),
        messages: app.messages.clone(),
        // Persisted separately from `messages` so a reopened session keeps the
        // context compaction left it with, while still holding every turn it
        // ever had (#379).
        folded_history: app.active_session().folded_history.clone(),
        input_history: app.input_history().to_vec(),
        tokens_in: app.tokens_in,
        tokens_out: app.tokens_out,
        cost_usd: app.cost_usd,
        per_profile: app.per_profile.clone(),
        last_served_model: app.last_served_model.clone(),
        model_per_profile: app.model_per_profile.clone(),
        ssh_info: app.ssh_info.clone(),
        model,
        api_base_url,
        confirm_mode: Some(app.active_session().confirm_mode),
    };
    session.truncate_history();
    session
}

/// Whether the active session changed since the last auto-save: either its
/// message revision advanced or the user switched to a different tab.
fn session_changed(app: &App, last_rev: u64, last_session: crate::app::SessionId) -> bool {
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
fn history_to_messages(blocks: &[ChatBlock]) -> Vec<filar_agent::ChatMessage> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ChatBlock::User(text) => Some(filar_agent::ChatMessage::user(text)),
            ChatBlock::Agent(text) => Some(filar_agent::ChatMessage::assistant(text)),
            ChatBlock::Command {
                command,
                output,
                approved,
                ..
            } => {
                let output_text = output.as_deref().unwrap_or(
                    if *approved { "(no output)" } else { "(denied by user)" },
                );
                Some(filar_agent::ChatMessage::assistant(format!(
                    "Command: {command}\nOutput: {output_text}"
                )))
            }
            ChatBlock::Error(text) => Some(filar_agent::ChatMessage::assistant(format!(
                "Error: {text}"
            ))),
            ChatBlock::System(_) => None,
            ChatBlock::Summary { text, .. } => Some(filar_agent::ChatMessage::user(format!(
                "Summary of earlier turns in this session:\n{text}"
            ))),
        })
        .collect()
}

/// Spawn the agent in a tokio task to process the user's input.
#[allow(clippy::too_many_arguments)]
fn spawn_agent(
    llm: Arc<dyn LlmClient>,
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
            let history = history_to_messages(&chat_history);

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
                .arbiter_model_name(arbiter_model_name.clone());
            if is_local {
                builder = builder.local_mode().arbiter_local_context();
            } else {
                builder = builder
                    .ssh_mode(ssh_info.as_deref())
                    .arbiter_ssh_context(ssh_info.clone());
            }
            builder = builder.session_id(sid.0.to_string());

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
        assert!(!should_retry_after_overflow(false, false, false, false), "a success");
        // Any other failure would fail identically over a shorter history, so
        // retrying it just costs the user another request.
        assert!(!should_retry_after_overflow(false, true, false, false), "another error");
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
        (0..n).map(|i| ChatBlock::User(format!("turn {i}"))).collect()
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
            compact_for_request(blocks.clone(), Some(4), &llm, &tx, SessionId(1), "p", &token),
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
            compact_for_request(blocks.clone(), Some(4), &llm, &tx, SessionId(1), "p", &token),
        )
        .await
        .expect("an already-cancelled compaction must return at once");

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "no request sent");
        assert_eq!(format!("{out:?}"), format!("{blocks:?}"), "history untouched");
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
            matches!(rx.try_recv(), Ok(TuiEvent::HistoryCompacted { summary: Ok(_), .. })),
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

    #[test]
    fn a_command_and_its_outcome_still_reach_the_model_unchanged() {
        // Guards the extraction of `history_to_messages` out of `spawn_agent`.
        let blocks = vec![
            ChatBlock::Command {
                command: "systemctl restart nginx".into(),
                explanation: "restart it".into(),
                output: Some("done".into()),
                approved: true,
            },
            ChatBlock::Command {
                command: "rm -rf /var/log".into(),
                explanation: String::new(),
                output: None,
                approved: false,
            },
        ];
        let messages = history_to_messages(&blocks);
        assert!(messages[0].content.contains("Command: systemctl restart nginx"));
        assert!(messages[0].content.contains("Output: done"));
        assert!(messages[1].content.contains("(denied by user)"));
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
        app.sessions[0].terminal =
            Some(crate::terminal::TerminalModel::new(80, 24));

        let outcome = route_term_chunk(
            &mut app,
            sid0,
            TermChunk::Bytes(b"hi".to_vec()),
        );
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
}
