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
    CommandExecutor, InteractiveTerminal, LocalInteractive, SecretSubstitutingExecutor, SshInteractive,
};
use tokio_util::sync::CancellationToken;

use crate::app::{App, AppMode, SaveProgress, SessionId};
use crate::confirmer::TuiConfirmer;
use crate::event::TuiEvent;
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
            if let Some(session) = snapshot.try_lock().ok().and_then(|g| g.clone()) {
                if let Ok(store) = filar_core::SessionStore::with_default_dir() {
                    let _ = store.save(&session);
                    let _ = store.prune_to(filar_core::session::MAX_SESSIONS);
                }
            }

            default_hook(info);
        }));
        Self
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
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

    // Receiver for WARN/ERROR log lines mirrored into the chat.
    let mut log_rx = config.log_rx.take();

    // Create the TUI confirmer.
    let confirmer = Arc::new(TuiConfirmer::new(agent_tx.clone()));

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
    } else if let Some(ref info) = config.initial_ssh_info {
        // Restored session was over SSH but no live ssh_target was provided
        // (e.g. `--session` restore without `--target`). Surface the saved
        // host in the tab label; reconnecting is handled by the overlay.
        app.sessions[0].ssh_info = Some(info.clone());
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
                    match save_session_now(
                        &app,
                        &config.target_name,
                        &session_id,
                        &session_timestamp,
                        &snapshot,
                    ) {
                        Ok(_) => {
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

                // Handle mode toggle (Ctrl+T).
                if app.take_toggle_interactive() {
                    let toggle_sid = app.sessions[app.active].id;
                    if in_interactive {
                        // Exit interactive mode — close backend, abort reader.
                        if let Some((term, handle)) = interactive_backends.remove(&toggle_sid) {
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
                        let term_result: Result<Arc<dyn InteractiveTerminal>> =
                            if let Some(ref target) = ssh_target {
                                SshInteractive::connect(target, cols, rows)
                                    .await
                                    .map(|t| Arc::new(t) as Arc<dyn InteractiveTerminal>)
                            } else {
                                LocalInteractive::with_size(cols, rows)
                                    .await
                                    .map(|t| Arc::new(t) as Arc<dyn InteractiveTerminal>)
                            };
                        match term_result {
                            Ok(term) => {
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
                        // Resolve the LLM client for this session's profile.
                        let session_llm = {
                            let profile_name = app.sessions[app.active]
                                .llm_profile
                                .as_deref()
                                .unwrap_or(&app.default_profile_name);
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
                        spawn_agent(
                            session_llm,
                            agent_exec,
                            confirmer.clone(),
                            app.confirm_mode,
                            user_input,
                            app.messages.clone(),
                            agent_tx.clone(),
                            is_local,
                            ssh_info,
                            cancel_token,
                            config.secret_provider.clone(),
                            sid,
                        );
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
                        tokio::spawn(async move {
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
                            match filar_transport::SshExecutor::connect(&target).await {
                                Ok(ssh_exec) => {
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
                                    let _ = tx.send(TuiEvent::Agent {
                                        session_id: sid,
                                        event: filar_agent::AgentEvent::Error(format!(
                                            "SSH connection failed: {e}"
                                        ))
                                    });
                                }
                            }
                        });
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
                let token = CancellationToken::new();
                app.ctrl_o_cancel = Some(token.clone());
                tokio::spawn(async move {
                    tokio::select! {
                        _ = token.cancelled() => return,
                        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                    }
                    let new_info = format!("{}@{}:{}", target.user, target.host, target.port);
                    match filar_transport::SshExecutor::connect(&target).await {
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
                continue;
            }
            app.ctrl_o_needs_connect = false;
            let token = CancellationToken::new();
            app.ctrl_o_cancel = Some(token.clone());
            let selection = app.ctrl_o_selection;
            let targets = app.ssh_targets.clone();
            let sid = app.sessions[app.active].id;
            let exec_entry = executors.get(&sid)
                .map(|e| (e.executor.clone(), e.ssh_target.clone()));
            let tx = agent_tx.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = token.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                }
                if let Some(idx) = selection {
                    if idx == 0 {
                        // Switch to local.
                        let local_exec = match filar_transport::LocalExecutor::new().await {
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
                        match filar_transport::SshExecutor::connect(&target).await {
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

        // Create local executors for new tabs signalled via new_tab().
        for sid in app.take_pending_local_executors() {
            match filar_transport::LocalExecutor::new().await {
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
                    if let TuiEvent::TransportChanged { session_id, ref ssh_info, ref alias, .. } = &event {
                        if let Some(idx) = app.find_session_idx(*session_id) {
                            app.sessions[idx].ssh_info = ssh_info.clone();
                            app.sessions[idx].target_name =
                                alias.clone().unwrap_or_else(|| {
                                    ssh_info.clone().unwrap_or_else(|| {
                                        format!("local-{}", idx + 1)
                                    })
                                });
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
                // Clear an expired toast so the next tick's guard goes false and
                // ticking stops after this erasing frame.
                if app.toast_text().is_none() {
                    app.toast = None;
                }
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
    match save_session_now(&app, &config.target_name, &session_id, &session_timestamp, &snapshot) {
        Ok(session) => {
            eprintln!("\nSession saved ({} messages).", session.messages.len());
        }
        Err(e) => {
            eprintln!("\nFailed to save session: {e}");
        }
    }

    info!("TUI session ended");
    Ok(())
}

/// Save the active session snapshot to disk and refresh the shared
/// panic-safe snapshot. Returns the saved session on success.
fn save_session_now(
    app: &App,
    target_name: &str,
    id: &str,
    timestamp: &str,
    snapshot: &SessionSnapshot,
) -> std::result::Result<filar_core::Session, CoreError> {
    let session = session_snapshot(app, target_name, id, timestamp);
    let store = filar_core::SessionStore::with_default_dir()?;
    store.save(&session)?;
    let _ = store.prune_to(filar_core::session::MAX_SESSIONS);
    if let Ok(mut guard) = snapshot.lock() {
        *guard = Some(session.clone());
    }
    Ok(session)
}

/// Build a serialisable [`filar_core::Session`] snapshot from the active TUI
/// session, including launch context (ssh_info, model, api_base_url,
/// confirm_mode) so a later restore can re-select the same host and model.
fn session_snapshot(app: &App, target_name: &str, id: &str, timestamp: &str) -> filar_core::Session {
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

/// Spawn the agent in a tokio task to process the user's input.
#[allow(clippy::too_many_arguments)]
fn spawn_agent(
    llm: Arc<dyn LlmClient>,
    executor: Arc<dyn CommandExecutor>,
    confirmer: Arc<dyn CommandConfirmer>,
    confirm_mode: CommandConfirmMode,
    user_input: String,
    chat_history: Vec<ChatBlock>,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    is_local: bool,
    ssh_info: Option<String>,
    cancellation: CancellationToken,
    secret_provider: Arc<dyn SecretProvider>,
    sid: SessionId,
) {
    let tx = event_tx.clone();

    tokio::spawn(async move {
        let _ = tx.send(TuiEvent::Thinking);

        let history: Vec<filar_agent::ChatMessage> = chat_history
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
                ChatBlock::Error(text) => {
                    Some(filar_agent::ChatMessage::assistant(format!(
                        "Error: {text}"
                    )))
                }
                ChatBlock::System(_) => None,
            })
            .collect();

        let tx_for_sink = tx.clone();
        let sink: filar_agent::EventSink = Arc::new(move |event: filar_agent::AgentEvent| {
            let _ = tx_for_sink.send(TuiEvent::Agent {
                session_id: sid,
                event,
            });
        });

        // Build the agent with appropriate system prompt.
        let mut builder = AgentBuilder::new()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .confirm_mode(confirm_mode)
            .event_sink(sink)
            .cancellation(cancellation)
            .secret_provider(secret_provider);
        if is_local {
            builder = builder.local_mode();
        } else {
            builder = builder.ssh_mode(ssh_info.as_deref());
        }

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

        // Run the agent loop. All events (Started, TextDelta, CommandProposed,
        // CommandFinished, Finished, Error) are emitted via the EventSink.
        // The run() wrapper emits Finished on Ok and Error on Err, so we
        // don't need to send them again here.
        let _ = agent.run(&user_input, &history).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
