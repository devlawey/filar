//! Main TUI runner — sets up the terminal and runs the event loop.
//!
//! The runner uses `tokio::select!` to poll both crossterm terminal events
//! (keyboard) and agent events (from the agent task). The agent runs in a
//! separate tokio task, and communication happens via channels.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{EnableMouseCapture, DisableMouseCapture, Event, EventStream};
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

use crate::app::{App, AppMode, SessionId};
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
// Panic hook guard
// ---------------------------------------------------------------------------

/// RAII guard that restores the default panic hook when dropped.
///
/// Installs a custom panic hook that restores the terminal state
/// (disables raw mode, leaves alternate screen, disables mouse capture)
/// *before* printing the panic message. This ensures the user can read
/// the panic text and select it with the mouse even if a panic occurs
/// inside the event loop or rendering code.
///
/// When the guard is dropped (either after normal teardown or on early
/// return), the original panic hook is restored via `take_hook()`, so
/// code running after the TUI is unaffected.
struct PanicHookGuard;

impl PanicHookGuard {
    /// Install the terminal-restoring panic hook and return a guard.
    fn install() -> Self {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Restore terminal state BEFORE printing the panic message
            // so the user can read it and select the text.
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::event::DisableMouseCapture,
                crossterm::terminal::LeaveAlternateScreen
            );
            let _ = crossterm::terminal::disable_raw_mode();
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
    /// SSH target for interactive terminal mode (Ctrl+T).
    /// If `None`, the agent runs in local mode.
    pub ssh_target: Option<filar_core::SshTarget>,
    /// Whether commands execute on the local machine (true) or over SSH (false).
    pub is_local: bool,
    /// Secret provider for command substitution and output sanitisation.
    /// Shared between the TUI (for dynamic `$FILAR_SECRET_N` insertion via
    /// Ctrl+P) and the agent (via `SecretSubstitutingExecutor`).
    pub secret_provider: Arc<StaticSecretProvider>,
    /// Receiver for WARN/ERROR log lines forwarded from the tracing subscriber
    /// (see [`crate::log_layer`]). The runner polls it and shows each line as a
    /// `System` block, so important logs surface in the chat instead of being
    /// painted over the interface. `None` disables the feature (e.g. in tests).
    pub log_rx: Option<mpsc::UnboundedReceiver<String>>,
}

/// Run the TUI with the given LLM client, executor, and configuration.
pub async fn run(
    llm: Arc<dyn LlmClient>,
    executor: Arc<dyn CommandExecutor>,
    config: TuiConfig,
) -> Result<()> {
    // Install panic hook to restore terminal state on panic.
    // The hook is automatically uninstalled when _hook_guard is dropped
    // (on normal return, early error, or panic).
    let _hook_guard = PanicHookGuard::install();

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

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)
        .map_err(|e| CoreError::Other(format!("failed to create terminal: {e}")))?;

    let result = run_app(&mut terminal, llm, executor, config).await;

    // Restore the original panic hook before terminal teardown.
    // The custom hook is no longer needed — teardown uses .ok() and
    // cannot panic. Removing the hook here avoids a redundant double
    // DisableMouseCapture if the default hook fires during teardown.
    drop(_hook_guard);

    // Restore terminal.
    disable_raw_mode().ok();
    crossterm::execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen).ok();

    result
}

/// The main application loop.
async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    llm: Arc<dyn LlmClient>,
    executor: Arc<dyn CommandExecutor>,
    mut config: TuiConfig,
) -> Result<()> {
    let mut app = if config.initial_messages.is_empty() {
        App::new(config.target_name.clone(), config.confirm_mode)
    } else {
        App::with_history(
            config.target_name.clone(),
            config.confirm_mode,
            std::mem::take(&mut config.initial_messages),
        )
    };
    // Wire the App to the same StaticSecretProvider instance used by the
    // agent's SecretSubstitutingExecutor, so Ctrl+P inserts are visible to
    // command substitution and output sanitisation.
    app.secrets = config.secret_provider.clone();

    // Channel for agent → UI events.
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<TuiEvent>();

    // Receiver for WARN/ERROR log lines mirrored into the chat.
    let mut log_rx = config.log_rx.take();

    // Build SSH info string for the system prompt (e.g. "user@host:port").
    let mut ssh_info = config.ssh_target.as_ref().map(|t| {
        format!("{}@{}:{}", t.user, t.host, t.port)
    });
    let mut is_local = config.is_local;

    // Create the TUI confirmer and executor wrappers.
    let confirmer = Arc::new(TuiConfirmer::new(agent_tx.clone()));
    let tui_executor = Arc::new(TuiExecutor {
        inner: Arc::new(tokio::sync::RwLock::new(executor)),
    });

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

    loop {
        let in_interactive = app.mode == AppMode::Interactive;

        tokio::select! {
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
                        if in_interactive {
                            let resize_sid = app.sessions[app.active].id;
                            let term_cols = cols;
                            let term_rows = ui::interactive_grid_rows(rows);
                            if let Some(model) = &mut app.terminal {
                                model.resize(term_cols, term_rows);
                            }
                            if let Some((ref term, _)) = interactive_backends.get(&resize_sid) {
                                let _ = term.resize(term_cols, term_rows).await;
                            }
                        }
                        needs_redraw = true;
                    }
                    Some(Ok(Event::Mouse(m))) => {
                        app.handle_mouse(m);
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
                        let term_result: Result<Arc<dyn InteractiveTerminal>> =
                            if let Some(ref target) = config.ssh_target {
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
                            let exec = tui_executor.clone();
                            let provider = config.secret_provider.clone();
                            let sid = app.sessions[app.active].id;
                            let tx = agent_tx.clone();
                            tokio::spawn(async move {
                                let wrapped = SecretSubstitutingExecutor::new(
                                    exec as Arc<dyn CommandExecutor>,
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
                        // Create a cancellation token for this agent run.
                        let cancel_token = CancellationToken::new();
                        app.cancellation = Some(cancel_token.clone());
                        spawn_agent(
                            llm.clone(),
                            tui_executor.clone(),
                            confirmer.clone(),
                            config.confirm_mode,
                            user_input,
                            app.messages.clone(),
                            agent_tx.clone(),
                            is_local,
                            ssh_info.clone(),
                            cancel_token,
                            config.secret_provider.clone(),
                            app.sessions[app.active].id,
                        );
                    }
                }

                // Check if user entered an SSH password — perform connection.
                if let Some(password) = app.pending_ssh_password.take() {
                    if let Some((user, host, port)) = app.pending_ssh.take() {
                        let sid = app.sessions[app.active].id;
                        let tx = agent_tx.clone();
                        let exec_clone = tui_executor.clone();
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
                            match filar_transport::SshExecutor::connect(&target).await {
                                Ok(ssh_exec) => {
                                    // Swap the executor to SSH.
                                    exec_clone
                                        .swap_executor(Arc::new(ssh_exec)
                                            as Arc<dyn CommandExecutor>)
                                        .await;
                                    // Notify runner to update system prompt info.
                                    let new_ssh_info = format!("{user}@{host}:{port}");
                                    let _ = tx.send(TuiEvent::TransportChanged {
                                        is_local: false,
                                        ssh_info: Some(new_ssh_info),
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

        // Teardown backends for tabs closed via Ctrl+W / close_tab.
        // App only signals the SessionId; runner executes the async close.
        for sid in app.take_closed_ids() {
            if let Some((term, handle)) = interactive_backends.remove(&sid) {
                let _ = term.close().await;
                handle.abort();
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
                    // Intercept TransportChanged to update system prompt info.
                    if let TuiEvent::TransportChanged { is_local: new_local, ssh_info: new_ssh } = &event {
                        is_local = *new_local;
                        ssh_info = new_ssh.clone();
                        app.target_name = new_ssh.clone().unwrap_or_else(|| "local".into());
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
    let (id, timestamp) = filar_core::session::now_session_id();
    let session = filar_core::Session {
        id,
        timestamp,
        target: config.target_name.clone(),
        llm_profile: config.llm_profile.clone(),
        messages: app.messages.clone(),
    };
    match filar_core::SessionStore::with_default_dir() {
        Ok(store) => {
            if let Err(e) = store.save(&session) {
                eprintln!("\nFailed to save session: {e}");
            } else {
                eprintln!("\nSession saved ({} messages).", session.messages.len());
            }
            let _ = store.prune_to(filar_core::session::MAX_SESSIONS);
        }
        Err(e) => {
            eprintln!("\nFailed to create session store: {e}");
        }
    }

    info!("TUI session ended");
    Ok(())
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
}
