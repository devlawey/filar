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

use std::collections::{HashMap, HashSet};
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
// Mouse hit-testing
// ---------------------------------------------------------------------------

/// Which zone of the UI a mouse click landed on.
///
/// Produced by [`App::hit_test`] to route mouse events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitZone {
    /// Inside the chat content; `line_idx` is the index into `layout_cache.lines`.
    Chat { line_idx: usize },
    /// Inside the chat area but below all content (empty space).
    ChatEmpty,
    /// On the scrollbar track/thumb.
    Scrollbar,
    /// Inside the input field.
    Input,
    /// On the status bar (top line).
    StatusBar,
    /// On the help bar (bottom line).
    HelpBar,
    /// On a confirm dialog button (`true` = approve, `false` = deny).
    ConfirmButton(bool),
    /// On the "↓ N new" scroll indicator.
    ScrollIndicator,
    /// Outside any interactive zone.
    Outside,
}

/// The kind of mouse drag in progress (if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    /// Dragging the scrollbar thumb.
    Scrollbar,
    // `Selection` (text selection) is reserved for future work.
}

/// An action triggered by clicking a help-bar item.
///
/// Each clickable help-bar item stores its `Rect` and associated `HelpAction`
/// in [`App::helpbar_zones`] during render.  When a click lands on the help
/// bar, [`App::handle_mouse`] looks up the zone and executes the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpAction {
    /// Send the current input (Enter in Normal mode).
    Send,
    /// Insert `!` prefix for shell escape.
    Shell,
    /// Toggle interactive terminal mode (Ctrl+T).
    Terminal,
    /// Enter password input mode (Ctrl+P).
    Password,
    /// Quit the application (Ctrl+C) or cancel agent (in Thinking).
    Quit,
    /// Switch confirm selection (Tab).
    Switch,
    /// Confirm with the selected button (Enter in Confirming).
    Confirm,
    /// Approve the command (a/y).
    Approve,
    /// Deny the command (d/n).
    Deny,
    /// Send password (Enter in PasswordInput).
    SendPassword,
    /// Cancel password input (Esc).
    Cancel,
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
    /// Current mouse drag operation (if any).
    pub mouse_drag: Option<DragKind>,
    /// Area of the "↓ N new" indicator (set during render, for click detection).
    pub indicator_area: Rect,
    /// Status bar area (set during render, for hit-testing).
    pub status_bar_area: Rect,
    /// Help bar area (set during render, for hit-testing).
    pub help_bar_area: Rect,
    /// Currently selected confirm button: `false` = Deny (safe default), `true` = Approve.
    pub confirm_selected: bool,
    /// Button under mouse cursor during hover (`Some(true)` = Approve, `Some(false)` = Deny).
    pub hovered_button: Option<bool>,
    /// User-set collapse overrides: block index → is_collapsed.
    /// Blocks not in this map use the default (collapsed if output > 6 lines).
    pub collapsed_overrides: HashMap<usize, bool>,
    /// Whether the agent is currently streaming a text response.
    /// When true, `TextDelta` events append to the last `Agent` block.
    pub streaming: bool,
    /// Spinner animation tick counter — incremented each render frame
    /// while in `Thinking` mode.
    pub tick: u64,
    /// Clickable help-bar zones: (rect, action) filled during render.
    pub helpbar_zones: Vec<(Rect, HelpAction)>,
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
            mouse_drag: None,
            indicator_area: Rect::default(),
            status_bar_area: Rect::default(),
            help_bar_area: Rect::default(),
            confirm_selected: false,
            hovered_button: None,
            collapsed_overrides: HashMap::new(),
            streaming: false,
            tick: 0,
            helpbar_zones: Vec::new(),
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
                KeyCode::Enter => {
                    // Enter activates the selected button (default Deny — safe).
                    self.respond_to_confirmation(self.confirm_selected);
                }
                KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                    // Toggle between Approve and Deny.
                    self.confirm_selected = !self.confirm_selected;
                }
                KeyCode::Char('a') | KeyCode::Char('y') | KeyCode::Char('e')
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

    /// Handle a mouse event (scroll wheel, clicks, drags).
    ///
    /// Help-bar clicks work in all modes; other mouse events are active in
    /// all modes except `Interactive` and `PasswordInput`.
    pub fn handle_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};

        // Help-bar clicks work in ALL modes (including Interactive/Password).
        if m.kind == MouseEventKind::Down(MouseButton::Left) {
            if let HitZone::HelpBar = self.hit_test(m.column, m.row) {
                for (rect, action) in &self.helpbar_zones {
                    if m.column >= rect.x
                        && m.column < rect.x + rect.width
                        && m.row >= rect.y
                        && m.row < rect.y + rect.height
                    {
                        self.execute_help_action(*action);
                        return;
                    }
                }
                return;
            }
        }

        // Other mouse events are not used in interactive/password modes.
        if self.mode == AppMode::Interactive || self.mode == AppMode::PasswordInput {
            return;
        }

        let zone = self.hit_test(m.column, m.row);

        match m.kind {
            // --- Scroll wheel ---
            MouseEventKind::ScrollUp => {
                if matches!(
                    zone,
                    HitZone::Chat { .. } | HitZone::ChatEmpty | HitZone::ScrollIndicator
                ) {
                    self.scroll = self.scroll.saturating_add(3);
                    self.clamp_scroll();
                }
            }
            MouseEventKind::ScrollDown => {
                if matches!(
                    zone,
                    HitZone::Chat { .. } | HitZone::ChatEmpty | HitZone::ScrollIndicator
                ) {
                    self.scroll = self.scroll.saturating_sub(3);
                }
            }
            // --- Left click ---
            MouseEventKind::Down(MouseButton::Left) => match zone {
                HitZone::Scrollbar => {
                    self.mouse_drag = Some(DragKind::Scrollbar);
                    self.update_scrollbar_drag(m.row);
                }
                HitZone::ScrollIndicator => {
                    self.scroll = 0;
                }
                HitZone::Input if self.mode == AppMode::Normal => {
                    self.set_cursor_from_click(m.column, m.row);
                }
                HitZone::ConfirmButton(approve) => {
                    self.respond_to_confirmation(approve);
                }
                HitZone::Chat { line_idx } => {
                    // Click on OutputToggle or Command header → toggle collapse.
                    if let Some(rl) = self.layout_cache.lines.get(line_idx) {
                        match rl.region {
                            crate::ui::layout_cache::LineRegion::OutputToggle => {
                                if let Some(block_idx) = rl.block_index {
                                    self.toggle_collapse(block_idx);
                                }
                            }
                            crate::ui::layout_cache::LineRegion::Header => {
                                if let Some(block_idx) = rl.block_index {
                                    // Only toggle for Command blocks with output.
                                    if matches!(
                                        self.messages.get(block_idx),
                                        Some(ChatBlock::Command { output: Some(_), .. })
                                    ) {
                                        self.toggle_collapse(block_idx);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            // --- Drag ---
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.mouse_drag == Some(DragKind::Scrollbar) {
                    self.update_scrollbar_drag(m.row);
                }
            }
            // --- Mouse up ---
            MouseEventKind::Up(MouseButton::Left) => {
                self.mouse_drag = None;
            }
            // --- Hover (track which button is under cursor) ---
            MouseEventKind::Moved => {
                if let HitZone::ConfirmButton(approve) = zone {
                    self.hovered_button = Some(approve);
                    self.confirm_selected = approve;
                } else {
                    self.hovered_button = None;
                }
            }
            _ => {}
        }
    }

    /// Execute the action associated with a help-bar click.
    fn execute_help_action(&mut self, action: HelpAction) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        match action {
            HelpAction::Send => {
                self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
            }
            HelpAction::Shell => {
                // Insert `!` prefix if input is empty.
                if self.mode == AppMode::Normal && self.input.is_empty() {
                    self.insert_char('!');
                }
            }
            HelpAction::Terminal => {
                self.toggle_interactive = true;
            }
            HelpAction::Password => {
                if self.mode == AppMode::Normal {
                    self.input.clear();
                    self.cursor_pos = 0;
                    self.mode = AppMode::PasswordInput;
                    if let Some((user, host, port)) = &self.pending_ssh {
                        self.push_message(ChatBlock::System(format!(
                            "Enter SSH password for {user}@{host}:{port}"
                        )));
                        self.scroll = 0;
                    }
                }
            }
            HelpAction::Quit => match self.mode {
                AppMode::Normal => self.should_quit = true,
                AppMode::Thinking => {
                    self.agent_running = false;
                    self.pending_input = None;
                    self.pending_ssh = None;
                    self.pending_ssh_password = None;
                    self.mode = AppMode::Normal;
                    self.push_message(ChatBlock::System("Cancelled.".into()));
                    self.scroll = 0;
                }
                AppMode::Confirming => {
                    self.respond_to_confirmation(false);
                    self.should_quit = true;
                }
                AppMode::Interactive => {
                    self.toggle_interactive = true;
                }
                AppMode::PasswordInput => {
                    self.input.clear();
                    self.cursor_pos = 0;
                    self.mode = AppMode::Normal;
                }
            },
            HelpAction::Switch => {
                if self.mode == AppMode::Confirming {
                    self.confirm_selected = !self.confirm_selected;
                }
            }
            HelpAction::Confirm => {
                if self.mode == AppMode::Confirming {
                    self.respond_to_confirmation(self.confirm_selected);
                }
            }
            HelpAction::Approve => {
                if self.mode == AppMode::Confirming {
                    self.respond_to_confirmation(true);
                }
            }
            HelpAction::Deny => {
                if self.mode == AppMode::Confirming {
                    self.respond_to_confirmation(false);
                }
            }
            HelpAction::SendPassword => {
                if self.mode == AppMode::PasswordInput {
                    self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
                }
            }
            HelpAction::Cancel => {
                if self.mode == AppMode::PasswordInput {
                    self.input.clear();
                    self.cursor_pos = 0;
                    self.mode = AppMode::Normal;
                }
            }
        }
    }

    /// Determine which UI zone a screen coordinate falls into.
    ///
    /// Uses the last-known areas (filled during render).  The caller is
    /// responsible for acting on the result.
    fn hit_test(&self, col: u16, row: u16) -> HitZone {
        // --- Confirm buttons (check first — modal overlays everything) ---
        for (rect, approved) in &self.confirm_button_areas {
            if col >= rect.x
                && col < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height
            {
                return HitZone::ConfirmButton(*approved);
            }
        }

        // --- ↓ N new indicator (overlays the chat area) ---
        if self.indicator_area.width > 0
            && col >= self.indicator_area.x
            && col < self.indicator_area.x + self.indicator_area.width
            && row >= self.indicator_area.y
            && row < self.indicator_area.y + self.indicator_area.height
        {
            return HitZone::ScrollIndicator;
        }

        // --- Scrollbar (rightmost column of chat area, inside borders) ---
        let visible_height = self.chat_area.height.saturating_sub(2) as usize;
        let total_lines = self.layout_cache.lines.len();
        let scrollbar_visible = total_lines > visible_height;
        if scrollbar_visible
            && self.chat_area.width > 0
            && col == self.chat_area.x + self.chat_area.width - 1
            && row > self.chat_area.y
            && row < self.chat_area.y + self.chat_area.height - 1
        {
            return HitZone::Scrollbar;
        }

        // --- Chat content (inside borders, excluding scrollbar column) ---
        if self.chat_area.width > 2
            && self.chat_area.height > 2
            && col > self.chat_area.x
            && col < self.chat_area.x + self.chat_area.width - 1
            && row > self.chat_area.y
            && row < self.chat_area.y + self.chat_area.height - 1
        {
            let inner_row = (row - self.chat_area.y - 1) as usize;
            let skip = if total_lines > visible_height {
                total_lines.saturating_sub(visible_height + self.scroll)
            } else {
                0
            };
            let line_idx = skip + inner_row;
            if line_idx < total_lines {
                return HitZone::Chat { line_idx };
            } else {
                return HitZone::ChatEmpty;
            }
        }

        // --- Input field ---
        if self.input_area.width > 0
            && col >= self.input_area.x
            && col < self.input_area.x + self.input_area.width
            && row >= self.input_area.y
            && row < self.input_area.y + self.input_area.height
        {
            return HitZone::Input;
        }

        // --- Status bar ---
        if self.status_bar_area.width > 0
            && col >= self.status_bar_area.x
            && col < self.status_bar_area.x + self.status_bar_area.width
            && row >= self.status_bar_area.y
            && row < self.status_bar_area.y + self.status_bar_area.height
        {
            return HitZone::StatusBar;
        }

        // --- Help bar ---
        if self.help_bar_area.width > 0
            && col >= self.help_bar_area.x
            && col < self.help_bar_area.x + self.help_bar_area.width
            && row >= self.help_bar_area.y
            && row < self.help_bar_area.y + self.help_bar_area.height
        {
            return HitZone::HelpBar;
        }

        // --- Confirm buttons (checked at top of hit_test — see above) ---

        HitZone::Outside
    }

    /// Update scroll position from a scrollbar drag at the given row.
    ///
    /// Maps the row proportionally: top of track → scroll = max (top of
    /// content), bottom → scroll = 0 (bottom/latest).
    fn update_scrollbar_drag(&mut self, row: u16) {
        if self.chat_area.height == 0 {
            return;
        }
        let visible_height = self.chat_area.height.saturating_sub(2) as usize;
        let total_lines = self.layout_cache.lines.len();
        let max_scroll = total_lines.saturating_sub(visible_height);
        if max_scroll == 0 || visible_height == 0 {
            return;
        }
        let track_top = self.chat_area.y + 1; // inside top border
        let relative_row = (row.saturating_sub(track_top)) as usize;
        // Track spans rows 0..=visible_height-1.  Divide by (visible_height - 1)
        // so the bottom row maps to skip=max_scroll → scroll=0.
        let track_span = (visible_height - 1).max(1);
        let skip = relative_row * max_scroll / track_span;
        self.scroll = max_scroll.saturating_sub(skip).min(max_scroll);
    }

    /// Set cursor position from a click in the input area.
    ///
    /// Reverses the `place_cursor` math: `cursor_pos = row * inner_width + col`.
    fn set_cursor_from_click(&mut self, col: u16, row: u16) {
        if self.input_area.width == 0 {
            return;
        }
        let inner_x = self.input_area.x + 1;
        let inner_y = self.input_area.y + 1;
        let inner_width = (self.input_area.width.saturating_sub(2)).max(1) as usize;

        let relative_col = (col.saturating_sub(inner_x)) as usize;
        let relative_row = (row.saturating_sub(inner_y)) as usize;

        let char_count = self.input.chars().count();
        let pos = relative_row * inner_width + relative_col;
        self.cursor_pos = pos.min(char_count);
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
            // Clear modal hit-test state so stale button areas don't swallow clicks.
            self.confirm_button_areas.clear();
            self.hovered_button = None;
        }
    }

    /// Compute the default collapse state for a chat block.
    /// A Command block is collapsed by default if its output has more than 6 lines.
    fn default_collapsed_for(msg: &ChatBlock) -> bool {
        matches!(msg, ChatBlock::Command { output: Some(out), .. } if out.lines().count() > 6)
    }

    /// Compute the set of collapsed block indices from `collapsed_overrides`
    /// and defaults.  `collapsed_overrides` can force either state.
    pub fn collapsed_set(&self) -> HashSet<usize> {
        self.messages
            .iter()
            .enumerate()
            .filter_map(|(idx, msg)| {
                let is_collapsed = self
                    .collapsed_overrides
                    .get(&idx)
                    .copied()
                    .unwrap_or_else(|| Self::default_collapsed_for(msg));
                if is_collapsed { Some(idx) } else { None }
            })
            .collect()
    }

    /// Toggle the collapse state of a command block.
    /// Bumps `message_rev` so the layout cache rebuilds.
    fn toggle_collapse(&mut self, block_idx: usize) {
        let is_collapsed = self
            .collapsed_overrides
            .get(&block_idx)
            .copied()
            .unwrap_or_else(|| {
                self.messages
                    .get(block_idx)
                    .is_some_and(Self::default_collapsed_for)
            });
        self.collapsed_overrides.insert(block_idx, !is_collapsed);
        self.message_rev = self.message_rev.wrapping_add(1);
    }

    /// Return the current spinner character based on `tick`.
    ///
    /// Uses braille frames in modern terminals (Windows Terminal),
    /// ASCII fallback (`|/-\`) in conhost.
    pub fn spinner_char(&self) -> &'static str {
        static IS_WT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let is_wt = *IS_WT.get_or_init(|| std::env::var("WT_SESSION").is_ok());
        const BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        const ASCII: &[&str] = &["|", "/", "-", "\\"];
        let frames = if is_wt { BRAILLE } else { ASCII };
        frames[(self.tick as usize) % frames.len()]
    }

    /// Handle an agent event.
    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        let mut auto_scroll = true;
        match event {
            AgentEvent::Started | AgentEvent::Thinking => {
                self.mode = AppMode::Thinking;
            }
            AgentEvent::TextDelta(s) => {
                // Append to the last Agent block if streaming, else create new.
                if self.streaming {
                    if let Some(ChatBlock::Agent(ref mut text)) = self.messages.last_mut() {
                        text.push_str(&s);
                    } else {
                        self.push_message(ChatBlock::Agent(s));
                    }
                } else {
                    self.push_message(ChatBlock::Agent(s));
                    self.streaming = true;
                }
                self.message_rev = self.message_rev.wrapping_add(1);
                // Only auto-scroll if already at bottom.
                // If the user scrolled up, don’t yank them down.
                auto_scroll = self.scroll == 0;
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
                // Finalize any streaming text before showing the dialog.
                self.streaming = false;
                auto_scroll = self.scroll == 0;
                self.pending_confirm = Some(PendingConfirm {
                    command: command.clone(),
                    explanation: explanation.clone(),
                    destructive,
                    respond_to,
                });
                self.mode = AppMode::Confirming;
                // Reset selection to safe default (Deny).
                self.confirm_selected = false;
            }
            AgentEvent::CommandExecuted {
                command,
                output,
                approved,
            } => {
                // Finalize any streaming text before showing command output.
                self.streaming = false;
                auto_scroll = self.scroll == 0;
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
                // Finalize streaming block with authoritative text.
                if self.streaming {
                    if !text.is_empty() {
                        if let Some(ChatBlock::Agent(ref mut existing)) = self.messages.last_mut() {
                            *existing = text;
                            self.message_rev = self.message_rev.wrapping_add(1);
                        } else {
                            self.push_message(ChatBlock::Agent(text));
                        }
                    }
                    self.streaming = false;
                    auto_scroll = self.scroll == 0;
                } else if !text.is_empty() {
                    self.push_message(ChatBlock::Agent(text));
                }
                self.mode = AppMode::Normal;
                self.agent_running = false;
            }
            AgentEvent::Error(err) => {
                // If streaming was interrupted, mark it.
                if self.streaming {
                    self.push_message(ChatBlock::System("response interrupted".into()));
                    self.streaming = false;
                    auto_scroll = self.scroll == 0;
                }
                self.push_message(ChatBlock::Error(err));
                self.mode = AppMode::Normal;
                self.agent_running = false;
            }
            AgentEvent::TransportChanged { .. } => {
                // Handled by the runner before reaching here — no-op.
            }
        }
        // Auto-scroll to bottom on new content (unless user scrolled up during streaming).
        if auto_scroll {
            self.scroll = 0;
        }
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

    // ----- Hit-testing tests (issue #16) -----

    /// Helper: set up an app with a chat area and cached lines for hit-testing.
    fn make_hit_test_app() -> App {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        // Chat area: x=0, y=1, w=80, h=24 (inner: 78x22)
        app.chat_area = Rect::new(0, 1, 80, 24);
        // Input area: x=0, y=26, w=80, h=5 (inner: 78x3)
        app.input_area = Rect::new(0, 26, 80, 5);
        // Status bar: y=0, h=1
        app.status_bar_area = Rect::new(0, 0, 80, 1);
        // Help bar: y=31, h=1
        app.help_bar_area = Rect::new(0, 31, 80, 1);
        // 50 cached lines → scrollbar visible (50 > 22)
        app.layout_cache.lines = (0..50)
            .map(|i| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw(format!("line {i}")),
                block_index: Some(i),
                region: crate::ui::layout_cache::LineRegion::Body,
            })
            .collect();
        // scroll = 0 → bottom; skip = 50 - 22 = 28
        app
    }

    #[test]
    fn hit_test_chat_content() {
        let app = make_hit_test_app();
        // Click at col=5, row=2 (inside chat, first content row)
        // skip = 28, inner_row = 0, line_idx = 28
        let zone = app.hit_test(5, 2);
        assert_eq!(zone, HitZone::Chat { line_idx: 28 });
    }

    #[test]
    fn hit_test_chat_empty_below_content() {
        let mut app = make_hit_test_app();
        // Only 5 lines → no overflow, scrollbar hidden.
        app.layout_cache.lines.truncate(5);
        app.scroll = 0;
        // Click at row=20 (inner_row=19), but only 5 lines total → ChatEmpty
        let zone = app.hit_test(5, 20);
        assert_eq!(zone, HitZone::ChatEmpty);
    }

    #[test]
    fn hit_test_scrollbar() {
        let app = make_hit_test_app();
        // Scrollbar = rightmost column of chat area (col=79), inside borders (row 2..23)
        let zone = app.hit_test(79, 10);
        assert_eq!(zone, HitZone::Scrollbar);
    }

    #[test]
    fn hit_test_scrollbar_not_visible_when_content_fits() {
        let mut app = make_hit_test_app();
        // Only 5 lines → fits in visible_height=22, no scrollbar.
        app.layout_cache.lines.truncate(5);
        // Click at rightmost column → should be chat border, not scrollbar.
        // But col=79 is the right border, so it's not inside chat content either.
        // hit_test returns Outside for border clicks when no scrollbar.
        let zone = app.hit_test(79, 10);
        assert_eq!(zone, HitZone::Outside);
    }

    #[test]
    fn hit_test_input() {
        let app = make_hit_test_app();
        // Click inside the input area.
        let zone = app.hit_test(5, 27);
        assert_eq!(zone, HitZone::Input);
    }

    #[test]
    fn hit_test_status_bar() {
        let app = make_hit_test_app();
        let zone = app.hit_test(5, 0);
        assert_eq!(zone, HitZone::StatusBar);
    }

    #[test]
    fn hit_test_help_bar() {
        let app = make_hit_test_app();
        let zone = app.hit_test(5, 31);
        assert_eq!(zone, HitZone::HelpBar);
    }

    #[test]
    fn hit_test_outside() {
        let app = make_hit_test_app();
        // Click way outside any area.
        let zone = app.hit_test(200, 200);
        assert_eq!(zone, HitZone::Outside);
    }

    #[test]
    fn hit_test_scroll_indicator() {
        let mut app = make_hit_test_app();
        // Set up a fake indicator area inside the chat area.
        app.indicator_area = Rect::new(70, 22, 8, 1);
        let zone = app.hit_test(72, 22);
        assert_eq!(zone, HitZone::ScrollIndicator);
    }

    #[test]
    fn hit_test_line_idx_with_scroll() {
        let mut app = make_hit_test_app();
        // scroll=10 → skip = 50 - 22 - 10 = 18
        app.scroll = 10;
        // Click at row=2 (inner_row=0) → line_idx = 18
        let zone = app.hit_test(5, 2);
        assert_eq!(zone, HitZone::Chat { line_idx: 18 });
    }

    // ----- Scrollbar drag tests -----

    #[test]
    fn scrollbar_drag_sets_scroll_proportionally() {
        let mut app = make_hit_test_app();
        // visible_height = 22, max_scroll = 50 - 22 = 28
        // Drag to top of track (row=2, inner_row=0):
        // skip = 0 * 28 / 22 = 0, scroll = 28 - 0 = 28 (top)
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            79,
            2,
        ));
        assert_eq!(app.scroll, 28);
        assert_eq!(app.mouse_drag, Some(DragKind::Scrollbar));

        // Drag to bottom of track (row=23, inner_row=21):
        // track_span = 22 - 1 = 21, skip = 21 * 28 / 21 = 28, scroll = 28 - 28 = 0
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            79,
            23,
        ));
        assert_eq!(app.scroll, 0, "drag to bottom should reach scroll=0");
    }

    #[test]
    fn scrollbar_mouse_up_clears_drag() {
        let mut app = make_hit_test_app();
        app.mouse_drag = Some(DragKind::Scrollbar);
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            79,
            10,
        ));
        assert_eq!(app.mouse_drag, None);
    }

    // ----- Click indicator → scroll = 0 -----

    #[test]
    fn click_indicator_resets_scroll() {
        let mut app = make_hit_test_app();
        app.scroll = 15;
        app.indicator_area = Rect::new(70, 22, 8, 1);
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            72,
            22,
        ));
        assert_eq!(app.scroll, 0);
    }

    // ----- Click input → cursor_pos -----

    #[test]
    fn click_input_sets_cursor() {
        let mut app = make_hit_test_app();
        app.mode = AppMode::Normal;
        app.input = "hello world test".into(); // 16 chars
        // input_area = x=0, y=26, w=80, h=5 → inner_x=1, inner_y=27, inner_width=78
        // Click at col=3, row=27 → relative_col=2, relative_row=0 → cursor_pos=2
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            3,
            27,
        ));
        assert_eq!(app.cursor_pos, 2);
    }

    #[test]
    fn click_input_second_row_sets_cursor() {
        let mut app = make_hit_test_app();
        app.mode = AppMode::Normal;
        // 80 chars → wraps to 2 lines at inner_width=78
        app.input = "a".repeat(80);
        // Click at col=1, row=28 (second row of input, relative_row=1)
        // cursor_pos = 1 * 78 + 0 = 78
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            1,
            28,
        ));
        assert_eq!(app.cursor_pos, 78);
    }

    #[test]
    fn click_input_clamps_to_end() {
        let mut app = make_hit_test_app();
        app.mode = AppMode::Normal;
        app.input = "hi".into(); // 2 chars
        // Click far right → cursor_pos clamped to 2
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            70,
            27,
        ));
        assert_eq!(app.cursor_pos, 2);
    }

    #[test]
    fn click_input_ignored_in_thinking_mode() {
        let mut app = make_hit_test_app();
        app.mode = AppMode::Thinking;
        app.input = "hello".into();
        app.cursor_pos = 0;
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            3,
            27,
        ));
        assert_eq!(app.cursor_pos, 0); // no change in Thinking mode
    }

    // ----- Confirm modal tests (issue #17) -----

    /// Helper: set up an app in Confirming mode with a pending confirmation.
    fn make_confirm_app(destructive: bool) -> App {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let (_tx, _rx) = oneshot::channel::<bool>();
        // We need the tx to stay alive so respond_to doesn't fail silently;
        // but for tests we just check mode/message changes.
        // Use a fresh sender that we drop to simulate a real channel.
        let (tx, _rx2) = oneshot::channel::<bool>();
        app.pending_confirm = Some(PendingConfirm {
            command: "rm -rf /tmp/test".into(),
            explanation: "cleanup".into(),
            destructive,
            respond_to: tx,
        });
        app.mode = AppMode::Confirming;
        app.confirm_selected = false; // safe default
        app
    }

    /// Helper: create a key event.
    fn key_event(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn confirm_selected_defaults_to_deny() {
        let app = make_confirm_app(false);
        assert!(!app.confirm_selected, "default should be Deny (false)");
    }

    #[test]
    fn tab_toggles_confirm_selected() {
        let mut app = make_confirm_app(false);
        assert!(!app.confirm_selected);
        app.handle_key(key_event(crossterm::event::KeyCode::Tab));
        assert!(app.confirm_selected, "Tab should toggle to Approve");
        app.handle_key(key_event(crossterm::event::KeyCode::Tab));
        assert!(!app.confirm_selected, "Tab should toggle back to Deny");
    }

    #[test]
    fn left_arrow_toggles_confirm_selected() {
        let mut app = make_confirm_app(false);
        app.handle_key(key_event(crossterm::event::KeyCode::Left));
        assert!(app.confirm_selected);
        app.handle_key(key_event(crossterm::event::KeyCode::Left));
        assert!(!app.confirm_selected);
    }

    #[test]
    fn right_arrow_toggles_confirm_selected() {
        let mut app = make_confirm_app(false);
        app.handle_key(key_event(crossterm::event::KeyCode::Right));
        assert!(app.confirm_selected);
    }

    #[test]
    fn enter_activates_selected_default_deny() {
        let mut app = make_confirm_app(false);
        // Default is Deny → Enter should deny.
        app.handle_key(key_event(crossterm::event::KeyCode::Enter));
        assert_eq!(app.mode, AppMode::Thinking);
        assert!(app.pending_confirm.is_none());
        // Last message should be a Command with approved=false.
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(!*approved, "Enter on default Deny should deny");
        } else {
            panic!("expected Command block");
        }
    }

    #[test]
    fn enter_after_tab_activates_approve() {
        let mut app = make_confirm_app(false);
        app.handle_key(key_event(crossterm::event::KeyCode::Tab));
        assert!(app.confirm_selected);
        app.handle_key(key_event(crossterm::event::KeyCode::Enter));
        assert_eq!(app.mode, AppMode::Thinking);
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(*approved, "Enter after Tab should approve");
        } else {
            panic!("expected Command block");
        }
    }

    #[test]
    fn letter_a_approves_directly() {
        let mut app = make_confirm_app(false);
        app.handle_key(key_event(crossterm::event::KeyCode::Char('a')));
        assert_eq!(app.mode, AppMode::Thinking);
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(*approved);
        } else {
            panic!("expected Command block");
        }
    }

    #[test]
    fn letter_d_denies_directly() {
        let mut app = make_confirm_app(false);
        app.handle_key(key_event(crossterm::event::KeyCode::Char('d')));
        assert_eq!(app.mode, AppMode::Thinking);
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(!*approved);
        } else {
            panic!("expected Command block");
        }
    }

    #[test]
    fn russian_layout_approve() {
        let mut app = make_confirm_app(false);
        // ф = a in ЙЦУКЕН layout
        app.handle_key(key_event(crossterm::event::KeyCode::Char('ф')));
        assert_eq!(app.mode, AppMode::Thinking);
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(*approved);
        } else {
            panic!("expected Command block");
        }
    }

    #[test]
    fn russian_layout_deny() {
        let mut app = make_confirm_app(false);
        // в = d in ЙЦУКЕН layout
        app.handle_key(key_event(crossterm::event::KeyCode::Char('в')));
        assert_eq!(app.mode, AppMode::Thinking);
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(!*approved);
        } else {
            panic!("expected Command block");
        }
    }

    #[test]
    fn ctrl_c_denies_and_quits() {
        let mut app = make_confirm_app(false);
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert!(app.should_quit);
        assert_eq!(app.mode, AppMode::Thinking);
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(!*approved, "Ctrl+C should deny");
        } else {
            panic!("expected Command block");
        }
    }

    #[test]
    fn confirm_selected_resets_on_new_request() {
        let mut app = make_confirm_app(false);
        // Toggle to Approve.
        app.handle_key(key_event(crossterm::event::KeyCode::Tab));
        assert!(app.confirm_selected);
        // Simulate a new confirmation request.
        let (tx, _rx) = oneshot::channel::<bool>();
        app.handle_agent_event(AgentEvent::ConfirmationRequest {
            command: "ls".into(),
            explanation: "list".into(),
            destructive: false,
            respond_to: tx,
        });
        assert!(!app.confirm_selected, "new request should reset to Deny");
    }

    #[test]
    fn mouse_click_approve_button() {
        let mut app = make_confirm_app(false);
        // Simulate button areas (set during render).
        // Approve button at col 20-34, row 10.
        app.confirm_button_areas.push((Rect::new(20, 10, 15, 1), true));
        // Deny button at col 38-50, row 10.
        app.confirm_button_areas.push((Rect::new(38, 10, 13, 1), false));
        // Click on Approve.
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            25,
            10,
        ));
        assert_eq!(app.mode, AppMode::Thinking);
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(*approved, "clicking Approve should approve");
        } else {
            panic!("expected Command block");
        }
    }

    #[test]
    fn mouse_click_deny_button() {
        let mut app = make_confirm_app(false);
        app.confirm_button_areas.push((Rect::new(20, 10, 15, 1), true));
        app.confirm_button_areas.push((Rect::new(38, 10, 13, 1), false));
        // Click on Deny.
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            42,
            10,
        ));
        assert_eq!(app.mode, AppMode::Thinking);
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(!*approved, "clicking Deny should deny");
        } else {
            panic!("expected Command block");
        }
    }

    #[test]
    fn mouse_hover_updates_selected() {
        let mut app = make_confirm_app(false);
        app.confirm_button_areas.push((Rect::new(20, 10, 15, 1), true));
        app.confirm_button_areas.push((Rect::new(38, 10, 13, 1), false));
        // Hover over Approve.
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Moved,
            25,
            10,
        ));
        assert_eq!(app.hovered_button, Some(true));
        assert!(app.confirm_selected, "hover on Approve should move selection");
        // Hover over Deny.
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Moved,
            42,
            10,
        ));
        assert_eq!(app.hovered_button, Some(false));
        assert!(!app.confirm_selected);
        // Hover outside buttons.
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Moved,
            0,
            0,
        ));
        assert_eq!(app.hovered_button, None);
    }

    #[test]
    fn hit_test_confirm_button_overrides_chat() {
        let mut app = make_hit_test_app();
        app.mode = AppMode::Confirming;
        // Place a confirm button over the chat area.
        app.confirm_button_areas.push((Rect::new(5, 5, 15, 1), true));
        // Hit-test at a point inside the button — should be ConfirmButton, not Chat.
        let zone = app.hit_test(7, 5);
        assert_eq!(zone, HitZone::ConfirmButton(true));
    }

    #[test]
    fn confirm_state_cleared_after_response() {
        let mut app = make_confirm_app(false);
        // Populate button areas as if rendered.
        app.confirm_button_areas.push((Rect::new(20, 10, 15, 1), true));
        app.confirm_button_areas.push((Rect::new(38, 10, 13, 1), false));
        app.hovered_button = Some(true);

        // Deny via keyboard.
        app.handle_key(key_event(crossterm::event::KeyCode::Char('d')));

        // After response, modal state should be cleared.
        assert!(app.confirm_button_areas.is_empty(), "button areas should be cleared");
        assert_eq!(app.hovered_button, None, "hovered_button should be cleared");

        // Hit-test in the former button area should NOT return ConfirmButton.
        let zone = app.hit_test(25, 10);
        assert!(
            !matches!(zone, HitZone::ConfirmButton(_)),
            "stale button area should not swallow clicks after modal closes"
        );
    }

    // ---- Collapse / expand tests (issue #18) ----

    fn make_command_app(output_lines: usize) -> App {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        let output = (0..output_lines)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.messages.push(ChatBlock::Command {
            command: "test".into(),
            explanation: "".into(),
            output: Some(output),
            approved: true,
        });
        app
    }

    #[test]
    fn collapsed_set_defaults_long_output_collapsed() {
        let app = make_command_app(50);
        let collapsed = app.collapsed_set();
        assert!(collapsed.contains(&0), "50-line output should be collapsed by default");
    }

    #[test]
    fn collapsed_set_defaults_short_output_not_collapsed() {
        let app = make_command_app(3);
        let collapsed = app.collapsed_set();
        assert!(!collapsed.contains(&0), "3-line output should not be collapsed by default");
    }

    #[test]
    fn collapsed_set_respects_expand_override() {
        let mut app = make_command_app(50);
        app.collapsed_overrides.insert(0, false);
        let collapsed = app.collapsed_set();
        assert!(!collapsed.contains(&0), "override=false should expand even long output");
    }

    #[test]
    fn collapsed_set_respects_collapse_override() {
        let mut app = make_command_app(3);
        app.collapsed_overrides.insert(0, true);
        let collapsed = app.collapsed_set();
        assert!(collapsed.contains(&0), "override=true should collapse even short output");
    }

    #[test]
    fn toggle_collapse_from_default_collapsed_to_expanded() {
        let mut app = make_command_app(50);
        assert!(app.collapsed_set().contains(&0));
        // Simulate toggle via the private method's logic.
        app.collapsed_overrides.insert(0, false);
        assert!(!app.collapsed_set().contains(&0));
    }

    #[test]
    fn toggle_collapse_from_default_expanded_to_collapsed() {
        let mut app = make_command_app(3);
        assert!(!app.collapsed_set().contains(&0));
        app.collapsed_overrides.insert(0, true);
        assert!(app.collapsed_set().contains(&0));
    }

    // ---- Mouse click routing tests (issue #18 review) ----

    #[test]
    fn mouse_click_output_toggle_toggles_collapse() {
        let mut app = make_command_app(50);
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = vec![
            crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("header"),
                block_index: Some(0),
                region: crate::ui::layout_cache::LineRegion::Header,
            },
            crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("toggle"),
                block_index: Some(0),
                region: crate::ui::layout_cache::LineRegion::OutputToggle,
            },
        ];
        // Click the OutputToggle line (row 3 → line_idx 1).
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            3,
        ));
        assert!(
            app.collapsed_overrides.contains_key(&0),
            "OutputToggle click should toggle collapse"
        );
    }

    #[test]
    fn mouse_click_command_header_with_output_toggles_collapse() {
        let mut app = make_command_app(50);
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = vec![
            crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("header"),
                block_index: Some(0),
                region: crate::ui::layout_cache::LineRegion::Header,
            },
        ];
        // Click the Header line (row 2 → line_idx 0).
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            2,
        ));
        assert!(
            app.collapsed_overrides.contains_key(&0),
            "Header click should toggle collapse for Command with output"
        );
    }

    #[test]
    fn mouse_click_command_header_without_output_no_toggle() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.messages.push(ChatBlock::Command {
            command: "pending".into(),
            explanation: "".into(),
            output: None,
            approved: false,
        });
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = vec![
            crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("header"),
                block_index: Some(0),
                region: crate::ui::layout_cache::LineRegion::Header,
            },
        ];
        // Click the Header line — should NOT toggle (no output).
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            2,
        ));
        assert!(
            !app.collapsed_overrides.contains_key(&0),
            "Header click should not toggle for Command without output"
        );
    }

    #[test]
    fn mouse_click_body_does_not_toggle_collapse() {
        let mut app = make_command_app(50);
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = vec![
            crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("header"),
                block_index: Some(0),
                region: crate::ui::layout_cache::LineRegion::Header,
            },
            crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("body"),
                block_index: Some(0),
                region: crate::ui::layout_cache::LineRegion::Body,
            },
        ];
        // Click the Body line (row 3 → line_idx 1).
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            3,
        ));
        assert!(
            !app.collapsed_overrides.contains_key(&0),
            "Body click should not toggle collapse"
        );
    }

    // ── Streaming tests ─────────────────────────────────────────────────

    #[test]
    fn text_delta_creates_new_agent_block() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.mode = AppMode::Thinking;

        app.handle_agent_event(AgentEvent::TextDelta("Hello".into()));

        assert!(app.streaming);
        assert_eq!(app.messages.len(), 1);
        match &app.messages[0] {
            ChatBlock::Agent(text) => assert_eq!(text, "Hello"),
            _ => panic!("expected Agent block"),
        }
    }

    #[test]
    fn text_delta_appends_when_streaming() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.mode = AppMode::Thinking;

        app.handle_agent_event(AgentEvent::TextDelta("Hello".into()));
        app.handle_agent_event(AgentEvent::TextDelta(" world".into()));

        assert!(app.streaming);
        assert_eq!(app.messages.len(), 1);
        match &app.messages[0] {
            ChatBlock::Agent(text) => assert_eq!(text, "Hello world"),
            _ => panic!("expected Agent block"),
        }
    }

    #[test]
    fn finished_finalizes_streaming_block() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.mode = AppMode::Thinking;

        // Stream some text.
        app.handle_agent_event(AgentEvent::TextDelta("Partial".into()));
        app.handle_agent_event(AgentEvent::TextDelta(" response".into()));
        assert!(app.streaming);

        // Finished replaces with authoritative text.
        app.handle_agent_event(AgentEvent::Finished(
            "Partial response — finalized".into(),
        ));

        assert!(!app.streaming);
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.messages.len(), 1);
        match &app.messages[0] {
            ChatBlock::Agent(text) => assert_eq!(text, "Partial response — finalized"),
            _ => panic!("expected Agent block"),
        }
    }

    #[test]
    fn finished_empty_text_keeps_streaming_block() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.mode = AppMode::Thinking;

        app.handle_agent_event(AgentEvent::TextDelta("Streamed text".into()));
        app.handle_agent_event(AgentEvent::Finished(String::new()));

        assert!(!app.streaming);
        assert_eq!(app.messages.len(), 1);
        match &app.messages[0] {
            ChatBlock::Agent(text) => assert_eq!(text, "Streamed text"),
            _ => panic!("expected Agent block"),
        }
    }

    #[test]
    fn error_during_stream_adds_interrupted_marker() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.mode = AppMode::Thinking;

        app.handle_agent_event(AgentEvent::TextDelta("Partial".into()));
        assert!(app.streaming);

        app.handle_agent_event(AgentEvent::Error("network error".into()));

        assert!(!app.streaming);
        assert_eq!(app.mode, AppMode::Normal);
        // Should have: Agent block (partial) + System (interrupted) + Error.
        assert_eq!(app.messages.len(), 3);
        assert!(matches!(&app.messages[1], ChatBlock::System(s) if s == "response interrupted"));
        assert!(matches!(&app.messages[2], ChatBlock::Error(_)));
    }

    #[test]
    fn confirmation_request_finalizes_streaming() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.mode = AppMode::Thinking;

        app.handle_agent_event(AgentEvent::TextDelta("Let me check...".into()));
        assert!(app.streaming);

        let (tx, _rx) = oneshot::channel::<bool>();
        app.handle_agent_event(AgentEvent::ConfirmationRequest {
            command: "ls".into(),
            explanation: "list files".into(),
            destructive: false,
            respond_to: tx,
        });

        assert!(!app.streaming);
        assert_eq!(app.mode, AppMode::Confirming);
    }

    #[test]
    fn text_delta_no_autoscroll_when_user_scrolled_up() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.mode = AppMode::Thinking;
        app.scroll = 5; // User scrolled up.

        app.handle_agent_event(AgentEvent::TextDelta("new text".into()));

        // Scroll should NOT be reset to 0 — user is reading history.
        assert_eq!(app.scroll, 5);
    }

    #[test]
    fn text_delta_autoscroll_when_at_bottom() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.mode = AppMode::Thinking;
        app.scroll = 0;

        app.handle_agent_event(AgentEvent::TextDelta("new text".into()));

        // Scroll stays at 0 (bottom).
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn spinner_char_cycles() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let c0 = app.spinner_char();
        app.tick = app.tick.wrapping_add(1);
        let c1 = app.spinner_char();
        app.tick = app.tick.wrapping_add(1);
        let c2 = app.spinner_char();

        // At least some frames should differ.
        // (In braille mode all 10 are unique; in ASCII 4 are unique.)
        assert_ne!(c0, c1, "spinner should advance");
        assert_ne!(c1, c2, "spinner should advance");
    }

    #[test]
    fn streaming_resets_on_new_agent_run() {
        // After Finished, a new TextDelta should create a new block (not append to old).
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.mode = AppMode::Thinking;

        app.handle_agent_event(AgentEvent::TextDelta("First".into()));
        app.handle_agent_event(AgentEvent::Finished("First".into()));
        assert!(!app.streaming);

        // New run.
        app.handle_agent_event(AgentEvent::Thinking);
        app.handle_agent_event(AgentEvent::TextDelta("Second".into()));

        assert!(app.streaming);
        assert_eq!(app.messages.len(), 2);
        match &app.messages[1] {
            ChatBlock::Agent(text) => assert_eq!(text, "Second"),
            _ => panic!("expected second Agent block"),
        }
    }

    // --- HelpAction and helpbar_zones tests ---

    #[test]
    fn helpbar_zones_init_empty() {
        let app = App::new("test".into(), CommandConfirmMode::Always);
        assert!(app.helpbar_zones.is_empty());
    }

    #[test]
    fn help_action_quit_in_normal_mode() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        assert!(!app.should_quit);
        app.execute_help_action(HelpAction::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn help_action_quit_in_thinking_cancels() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Thinking;
        app.agent_running = true;
        app.execute_help_action(HelpAction::Quit);
        assert_eq!(app.mode, AppMode::Normal);
        assert!(!app.agent_running);
    }

    #[test]
    fn help_action_terminal_toggles() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        assert!(!app.toggle_interactive);
        app.execute_help_action(HelpAction::Terminal);
        assert!(app.toggle_interactive);
    }

    #[test]
    fn help_action_password_enters_mode() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.execute_help_action(HelpAction::Password);
        assert_eq!(app.mode, AppMode::PasswordInput);
    }

    #[test]
    fn help_action_shell_inserts_bang() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        assert!(app.input.is_empty());
        app.execute_help_action(HelpAction::Shell);
        assert_eq!(app.input, "!");
    }

    #[test]
    fn help_action_shell_does_not_overwrite_nonempty() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.input = "hello".into();
        app.execute_help_action(HelpAction::Shell);
        assert_eq!(app.input, "hello");
    }

    #[test]
    fn help_action_approve_denies_in_confirming() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.mode = AppMode::Confirming;
        app.pending_confirm = Some(PendingConfirm {
            command: "ls".into(),
            explanation: "list".into(),
            destructive: false,
            respond_to: tx,
        });
        app.execute_help_action(HelpAction::Approve);
        assert_eq!(app.mode, AppMode::Thinking);
        assert!(rx.try_recv().unwrap());
    }

    #[test]
    fn help_action_deny_in_confirming() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.mode = AppMode::Confirming;
        app.pending_confirm = Some(PendingConfirm {
            command: "rm".into(),
            explanation: "remove".into(),
            destructive: true,
            respond_to: tx,
        });
        app.execute_help_action(HelpAction::Deny);
        assert_eq!(app.mode, AppMode::Thinking);
        assert!(!rx.try_recv().unwrap());
    }

    #[test]
    fn help_action_cancel_in_password_mode() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::PasswordInput;
        app.input = "secret".into();
        app.execute_help_action(HelpAction::Cancel);
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.input.is_empty());
    }

    #[test]
    fn help_action_switch_toggles_confirm_selected() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Confirming;
        assert!(!app.confirm_selected); // default = Deny
        app.execute_help_action(HelpAction::Switch);
        assert!(app.confirm_selected);
        app.execute_help_action(HelpAction::Switch);
        assert!(!app.confirm_selected);
    }
}
