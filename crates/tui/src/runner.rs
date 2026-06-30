//! Main TUI runner — sets up the terminal and runs the event loop.
//!
//! The runner uses `tokio::select!` to poll both crossterm terminal events
//! (keyboard) and agent events (from the agent task). The agent runs in a
//! separate tokio task, and communication happens via channels.

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, EventStream};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use filar_agent::{AgentBuilder, CommandConfirmer, LlmClient};
use filar_core::{CommandConfirmMode, CoreError, Result};
use filar_transport::{CommandExecutor, InteractiveTerminal, LocalInteractive, SshInteractive};

use crate::app::{App, AppMode};
use crate::confirmer::TuiConfirmer;
use crate::event::AgentEvent;
use crate::terminal::TerminalModel;
use crate::ui;
use filar_core::ChatBlock;

// ---------------------------------------------------------------------------
// TuiExecutor — wraps an executor and emits events
// ---------------------------------------------------------------------------

/// A [`CommandExecutor`] wrapper that sends [`AgentEvent::CommandExecuted`]
/// events to the TUI whenever a command is executed.
///
/// The inner executor is swappable at runtime, allowing the transport
/// to switch from local to SSH (or vice versa) without restarting the app.
struct TuiExecutor {
    inner: Arc<tokio::sync::RwLock<Arc<dyn CommandExecutor>>>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    /// Shared secret variables for command substitution.
    secrets: Arc<Mutex<HashMap<String, String>>>,
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
        // Substitute secret variables ($FILAR_SECRET_N) with actual values
        // before executing the command.
        let actual_command = {
            let mut cmd = command.to_string();
            if let Ok(secrets) = self.secrets.lock() {
                for (var, value) in secrets.iter() {
                    if !value.is_empty() {
                        cmd = cmd.replace(var, value);
                    }
                }
            }
            cmd
        };

        let mut result = {
            let executor = self.inner.read().await.clone();
            executor.run(&actual_command).await?
        };

        // Sanitize output — replace any secret value with its variable name
        // so the LLM never sees the actual password in command output.
        {
            let secrets = self.secrets.lock();
            if let Ok(secrets) = secrets {
                for (var, value) in secrets.iter() {
                    if !value.is_empty() {
                        result.stdout = result.stdout.replace(value, var);
                        result.stderr = result.stderr.replace(value, var);
                    }
                }
            }
        }

        // Build output string for the UI (also sanitized).
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

        // Display the ORIGINAL command (with $FILAR_SECRET_N) in the UI.
        let _ = self.event_tx.send(AgentEvent::CommandExecuted {
            command: command.to_string(),
            output,
            approved: true,
        });

        Ok(result)
    }

    async fn cancel(&self) -> Result<()> {
        let executor = self.inner.read().await.clone();
        executor.cancel().await
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
}

/// Run the TUI with the given LLM client, executor, and configuration.
pub async fn run(
    llm: Arc<dyn LlmClient>,
    executor: Arc<dyn CommandExecutor>,
    config: TuiConfig,
) -> Result<()> {
    // Set up terminal.
    enable_raw_mode().map_err(|e| CoreError::Other(format!("failed to enable raw mode: {e}")))?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)
        .map_err(|e| CoreError::Other(format!("failed to enter alternate screen: {e}")))?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)
        .map_err(|e| CoreError::Other(format!("failed to create terminal: {e}")))?;

    let result = run_app(&mut terminal, llm, executor, config).await;

    // Restore terminal.
    disable_raw_mode().ok();
    crossterm::execute!(io::stdout(), LeaveAlternateScreen).ok();

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

    // Channel for agent → UI events.
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // Build SSH info string for the system prompt (e.g. "user@host:port").
    let mut ssh_info = config.ssh_target.as_ref().map(|t| {
        format!("{}@{}:{}", t.user, t.host, t.port)
    });
    let mut is_local = config.is_local;

    // Create the TUI confirmer and executor wrappers.
    // Share secrets between App and TuiExecutor for secure variable substitution.
    let shared_secrets = app.secrets.clone();
    let confirmer = Arc::new(TuiConfirmer::new(agent_tx.clone()));
    let tui_executor = Arc::new(TuiExecutor {
        inner: Arc::new(tokio::sync::RwLock::new(executor)),
        event_tx: agent_tx.clone(),
        secrets: shared_secrets,
    });

    // Crossterm event stream for async keyboard input.
    let mut events = EventStream::new();

    // Interactive terminal backend (set when entering interactive mode).
    let mut interactive_term: Option<Arc<dyn InteractiveTerminal>> = None;

    // Draw initial UI.
    terminal.draw(|f| ui::render(f, &app)).ok();

    let mut prev_mode = app.mode;
    let mut needs_redraw = false;
    let mut needs_clear = false; // Full clear to prevent overlap during rapid updates.
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
                                app.messages.push(ChatBlock::Error(
                                    format!("Failed to start terminal: {e}")
                                ));
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
                    if user_input.starts_with('!') {
                        // Shell escape: execute command directly without agent.
                        let cmd = user_input[1..].trim().to_string();
                        if !cmd.is_empty() {
                            let exec = tui_executor.clone();
                            let tx = agent_tx.clone();
                            tokio::spawn(async move {
                                if let Err(e) = exec.run(&cmd).await {
                                    let _ = tx.send(AgentEvent::Error(
                                        format!("Shell command failed: {e}")
                                    ));
                                }
                                // Signal completion to return to Normal mode.
                                let _ = tx.send(AgentEvent::Finished(String::new()));
                            });
                        } else {
                            // Empty command after ! — just return to normal.
                            app.mode = crate::app::AppMode::Normal;
                            app.agent_running = false;
                        }
                    } else {
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
                        );
                    }
                }

                // Check if user entered an SSH password — perform connection.
                if let Some(password) = app.pending_ssh_password.take() {
                    if let Some((user, host, port)) = app.pending_ssh.take() {
                        let tx = agent_tx.clone();
                        let exec_clone = tui_executor.clone();
                        tokio::spawn(async move {
                            let _ = tx.send(AgentEvent::Thinking);
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
                                    let _ = tx.send(AgentEvent::TransportChanged {
                                        is_local: false,
                                        ssh_info: Some(new_ssh_info),
                                    });
                                    let _ = tx.send(AgentEvent::Finished(format!(
                                        "Connected to {user}@{host}:{port} via SSH. \
                                         You are now operating on the remote machine."
                                    )));
                                }
                                Err(e) => {
                                    let _ = tx.send(AgentEvent::Error(format!(
                                        "SSH connection failed: {e}"
                                    )));
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
                    std::future::pending::<Option<AgentEvent>>().await
                } else {
                    agent_rx.recv().await
                }
            } => {
                if let Some(event) = maybe_agent_event {
                    // Intercept TransportChanged to update system prompt info.
                    if let AgentEvent::TransportChanged { is_local: new_local, ssh_info: new_ssh } = &event {
                        is_local = *new_local;
                        ssh_info = new_ssh.clone();
                        app.target_name = new_ssh.clone().unwrap_or_else(|| "local".into());
                    }
                    app.handle_agent_event(event);
                    needs_redraw = true;
                    needs_clear = true;
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
            _ = render_interval.tick(), if needs_redraw => {
                if needs_clear || prev_mode != app.mode {
                    terminal.clear().ok();
                    needs_clear = false;
                    prev_mode = app.mode;
                }
                terminal.draw(|f| ui::render(f, &app)).ok();
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
    match filar_core::SessionStore::new() {
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

/// Spawn the agent in a tokio task to process the user's input.
fn spawn_agent(
    llm: Arc<dyn LlmClient>,
    executor: Arc<dyn CommandExecutor>,
    confirmer: Arc<dyn CommandConfirmer>,
    confirm_mode: CommandConfirmMode,
    user_input: String,
    chat_history: Vec<ChatBlock>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    is_local: bool,
    ssh_info: Option<String>,
) {
    let tx = event_tx.clone();

    tokio::spawn(async move {
        // Notify UI that the agent started.
        let _ = tx.send(AgentEvent::Started);
        let _ = tx.send(AgentEvent::Thinking);

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

        // Build the agent with appropriate system prompt.
        let mut builder = AgentBuilder::new()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .confirm_mode(confirm_mode);
        if is_local {
            builder = builder.local_mode();
        } else {
            builder = builder.ssh_mode(ssh_info.as_deref());
        }
        let agent = match builder.build() {
            Ok(a) => a,
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(e.to_string()));
                return;
            }
        };

        // Run the agent loop.
        match agent.run(&user_input, &history).await {
            Ok(result) => {
                let _ = tx.send(AgentEvent::Finished(result));
            }
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(e.to_string()));
            }
        }
    });
}
