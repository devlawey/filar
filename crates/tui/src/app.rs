//! Application state for the TUI.
//!
//! [`App`] holds the chat history, current input, mode, and pending
//! confirmation requests. It is updated by both terminal events (keyboard)
//! and agent events (from the agent task).

use tokio::sync::oneshot;
use filar_core::{ChatBlock, CommandConfirmMode};
use ratatui::layout::Rect;

use crate::event::AgentEvent;
use crate::terminal::{key_to_bytes, TerminalModel};
use crate::ui::layout_cache::ChatLayoutCache;
use crate::ui::Theme;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// App mode
// ---------------------------------------------------------------------------

/// The current interaction mode of the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// Waiting for user input (agent chat mode).
    Normal,
    /// Agent is processing (LLM call or command execution in progress).
    Thinking,
    /// Waiting for the user to approve or deny a command.
    Confirming,
    /// Interactive terminal mode — raw PTY/SSH terminal emulator.
    Interactive,
    /// Secure password input mode — input is masked with asterisks.
    PasswordInput,
}

// ---------------------------------------------------------------------------
// Pending confirmation
// ---------------------------------------------------------------------------

/// A pending confirmation request from the agent.
pub struct PendingConfirm {
    pub command: String,
    pub explanation: String,
    pub destructive: bool,
    pub respond_to: oneshot::Sender<bool>,
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// The main application state.
pub struct App {
    /// Chat history blocks.
    pub messages: Vec<ChatBlock>,
    /// Current input text.
    pub input: String,
    /// Cursor position in the input (char index, 0 = before first char).
    pub cursor_pos: usize,
    /// Current interaction mode.
    pub mode: AppMode,
    /// Scroll offset: 0 = bottom (latest), positive = scrolled up.
    pub scroll: usize,
    /// Current target name (for the status bar).
    pub target_name: String,
    /// Command confirmation mode.
    pub confirm_mode: CommandConfirmMode,
    /// Pending confirmation request (when mode == Confirming).
    pub pending_confirm: Option<PendingConfirm>,
    /// Whether the agent task is currently running.
    pub agent_running: bool,
    /// Set to true when the user wants to quit.
    pub should_quit: bool,
    /// Pending user input to be sent to the agent.
    pending_input: Option<String>,
    /// Interactive terminal model (when in interactive mode).
    pub terminal: Option<TerminalModel>,
    /// Pending terminal input bytes (from key events, to be written to PTY/SSH).
    pending_term_input: Option<Vec<u8>>,
    /// Flag: user pressed Ctrl+T to toggle between agent and interactive modes.
    pub toggle_interactive: bool,
    /// Shared secret variables: $FILAR_SECRET_N → actual value.
    /// Used to substitute secrets in commands without exposing them to the LLM.
    pub secrets: Arc<Mutex<HashMap<String, String>>>,
    /// Counter for the next secret variable name.
    pub secret_counter: usize,
    /// History of all user inputs (for Up/Down navigation).
    input_history: Vec<String>,
    /// Current position in history browsing (None = not browsing).
    history_pos: Option<usize>,
    /// Saved input when user starts browsing history.
    saved_input: String,
    /// Pending SSH connection: (user, host, port) parsed from `!ssh user@host`.
    pub pending_ssh: Option<(String, String, u16)>,
    /// Pending SSH password entered by the user via Ctrl+P.
    pub pending_ssh_password: Option<String>,
    /// Colour theme used by the UI renderer.
    pub theme: Theme,
    /// Cached chat layout — avoids re-wrapping text on every frame.
    pub layout_cache: ChatLayoutCache,
    /// Revision counter — bumped on any mutation of `messages` to
    /// invalidate [`layout_cache`](Self::layout_cache).
    pub message_rev: u64,
    /// Actual chat area on screen (filled during render, for hit-testing).
    pub chat_area: Rect,
    /// Actual input area on screen (filled during render, for hit-testing).
    pub input_area: Rect,
    /// Confirm button areas (filled later, for mouse click detection).
    pub confirm_button_areas: Vec<(Rect, bool)>,
}

impl App {
    /// Create a new app with the given target name and confirmation mode.
    pub fn new(target_name: String, confirm_mode: CommandConfirmMode) -> Self {
        Self {
            messages: vec![ChatBlock::System(format!(
                "Connected to: {target_name} | Mode: {confirm_mode:?}"
            ))],
            input: String::new(),
            cursor_pos: 0,
            mode: AppMode::Normal,
            scroll: 0,
            target_name,
            confirm_mode,
            pending_confirm: None,
            agent_running: false,
            should_quit: false,
            pending_input: None,
            terminal: None,
            pending_term_input: None,
            toggle_interactive: false,
            secrets: Arc::new(Mutex::new(HashMap::new())),
            secret_counter: 0,
            input_history: Vec::new(),
            history_pos: None,
            saved_input: String::new(),
            pending_ssh: None,
            pending_ssh_password: None,
            theme: Theme::default_dark(),
            layout_cache: ChatLayoutCache::new(),
            message_rev: 0,
            chat_area: Rect::default(),
            input_area: Rect::default(),
            confirm_button_areas: Vec::new(),
        }
    }

    /// Create a new app with pre-loaded chat history (for session restore).
    pub fn with_history(
        target_name: String,
        confirm_mode: CommandConfirmMode,
        messages: Vec<ChatBlock>,
    ) -> Self {
        let mut app = Self::new(target_name, confirm_mode);
        if !messages.is_empty() {
            app.messages = messages;
            // Bump rev for the wholesale replacement so the cache rebuilds.
            app.message_rev = app.message_rev.wrapping_add(1);
            app.push_message(ChatBlock::System(
                "Session restored — history loaded from disk".into(),
            ));
        }
        app
    }

    /// Append a message to the history and bump [`message_rev`](Self::message_rev).
    ///
    /// All mutations of `messages` must go through this method (or explicitly
    /// bump `message_rev`) so that [`layout_cache`](Self::layout_cache)
    /// invalidates correctly.
    fn push_message(&mut self, msg: ChatBlock) {
        self.messages.push(msg);
        self.message_rev = self.message_rev.wrapping_add(1);
    }

    /// Append an error message from outside `App` (e.g. runner startup
    /// failures) while still bumping [`message_rev`](Self::message_rev) so
    /// the layout cache invalidates correctly.
    pub fn push_error(&mut self, text: String) {
        self.push_message(ChatBlock::Error(text));
    }

    /// Handle a terminal keyboard event.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        // Helper: check if key is Ctrl+<english_char>, considering Russian layout.
        // On Russian ЙЦУКЕН layout, physical keys produce different characters.
        let is_ctrl = |c: char| {
            key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char(c)
        };
        // Map English Ctrl shortcuts to both English and Russian layout chars.
        // Russian equivalents (ЙЦУКЕН): T=е, C=с, A=ф, D=в, Y=н, N=т, P=з, Esc=Esc
        let ctrl_key = |en: char, ru: char| is_ctrl(en) || is_ctrl(ru);

        match self.mode {
            AppMode::Normal => match key.code {
                KeyCode::Enter => {
                    let text = self.input.trim().to_string();
                    if !text.is_empty() {
                        // Save to input history (skip duplicates).
                        if self.input_history.last() != Some(&text) {
                            self.input_history.push(text.clone());
                        }
                        self.history_pos = None;
                        if let Some(stripped) = text.strip_prefix('!') {
                            let cmd = stripped.trim().to_string();
                            if !cmd.is_empty() {
                                // Check if this is an SSH connection command.
                                if let Some((user, host, port)) = parse_ssh_command(&cmd) {
                                    self.pending_ssh = Some((
                                        user.clone(),
                                        host.clone(),
                                        port,
                                    ));
                                    self.push_message(ChatBlock::System(format!(
                                        "Connecting to {user}@{host}:{port} via SSH. \
                                         Press Ctrl+P to enter the password."
                                    )));
                                    self.scroll = 0;
                                    self.input.clear();
                                    self.cursor_pos = 0;
                                    // Stay in Normal mode — user needs to press Ctrl+P.
                                } else if is_interactive_command(&cmd) {
                                    // Block interactive commands — they hang the executor.
                                    self.push_message(ChatBlock::System(format!(
                                        "Interactive command '{cmd}' is not supported in shell escape. \
                                         Use Ctrl+T to enter interactive terminal mode."
                                    )));
                                    self.scroll = 0;
                                    self.input.clear();
                                    self.cursor_pos = 0;
                                } else {
                                    // Regular shell escape.
                                    self.push_message(ChatBlock::Command {
                                        command: cmd,
                                        explanation: "Shell escape (direct)".into(),
                                        output: None,
                                        approved: true,
                                    });
                                    self.scroll = 0;
                                    self.input.clear();
                                    self.cursor_pos = 0;
                                    self.mode = AppMode::Thinking;
                                    self.agent_running = true;
                                    self.pending_input = Some(text);
                                }
                            }
                        } else {
                            self.push_message(ChatBlock::User(text.clone()));
                            self.scroll = 0;
                            self.input.clear();
                            self.cursor_pos = 0;
                            self.mode = AppMode::Thinking;
                            self.agent_running = true;
                            self.pending_input = Some(text);
                        }
                    }
                }
                _ if ctrl_key('t', 'е') => {
                    // Toggle to interactive terminal mode.
                    self.toggle_interactive = true;
                }
                _ if ctrl_key('c', 'с') => {
                    self.should_quit = true;
                }
                _ if ctrl_key('p', 'з') => {
                    // Enter secure password input mode.
                    self.input.clear();
                    self.cursor_pos = 0;
                    self.mode = AppMode::PasswordInput;
                    // If there's a pending SSH connection, show a hint.
                    if let Some((user, host, port)) = &self.pending_ssh {
                        self.push_message(ChatBlock::System(format!(
                            "Enter SSH password for {user}@{host}:{port}"
                        )));
                        self.scroll = 0;
                    }
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Any char input cancels history browsing.
                    self.history_pos = None;
                    self.insert_char(c);
                }
                KeyCode::Backspace => {
                    self.history_pos = None;
                    self.backspace_at_cursor();
                }
                KeyCode::Delete => {
                    self.delete_at_cursor();
                }
                KeyCode::Left => {
                    self.cursor_pos = self.cursor_pos.saturating_sub(1);
                }
                KeyCode::Right => {
                    let char_count = self.input.chars().count();
                    self.cursor_pos = (self.cursor_pos + 1).min(char_count);
                }
                KeyCode::Up => {
                    // Browse input history (older).
                    if !self.input_history.is_empty() {
                        if self.history_pos.is_none() {
                            self.saved_input = self.input.clone();
                        }
                        let new_pos = match self.history_pos {
                            None => 0,
                            Some(pos) => (pos + 1).min(self.input_history.len() - 1),
                        };
                        self.history_pos = Some(new_pos);
                        let idx = self.input_history.len() - 1 - new_pos;
                        self.input = self.input_history[idx].clone();
                        self.cursor_pos = self.input.chars().count();
                    }
                }
                KeyCode::Down => {
                    // Browse input history (newer).
                    if let Some(pos) = self.history_pos {
                        if pos == 0 {
                            self.history_pos = None;
                            self.input = self.saved_input.clone();
                            self.cursor_pos = self.input.chars().count();
                        } else {
                            self.history_pos = Some(pos - 1);
                            let idx = self.input_history.len() - pos;
                            self.input = self.input_history[idx].clone();
                            self.cursor_pos = self.input.chars().count();
                        }
                    }
                }
                KeyCode::Home => {
                    self.cursor_pos = 0;
                }
                KeyCode::End => {
                    if self.input.is_empty() {
                        self.scroll = 0;
                    } else {
                        self.cursor_pos = self.input.chars().count();
                    }
                }
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_add(5);
                    self.clamp_scroll();
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_sub(5);
                }
                _ => {}
            },
            AppMode::Thinking => {
                if ctrl_key('c', 'с') {
                    // Cancel the running command and return to Normal mode.
                    self.agent_running = false;
                    self.pending_input = None;
                    self.pending_ssh = None;
                    self.pending_ssh_password = None;
                    self.mode = AppMode::Normal;
                    self.push_message(ChatBlock::System(
                        "Cancelled.".into()
                    ));
                    self.scroll = 0;
                }
                if key.code == KeyCode::PageUp {
                    self.scroll = self.scroll.saturating_add(5);
                    self.clamp_scroll();
                }
                if key.code == KeyCode::PageDown {
                    self.scroll = self.scroll.saturating_sub(5);
                }
                if key.code == KeyCode::End {
                    self.scroll = 0;
                }
            }
            AppMode::Confirming => match key.code {
                KeyCode::Enter
                | KeyCode::Char('a') | KeyCode::Char('y') | KeyCode::Char('e')
                | KeyCode::Char('ф') | KeyCode::Char('н') | KeyCode::Char('у') => {
                    self.respond_to_confirmation(true);
                }
                KeyCode::Char('d') | KeyCode::Char('n')
                | KeyCode::Char('в') | KeyCode::Char('т') => {
                    self.respond_to_confirmation(false);
                }
                KeyCode::End => {
                    self.scroll = 0;
                }
                _ if ctrl_key('c', 'с') => {
                    self.respond_to_confirmation(false);
                    self.should_quit = true;
                }
                _ => {}
            },
            AppMode::Interactive => {
                // Ctrl+T toggles back to agent mode.
                if ctrl_key('t', 'е') {
                    self.toggle_interactive = true;
                    return;
                }
                // Ctrl+C also exits interactive mode.
                if ctrl_key('c', 'с') {
                    self.toggle_interactive = true;
                    return;
                }
                // Convert the key event to terminal input bytes.
                let bytes = key_to_bytes(key);
                if !bytes.is_empty() {
                    // Append to pending input (multiple keys may arrive per loop iteration).
                    match &mut self.pending_term_input {
                        Some(existing) => existing.extend_from_slice(&bytes),
                        None => self.pending_term_input = Some(bytes),
                    }
                }
            }
            AppMode::PasswordInput => match key.code {
                KeyCode::Enter => {
                    let password = self.input.clone();
                    if !password.is_empty() {
                        // Check if this password is for an SSH connection.
                        if self.pending_ssh.is_some() {
                            // SSH password — store for runner to pick up.
                            self.pending_ssh_password = Some(password);
                            self.input.clear();
                            self.cursor_pos = 0;
                            self.mode = AppMode::Thinking;
                            self.agent_running = true;
                        } else {
                            // Regular secret variable — never sent to the LLM.
                            self.secret_counter += 1;
                            let var_name = format!("$FILAR_SECRET_{}", self.secret_counter);
                            if let Ok(mut secrets) = self.secrets.lock() {
                                secrets.insert(var_name.clone(), password);
                            }
                            let agent_msg = format!(
                                "Password provided as secret variable {}. \
                                 Use this variable directly in your commands.",
                                var_name
                            );
                            self.push_message(ChatBlock::System(
                                format!("Password provided as {} (hidden)", var_name)
                            ));
                            self.scroll = 0;
                            self.input.clear();
                            self.cursor_pos = 0;
                            self.mode = AppMode::Thinking;
                            self.agent_running = true;
                            self.pending_input = Some(agent_msg);
                        }
                    }
                }
                KeyCode::Esc => {
                    // Cancel — go back to normal input.
                    self.input.clear();
                    self.cursor_pos = 0;
                    self.mode = AppMode::Normal;
                }
                _ if ctrl_key('c', 'с') => {
                    self.input.clear();
                    self.cursor_pos = 0;
                    self.mode = AppMode::Normal;
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.insert_char(c);
                }
                KeyCode::Backspace => {
                    self.backspace_at_cursor();
                }
                KeyCode::Delete => {
                    self.delete_at_cursor();
                }
                KeyCode::Left => {
                    self.cursor_pos = self.cursor_pos.saturating_sub(1);
                }
                KeyCode::Right => {
                    let char_count = self.input.chars().count();
                    self.cursor_pos = (self.cursor_pos + 1).min(char_count);
                }
                KeyCode::Home => {
                    self.cursor_pos = 0;
                }
                KeyCode::End => {
                    self.cursor_pos = self.input.chars().count();
                }
                _ => {}
            },
        }
    }

    /// Clamp `scroll` so the user cannot scroll past the content.
    ///
    /// Uses the last-known chat area height and cached line count.  Called
    /// after mouse-wheel and PageUp adjustments, and also during render for
    /// a definitive clamp.
    fn clamp_scroll(&mut self) {
        if self.chat_area.height == 0 {
            return;
        }
        let visible_height = self.chat_area.height.saturating_sub(2) as usize;
        let max_scroll = self
            .layout_cache
            .lines
            .len()
            .saturating_sub(visible_height);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    /// Handle a mouse event (scroll wheel inside the chat area).
    ///
    /// Active in all modes except `Interactive` and `PasswordInput`.
    pub fn handle_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;

        // Mouse is only for chat scrolling — ignore in interactive/password modes.
        if self.mode == AppMode::Interactive || self.mode == AppMode::PasswordInput {
            return;
        }

        // Check if the event is inside the chat area (including borders).
        let inside = m.row >= self.chat_area.y
            && m.row < self.chat_area.y + self.chat_area.height
            && m.column >= self.chat_area.x
            && m.column < self.chat_area.x + self.chat_area.width;

        if !inside {
            return;
        }

        match m.kind {
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_add(3);
                self.clamp_scroll();
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_sub(3);
            }
            _ => {}
        }
    }

    /// Respond to a pending confirmation request.
    fn respond_to_confirmation(&mut self, approved: bool) {
        if let Some(confirm) = self.pending_confirm.take() {
            let _ = confirm.respond_to.send(approved);
            self.push_message(ChatBlock::Command {
                command: confirm.command,
                explanation: confirm.explanation,
                output: None,
                approved,
            });
            self.mode = AppMode::Thinking;
        }
    }

    /// Handle an agent event.
    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Started | AgentEvent::Thinking => {
                self.mode = AppMode::Thinking;
            }
            AgentEvent::TextResponse(text) => {
                self.push_message(ChatBlock::Agent(text));
            }
            AgentEvent::ConfirmationRequest {
                command,
                explanation,
                destructive,
                respond_to,
            } => {
                self.pending_confirm = Some(PendingConfirm {
                    command: command.clone(),
                    explanation: explanation.clone(),
                    destructive,
                    respond_to,
                });
                self.mode = AppMode::Confirming;
            }
            AgentEvent::CommandExecuted {
                command,
                output,
                approved,
            } => {
                // Try to update the last matching Command block with output.
                let mut updated = false;
                if let Some(ChatBlock::Command {
                    command: ref cmd,
                    output: ref mut o,
                    approved: ref mut a,
                    ..
                }) = self.messages.last_mut()
                {
                    if *cmd == command && o.is_none() {
                        *o = Some(output.clone());
                        *a = approved;
                        updated = true;
                        // Bump rev so the cache rebuilds with updated output.
                        self.message_rev = self.message_rev.wrapping_add(1);
                    }
                }
                if !updated {
                    self.push_message(ChatBlock::Command {
                        command,
                        explanation: String::new(),
                        output: Some(output),
                        approved,
                    });
                }
            }
            AgentEvent::Finished(text) => {
                if !text.is_empty() {
                    self.push_message(ChatBlock::Agent(text));
                }
                self.mode = AppMode::Normal;
                self.agent_running = false;
            }
            AgentEvent::Error(err) => {
                self.push_message(ChatBlock::Error(err));
                self.mode = AppMode::Normal;
                self.agent_running = false;
            }
            AgentEvent::TransportChanged { .. } => {
                // Handled by the runner before reaching here — no-op.
            }
        }
        // Auto-scroll to bottom on any new content.
        self.scroll = 0;
    }

    /// Take the pending user input (called by the runner to send to the agent).
    pub fn take_input(&mut self) -> Option<String> {
        self.pending_input.take()
    }

    /// Take pending terminal input bytes (called by the runner to write to PTY/SSH).
    pub fn take_term_input(&mut self) -> Option<Vec<u8>> {
        self.pending_term_input.take()
    }

    /// Check and reset the interactive mode toggle flag.
    pub fn take_toggle_interactive(&mut self) -> bool {
        std::mem::take(&mut self.toggle_interactive)
    }

    /// Enter interactive terminal mode with the given terminal model.
    pub fn enter_interactive(&mut self, model: TerminalModel) {
        self.terminal = Some(model);
        self.mode = AppMode::Interactive;
        self.push_message(ChatBlock::System(
            "Entered interactive terminal mode (Ctrl+T to switch back)".into(),
        ));
    }

    /// Exit interactive terminal mode and return to agent chat mode.
    pub fn exit_interactive(&mut self) {
        self.terminal = None;
        self.mode = AppMode::Normal;
        self.push_message(ChatBlock::System(
            "Returned to agent mode".into(),
        ));
    }

    /// Scroll to the bottom (latest messages).
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    // ----- Input editing helpers (char-index based) -----

    /// Insert a character at the cursor position.
    fn insert_char(&mut self, c: char) {
        let byte_pos = self
            .input
            .char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len());
        self.input.insert(byte_pos, c);
        self.cursor_pos += 1;
    }

    /// Delete the character before the cursor (backspace).
    fn backspace_at_cursor(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        self.cursor_pos -= 1;
        let start = self
            .input
            .char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len());
        let end = self
            .input
            .char_indices()
            .nth(self.cursor_pos + 1)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len());
        self.input.replace_range(start..end, "");
    }

    /// Delete the character at the cursor (forward delete).
    fn delete_at_cursor(&mut self) {
        let char_count = self.input.chars().count();
        if self.cursor_pos >= char_count {
            return;
        }
        let start = self
            .input
            .char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len());
        let end = self
            .input
            .char_indices()
            .nth(self.cursor_pos + 1)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len());
        self.input.replace_range(start..end, "");
    }
}

// ---------------------------------------------------------------------------
// SSH command parser
// ---------------------------------------------------------------------------

/// Parse an SSH command like `ssh user@host` or `ssh user@host -p 2222`.
/// Returns `Some((user, host, port))` on success, `None` if not a valid SSH command.
fn parse_ssh_command(cmd: &str) -> Option<(String, String, u16)> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() || parts[0] != "ssh" {
        return None;
    }

    let mut port: u16 = 22;
    let mut user_host: Option<&str> = None;

    let mut i = 1;
    while i < parts.len() {
        if parts[i] == "-p" {
            // Next argument is the port.
            if i + 1 < parts.len() {
                port = parts[i + 1].parse().ok()?;
                i += 2;
            } else {
                return None;
            }
        } else if parts[i].starts_with("-p") {
            // -pPORT format (e.g. -p2222).
            port = parts[i][2..].parse().ok()?;
            i += 1;
        } else if !parts[i].starts_with('-') {
            // First non-flag argument is user@host.
            user_host = Some(parts[i]);
            i += 1;
        } else {
            // Skip unknown flags.
            i += 1;
        }
    }

    let user_host = user_host?;
    let (user, host) = user_host.split_once('@')?;

    if user.is_empty() || host.is_empty() {
        return None;
    }

    Some((user.to_string(), host.to_string(), port))
}

/// Check if a command is interactive (would hang the executor waiting for input).
/// These commands take over the terminal and never produce the expected marker.
fn is_interactive_command(cmd: &str) -> bool {
    let first_word = cmd.split_whitespace().next().unwrap_or("").trim_start_matches("./");
    // Strip path prefix (e.g. /usr/bin/vim → vim).
    let prog = first_word.rsplit('/').next().unwrap_or(first_word);
    matches!(
        prog,
        "vim" | "vi" | "nano" | "emacs" | "pico" | "ed"
        | "top" | "htop" | "btop" | "btop++" | "glances" | "atop"
        | "less" | "more" | "most"
        | "man" | "info"
        | "mc" | "midnight"
        | "screen" | "tmux"
        | "passwd" | "chsh" | "chfn"
        | "telnet" | "ftp" | "sftp" | "nc" | "ncat"
        | "mysql" | "psql" | "sqlite3" | "redis-cli"
        | "python" | "python3" | "node" | "irb" | "pry"
        | "bash" | "sh" | "zsh" | "fish" | "dash"
        | "su" | "sudo"  // sudo can be interactive (e.g. sudo -i)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_new_initializes_correctly() {
        let app = App::new("test-target".into(), CommandConfirmMode::Always);
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.messages.len(), 1); // system message
        assert!(!app.agent_running);
        assert!(!app.should_quit);
    }

    #[test]
    fn app_handle_enter_sends_input() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.input = "hello world".into();

        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));

        assert_eq!(app.mode, AppMode::Thinking);
        assert!(app.agent_running);
        assert!(app.input.is_empty());
        assert_eq!(app.take_input(), Some("hello world".to_string()));
        // User message added to history.
        assert!(matches!(
            &app.messages[1],
            ChatBlock::User(s) if s == "hello world"
        ));
    }

    #[test]
    fn app_ctrl_c_quits() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);

        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        ));

        assert!(app.should_quit);
    }

    #[test]
    fn push_error_bumps_message_rev() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let rev_before = app.message_rev;
        app.push_error("boom".into());
        assert!(app.message_rev > rev_before);
        assert!(matches!(app.messages.last(), Some(ChatBlock::Error(s)) if s == "boom"));
    }

    #[test]
    fn enter_interactive_bumps_message_rev() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let rev_before = app.message_rev;
        let model = crate::terminal::TerminalModel::new(80, 24);
        app.enter_interactive(model);
        assert!(app.message_rev > rev_before);
        assert_eq!(app.mode, AppMode::Interactive);
    }

    #[test]
    fn exit_interactive_bumps_message_rev() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let model = crate::terminal::TerminalModel::new(80, 24);
        app.enter_interactive(model);
        let rev_before = app.message_rev;
        app.exit_interactive();
        assert!(app.message_rev > rev_before);
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn agent_text_response_bumps_message_rev() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let rev_before = app.message_rev;
        app.handle_agent_event(AgentEvent::TextResponse("hello".into()));
        assert!(app.message_rev > rev_before);
    }

    #[test]
    fn agent_error_bumps_message_rev() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let rev_before = app.message_rev;
        app.handle_agent_event(AgentEvent::Error("oops".into()));
        assert!(app.message_rev > rev_before);
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn agent_command_executed_inplace_bumps_message_rev() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        // Push a Command block without output — this is the one that will be
        // updated in-place by CommandExecuted.
        app.push_message(ChatBlock::Command {
            command: "ls".into(),
            explanation: String::new(),
            output: None,
            approved: false,
        });
        let rev_before = app.message_rev;
        app.handle_agent_event(AgentEvent::CommandExecuted {
            command: "ls".into(),
            output: "file1\nfile2".into(),
            approved: true,
        });
        assert!(app.message_rev > rev_before, "in-place update must bump rev");
        // Verify the block was updated in-place, not duplicated.
        assert_eq!(app.messages.len(), 2); // system + command (updated)
    }

    #[test]
    fn confirmation_response_bumps_message_rev() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let (tx, _rx) = oneshot::channel();
        app.pending_confirm = Some(PendingConfirm {
            command: "rm -rf /tmp/test".into(),
            explanation: "cleanup".into(),
            destructive: false,
            respond_to: tx,
        });
        app.mode = AppMode::Confirming;
        let rev_before = app.message_rev;

        // Press 'a' to approve.
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ));

        assert!(app.message_rev > rev_before);
        assert_eq!(app.mode, AppMode::Thinking);
    }

    // ----- Mouse / scroll tests (issue #15) -----

    /// Helper: create a mouse scroll event.
    fn mouse_event(kind: crossterm::event::MouseEventKind, col: u16, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_scroll_up_increases_scroll() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        // Simulate a chat area with content (set chat_area so clamp_scroll works).
        app.chat_area = Rect::new(0, 1, 80, 24); // y=1, height=24
        // Fill cache with enough lines so scroll is possible.
        app.layout_cache.lines = (0..50)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();
        // visible_height = 24 - 2 = 22; max_scroll = 50 - 22 = 28

        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::ScrollUp,
            10,
            10,
        ));
        assert_eq!(app.scroll, 3);
    }

    #[test]
    fn mouse_scroll_down_decreases_scroll() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = (0..50)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();
        app.scroll = 10;

        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::ScrollDown,
            10,
            10,
        ));
        assert_eq!(app.scroll, 7); // 10 - 3 = 7
    }

    #[test]
    fn mouse_scroll_down_clamps_to_zero() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = (0..50)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();
        app.scroll = 2;

        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::ScrollDown,
            10,
            10,
        ));
        assert_eq!(app.scroll, 0); // 2 - 3 saturates to 0
    }

    #[test]
    fn mouse_scroll_up_clamps_to_max() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = (0..30)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();
        // visible_height = 22; max_scroll = 30 - 22 = 8

        // Scroll up many times to exceed max.
        for _ in 0..10 {
            app.handle_mouse(mouse_event(
                crossterm::event::MouseEventKind::ScrollUp,
                10,
                10,
            ));
        }
        assert_eq!(app.scroll, 8); // clamped to max_scroll
    }

    #[test]
    fn mouse_ignored_outside_chat_area() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = (0..50)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();

        // Click outside chat area (row 0 is above chat_area.y=1).
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::ScrollUp,
            10,
            0,
        ));
        assert_eq!(app.scroll, 0); // no change
    }

    #[test]
    fn mouse_ignored_in_interactive_mode() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Interactive;
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = (0..50)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();

        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::ScrollUp,
            10,
            10,
        ));
        assert_eq!(app.scroll, 0); // no change in Interactive mode
    }

    #[test]
    fn end_key_resets_scroll_when_input_empty() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.scroll = 15;
        // Input is empty by default.
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::End,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn end_key_moves_cursor_when_input_nonempty() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.input = "hello".into();
        app.cursor_pos = 0;
        app.scroll = 15;
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::End,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(app.cursor_pos, 5); // cursor at end of "hello"
        assert_eq!(app.scroll, 15); // scroll unchanged
    }

    #[test]
    fn end_key_resets_scroll_in_thinking_mode() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Thinking;
        app.scroll = 20;
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::End,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn end_key_resets_scroll_in_confirming_mode() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let (tx, _rx) = oneshot::channel();
        app.pending_confirm = Some(PendingConfirm {
            command: "ls".into(),
            explanation: "test".into(),
            destructive: false,
            respond_to: tx,
        });
        app.mode = AppMode::Confirming;
        app.scroll = 12;
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::End,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(app.scroll, 0);
        assert_eq!(app.mode, AppMode::Confirming); // still confirming
    }

    #[test]
    fn page_up_clamps_scroll() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = (0..30)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();
        // visible_height = 22; max_scroll = 30 - 22 = 8

        // PageUp many times to exceed max.
        for _ in 0..5 {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageUp,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        // 5 * 5 = 25, clamped to 8
        assert_eq!(app.scroll, 8);
    }
}
