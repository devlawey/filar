//! Main TUI runner — sets up the terminal and runs the event loop.
//!
//! The runner uses `tokio::select!` to poll both crossterm terminal events
//! (keyboard) and agent events (from the agent task). The agent runs in a
//! separate tokio task, and communication happens via channels.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

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

use crate::app::{App, AppMode};
use crate::confirmer::TuiConfirmer;
use crate::event::TuiEvent;
use crate::terminal::TerminalModel;
use crate::ui;
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

    // Interactive terminal backend (set when entering interactive mode).
    let mut interactive_term: Option<Arc<dyn InteractiveTerminal>> = None;

    // Draw initial UI.
    terminal.draw(|f| ui::render(f, &mut app)).ok();

    let mut prev_mode = app.mode;
    let mut needs_redraw = false;
    let mut render_interval = tokio::time::interval(Duration::from_millis(16));
    render_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let in_interactive = app.mode == AppMode::Interactive;
        // Clone the Arc for the read future (avoids borrowing interactive_term).
        let term_for_read = interactive_term.clone();

        tokio::select! {
            // Terminal keyboard / resize event.
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press => {
                        app.handle_key(key);
                        needs_redraw = true;
                    }
                    Some(Ok(Event::Resize(cols, rows))) => {
                        if in_interactive {
                            let term_cols = cols;
                            let term_rows = rows.saturating_sub(2); // status + help bar
                            if let Some(model) = &mut app.terminal {
                                model.resize(term_cols, term_rows);
                            }
                            if let Some(ref term) = interactive_term {
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
                    if in_interactive {
                        // Exit interactive mode.
                        if let Some(ref term) = interactive_term {
                            let _ = term.close().await;
                        }
                        interactive_term = None;
                        app.exit_interactive();
                    } else if !app.agent_running {
                        // Enter interactive mode.
                        let size = terminal.size().unwrap_or_default();
                        let cols = size.width;
                        let rows = size.height.saturating_sub(2);
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
                                interactive_term = Some(term);
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
                    if let Some(ref term) = interactive_term {
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
                            let tx = agent_tx.clone();
                            tokio::spawn(async move {
                                // Wrap with SecretSubstitutingExecutor so that
                                // $FILAR_SECRET_N placeholders are substituted
                                // and output is sanitised — same as agent path.
                                let wrapped = SecretSubstitutingExecutor::new(
                                    exec as Arc<dyn CommandExecutor>,
                                    provider as Arc<dyn SecretProvider>,
                                );
                                let succeeded = match wrapped.run(&cmd).await {
                                    Ok(result) => {
                                        // Build display output from CommandResult.
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
                                        let _ = tx.send(TuiEvent::Agent(
                                            filar_agent::AgentEvent::CommandFinished {
                                                command: cmd.clone(),
                                                output,
                                                denied: false,
                                            }
                                        ));
                                        true
                                    }
                                    Err(e) => {
                                        let _ = tx.send(TuiEvent::Agent(
                                            filar_agent::AgentEvent::Error(
                                                format!("Shell command failed: {e}")
                                            )
                                        ));
                                        false
                                    }
                                };
                                // Signal completion only on success — Error is
                                // already a terminal event for the TUI handler.
                                if succeeded {
                                    let _ = tx.send(TuiEvent::Agent(
                                        filar_agent::AgentEvent::Finished(String::new())
                                    ));
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
                        );
                    }
                }

                // Check if user entered an SSH password — perform connection.
                if let Some(password) = app.pending_ssh_password.take() {
                    if let Some((user, host, port)) = app.pending_ssh.take() {
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
                                    let _ = tx.send(TuiEvent::Agent(
                                        filar_agent::AgentEvent::Finished(format!(
                                            "Connected to {user}@{host}:{port} via SSH. \
                                             You are now operating on the remote machine."
                                        ))
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx.send(TuiEvent::Agent(
                                        filar_agent::AgentEvent::Error(format!(
                                            "SSH connection failed: {e}"
                                        ))
                                    ));
                                }
                            }
                        });
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

            // Terminal output (only when in interactive mode).
            maybe_output = async move {
                match term_for_read {
                    Some(term) => term.read_output().await,
                    None => std::future::pending::<
                        std::result::Result<Option<Vec<u8>>, filar_core::CoreError>
                    >().await,
                }
            } => {
                match maybe_output {
                    Ok(Some(bytes)) => {
                        if let Some(model) = &mut app.terminal {
                            model.feed(&bytes);
                        }
                        needs_redraw = true;
                    }
                    Ok(None) => {
                        // Terminal EOF (shell exited) — auto-exit interactive mode.
                        if let Some(ref term) = interactive_term {
                            let _ = term.close().await;
                        }
                        interactive_term = None;
                        app.exit_interactive();
                        needs_redraw = true;
                    }
                    Err(e) => {
                        error!(error = %e, "terminal read error");
                        interactive_term = None;
                        app.exit_interactive();
                        needs_redraw = true;
                    }
                }
            }

            // Render at most 60fps — batches multiple events into one draw.
            // Also tick when in Thinking mode so the spinner animates.
            _ = render_interval.tick(), if needs_redraw || app.mode == AppMode::Thinking => {
                if app.mode == AppMode::Thinking {
                    app.tick = app.tick.wrapping_add(1);
                }
                if prev_mode != app.mode {
                    terminal.clear().ok();
                    prev_mode = app.mode;
                }
                terminal.draw(|f| ui::render(f, &mut app)).ok();
                needs_redraw = false;
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Clean up interactive terminal if still open.
    if let Some(ref term) = interactive_term {
        let _ = term.close().await;
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
) {
    let tx = event_tx.clone();

    tokio::spawn(async move {
        // TUI-specific: show spinner before the first text delta arrives.
        let _ = tx.send(TuiEvent::Thinking);

        // Convert chat history (ChatBlock) to LLM messages (ChatMessage).
        // Only User and Agent blocks are included — command blocks and system
        // messages are omitted because they can't be faithfully reconstructed
        // without tool call IDs.
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
                    // Include command context as an assistant message so the
                    // LLM remembers what it ran and what the result was.
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

        // Set up EventSink: forward agent events to the TUI channel.
        // The agent emits Started, TextDelta, CommandProposed, CommandFinished,
        // Finished, and Error via this sink.
        let tx_for_sink = tx.clone();
        let sink: filar_agent::EventSink = Arc::new(move |event: filar_agent::AgentEvent| {
            let _ = tx_for_sink.send(TuiEvent::Agent(event));
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
                let _ = tx.send(TuiEvent::Agent(
                    filar_agent::AgentEvent::Error(e.to_string())
                ));
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
}
