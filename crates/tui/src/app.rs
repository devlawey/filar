//! Application state for the TUI.
//!
//! [`App`] holds the chat history, current input, mode, and pending
//! confirmation requests. It is updated by both terminal events (keyboard)
//! and agent events (from the agent task).

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use filar_core::{ChatBlock, CommandConfirmMode, StaticSecretProvider};
use ratatui::layout::Rect;

use crate::event::TuiEvent;
use crate::terminal::{key_to_bytes, TerminalModel};
use crate::ui::layout_cache::ChatLayoutCache;
use crate::ui::Theme;

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use std::time::Duration;

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
    /// Dragging to select text in the chat area.
    Selection,
}

/// A text selection in the chat area.
///
/// Coordinates are in `layout_cache.lines` index space (not screen space),
/// so the selection survives scrolling.  `anchor` is where the mouse went
/// down; `head` tracks the current drag position.  Normalised order is
/// computed at render/copy time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Line index where the selection started.
    pub anchor_line: usize,
    /// Column (char offset within the rendered line) where the selection started.
    pub anchor_col: usize,
    /// Line index of the current selection head (follows the mouse).
    pub head_line: usize,
    /// Column of the current selection head.
    pub head_col: usize,
}

impl Selection {
    /// Return `(start, end)` as normalised `(line, col)` pairs where
    /// `start <= end` lexicographically.
    pub fn normalised(&self) -> ((usize, usize), (usize, usize)) {
        let a = (self.anchor_line, self.anchor_col);
        let h = (self.head_line, self.head_col);
        if a <= h { (a, h) } else { (h, a) }
    }

    /// Whether the selection is empty (anchor == head).
    pub fn is_empty(&self) -> bool {
        self.anchor_line == self.head_line && self.anchor_col == self.head_col
    }
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
    /// Quit the application (Ctrl+Q). In Confirming denies first; in Thinking
    /// cancels the running agent; then shuts down gracefully.
    Quit,
    /// Cancel the current work (Ctrl+Z): stop the agent in Thinking or deny in
    /// Confirming, without quitting.
    CancelWork,
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
    /// All open sessions (tabs). The first session is created on startup.
    pub sessions: Vec<Session>,
    /// Index of the currently active session in `sessions`.
    pub active: usize,
    /// Command confirmation mode.
    pub confirm_mode: CommandConfirmMode,
    /// Set to true when the user wants to quit.
    pub should_quit: bool,
    /// Shared secret provider: $FILAR_SECRET_N → actual value.
    pub secrets: Arc<StaticSecretProvider>,
    /// Pending SSH connection: (user, host, port) parsed from `!ssh user@host`.
    pub pending_ssh: Option<(String, String, u16)>,
    /// Pending SSH password entered by the user via Ctrl+P.
    pub pending_ssh_password: Option<String>,
    /// Colour theme used by the UI renderer.
    pub theme: Theme,
    /// Status bar area (set during render, for hit-testing).
    pub status_bar_area: Rect,
    /// Help bar area (set during render, for hit-testing).
    pub help_bar_area: Rect,
}

/// Stable identifier for a session tab. Assigned once on creation, never
/// reused. Events carry this id so they can be dispatched to the originating
/// session even when the active tab changes or intermediate tabs close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub(crate) u64);

/// Global counter for unique SessionIds. Atomic so it can be incremented
/// from any context (runner, UI) without locking.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

impl SessionId {
    fn next() -> Self {
        SessionId(NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Per-tab session state — everything that is independent per open tab.
pub struct Session {
    /// Stable session identifier (never reused).
    pub id: SessionId,
    /// Display name shown on the tab label.
    pub target_name: String,
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
    /// Pending confirmation request (when mode == Confirming).
    pub pending_confirm: Option<PendingConfirm>,
    /// Whether the agent task is currently running.
    pub agent_running: bool,
    /// Pending user input to be sent to the agent.
    pending_input: Option<String>,
    /// Interactive terminal model (when in interactive mode).
    pub terminal: Option<TerminalModel>,
    /// Pending terminal input bytes (from key events, to be written to PTY/SSH).
    pending_term_input: Option<Vec<u8>>,
    /// Flag: user pressed Ctrl+T to toggle between agent and interactive modes.
    pub toggle_interactive: bool,
    /// Counter for the next secret variable name.
    pub secret_counter: usize,
    /// History of all user inputs (for Up/Down navigation).
    input_history: Vec<String>,
    /// Current position in history browsing (None = not browsing).
    history_pos: Option<usize>,
    /// Saved input when user starts browsing history.
    saved_input: String,
    /// Cached chat layout — avoids re-wrapping text on every frame.
    pub layout_cache: ChatLayoutCache,
    /// Revision counter — bumped on any mutation of `messages`.
    pub message_rev: u64,
    /// Actual chat area on screen (filled during render, for hit-testing).
    pub chat_area: Rect,
    /// Terminal grid area in interactive mode (filled during render).
    pub terminal_area: Rect,
    /// Actual input area on screen (filled during render, for hit-testing).
    pub input_area: Rect,
    /// Confirm button areas (filled later, for mouse click detection).
    pub confirm_button_areas: Vec<(Rect, bool)>,
    /// Whether the agent is currently streaming a text response.
    pub streaming: bool,
    /// Pending command proposal metadata from `CommandProposed`.
    pub pending_proposal: Option<(String, String)>,
    /// Spinner animation tick counter — incremented each render frame.
    pub tick: u64,
    /// Clickable help-bar zones: (rect, action) filled during render.
    pub helpbar_zones: Vec<(Rect, HelpAction)>,
    /// Input scroll offset (set during render when input exceeds 5 lines).
    pub input_scroll_offset: usize,
    /// Current text selection in the chat area (if any).
    pub selection: Option<Selection>,
    /// Toast notification: `(text, expiry)`.
    pub toast: Option<(String, Instant)>,
    /// Current mouse drag operation (if any).
    pub mouse_drag: Option<DragKind>,
    /// Area of the "↓ N new" indicator (set during render, for click detection).
    pub indicator_area: Rect,
    /// Currently selected confirm button: `false` = Deny (safe default), `true` = Approve.
    pub confirm_selected: bool,
    /// Button under mouse cursor during hover.
    pub hovered_button: Option<bool>,
    /// User-set collapse overrides: block index → is_collapsed.
    pub collapsed_overrides: HashMap<usize, bool>,
    /// Cancellation token for the currently running agent task.
    pub cancellation: Option<CancellationToken>,
    /// Timestamp of the last mouse-down in the chat area.
    last_click_time: Option<Instant>,
    /// Position of the last mouse-down in the chat area.
    last_click_pos: Option<(usize, usize)>,
    /// Current click count (1=single, 2=double, 3=triple).
    click_count: u8,
    /// Base text of the last forwarded log line (for dedup).
    last_log_text: Option<String>,
    /// Count of consecutive identical forwarded log lines (for `… xN`).
    last_log_count: usize,
    /// True when the agent is running in this session (even if not active).
    pub background_activity: bool,
    /// True when new output arrived since the user last viewed this tab.
    pub has_new: bool,
    /// True when a confirmation is pending (agent is waiting for user input).
    pub awaiting_confirmation: bool,
}

impl App {
    /// Get a reference to the active session.
    pub fn active_session(&self) -> &Session {
        &self.sessions[self.active]
    }
    /// Get a mutable reference to the active session.
    pub fn active_session_mut(&mut self) -> &mut Session {
        &mut self.sessions[self.active]
    }

    /// Create a new app with the given target name and confirmation mode.
    pub fn new(target_name: String, confirm_mode: CommandConfirmMode) -> Self {
        let session = Session::new(target_name, confirm_mode);
        Self {
            sessions: vec![session],
            active: 0,
            confirm_mode,
            should_quit: false,
            secrets: Arc::new(StaticSecretProvider::new()),
            pending_ssh: None,
            pending_ssh_password: None,
            theme: Theme::default_dark(),
            status_bar_area: Rect::default(),
            help_bar_area: Rect::default(),
        }
    }

    /// Create a new session tab in local mode, inheriting target_name display.
    pub fn new_tab(&mut self) {
        let name = format!("local-{}", self.sessions.len() + 1);
        let session = Session::new(name, self.confirm_mode);
        self.sessions.push(session);
        self.active = self.sessions.len() - 1;
    }

    /// Close the active tab. If it's the last tab, set should_quit.
    pub fn close_tab(&mut self) {
        if self.sessions.len() <= 1 {
            self.should_quit = true;
            return;
        }
        // Cancel the agent task for the tab being closed so leftover
        // events don't land on the next active session.
        if let Some(ref token) = self.sessions[self.active].cancellation {
            token.cancel();
        }
        self.sessions.remove(self.active);
        if self.active >= self.sessions.len() {
            self.active = self.sessions.len() - 1;
        }
    }

    /// Switch to the previous tab (wraps around).
    pub fn prev_tab(&mut self) {
        let prev = if self.active == 0 {
            self.sessions.len() - 1
        } else {
            self.active - 1
        };
        self.sessions[prev].has_new = false;
        self.active = prev;
    }

    /// Switch to the next tab (wraps around).
    pub fn next_tab(&mut self) {
        let next = (self.active + 1) % self.sessions.len();
        self.sessions[next].has_new = false;
        self.active = next;
    }

    /// Switch to tab at index (1-based from user, clamped).
    pub fn switch_to_tab(&mut self, index: usize) {
        let idx = index.saturating_sub(1).min(self.sessions.len().saturating_sub(1));
        if idx != self.active {
            // Clear "has new" flag on the tab being switched to.
            self.sessions[idx].has_new = false;
        }
        self.active = idx;
    }

    /// Find the index of a session by its stable id.
    pub fn find_session_idx(&self, id: SessionId) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == id)
    }
}

// App delegates per-session field access to the active session via Deref.
// This avoids touching ~300+ field references in the existing code while
// enabling multi-session support through self.sessions + self.active.
impl Deref for App {
    type Target = Session;
    fn deref(&self) -> &Self::Target {
        &self.sessions[self.active]
    }
}
impl DerefMut for App {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sessions[self.active]
    }
}

impl Session {
    pub fn new(target_name: String, confirm_mode: CommandConfirmMode) -> Self {
        let name = target_name.clone();
        Self {
            id: SessionId::next(),
            target_name,
            messages: vec![ChatBlock::System(format!(
                "Connected to: {name} | Mode: {confirm_mode:?}"
            ))],
            input: String::new(),
            cursor_pos: 0,
            mode: AppMode::Normal,
            scroll: 0,
            pending_confirm: None,
            agent_running: false,
            pending_input: None,
            terminal: None,
            pending_term_input: None,
            toggle_interactive: false,
            secret_counter: 0,
            input_history: Vec::new(),
            history_pos: None,
            saved_input: String::new(),
            layout_cache: ChatLayoutCache::new(),
            message_rev: 0,
            chat_area: Rect::default(),
            terminal_area: Rect::default(),
            input_area: Rect::default(),
            confirm_button_areas: Vec::new(),
            mouse_drag: None,
            indicator_area: Rect::default(),
            confirm_selected: false,
            hovered_button: None,
            collapsed_overrides: HashMap::new(),
            cancellation: None,
            streaming: false,
            pending_proposal: None,
            tick: 0,
            helpbar_zones: Vec::new(),
            input_scroll_offset: 0,
            selection: None,
            toast: None,
            last_click_time: None,
            last_click_pos: None,
            click_count: 0,
            last_log_text: None,
            last_log_count: 0,
            background_activity: false,
            has_new: false,
            awaiting_confirmation: false,
        }
    }
}

impl App {
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
        // New message invalidates line indices — clear any active selection.
        self.selection = None;
        // Any non-log message breaks a run of identical forwarded log lines.
        self.last_log_text = None;
    }

    /// Push a WARN/ERROR log line (from [`crate::log_layer`]) into the chat as
    /// a `System` block.
    ///
    /// Keeps the chat readable: the line is clamped to a single line no wider
    /// than the chat area, and consecutive identical lines collapse into a
    /// single block with a `… xN` counter instead of repeating.
    pub fn push_system_log(&mut self, line: String) {
        // Chat width for clamping (fallback before the first render).
        let width = if self.chat_area.width > 0 {
            self.chat_area.width as usize
        } else {
            120
        };
        // Clamp to a single line no wider than the chat area. Keeps a burst of
        // a long log line from reflowing the whole chat.
        let clamp = |s: &str| -> String {
            if s.chars().count() > width {
                let keep = width.saturating_sub(1).max(1);
                s.chars().take(keep).collect::<String>() + "…"
            } else {
                s.to_string()
            }
        };

        // Dedup key is the *full* normalized line (untruncated), so distinct
        // long messages that merely share a prefix don't collapse together.
        let normalized: String = line.replace(['\n', '\r'], " ");

        // Collapse a run of identical lines into `… xN`. The rendered string —
        // suffix included — is clamped to the chat width.
        if self.last_log_text.as_deref() == Some(normalized.as_str()) {
            self.last_log_count += 1;
            let count = self.last_log_count;
            if let Some(ChatBlock::System(s)) = self.messages.last_mut() {
                *s = clamp(&format!("{normalized} … x{count}"));
                self.message_rev = self.message_rev.wrapping_add(1);
                self.selection = None;
                return;
            }
        }

        // `push_message` resets `last_log_text`, so set the run state after it.
        self.push_message(ChatBlock::System(clamp(&normalized)));
        self.last_log_text = Some(normalized);
        self.last_log_count = 1;
    }

    /// Append an error message from outside `App` (e.g. runner startup
    /// failures) while still bumping [`message_rev`](Self::message_rev) so
    /// the layout cache invalidates correctly.
    pub fn push_error(&mut self, text: String) {
        self.push_message(ChatBlock::Error(text));
    }

    /// Handle a terminal keyboard event.
    /// Quit the application gracefully (Ctrl+Q) from any non-Interactive mode.
    ///
    /// Mirrors the old Ctrl+C quit: a pending confirmation is denied first
    /// (Confirming) and a running agent is cancelled (Thinking) so shutdown is
    /// clean, then `should_quit` triggers teardown + session save in the runner.
    fn quit(&mut self) {
        match self.mode {
            AppMode::Confirming => {
                self.respond_to_confirmation(false);
            }
            AppMode::Thinking => {
                if let Some(ref token) = self.cancellation {
                    token.cancel();
                }
                self.cancellation = None;
            }
            _ => {}
        }
        self.should_quit = true;
    }

    /// Cancel the current work (Ctrl+Z) without quitting.
    ///
    /// - Thinking: cancel the running agent (token → `Cancelled` event, partial
    ///   answer stays) and return to Normal.
    /// - Confirming: deny the pending command (stay in the app).
    /// - Other modes: no-op.
    fn cancel_work(&mut self) {
        match self.mode {
            AppMode::Thinking => {
                if let Some(ref token) = self.cancellation {
                    token.cancel();
                }
                self.cancellation = None;
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
            }
            _ => {}
        }
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        // Helper: check if key is Ctrl+<english_char>, considering Russian layout.
        // On Russian ЙЦУКЕН layout, physical keys produce different characters.
        let is_ctrl = |c: char| {
            key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char(c)
        };
        // Map English Ctrl shortcuts to both English and Russian layout chars.
        // Russian equivalents (ЙЦУКЕН): T=е, C=с, A=ф, D=в, Y=н, N=т, P=з,
        // Q=й, Z=я, Esc=Esc
        let ctrl_key = |en: char, ru: char| is_ctrl(en) || is_ctrl(ru);

        // Global control hotkeys — active in every mode EXCEPT Interactive, where
        // all keys (including ^Q/^Z/^C) are forwarded to the remote PTY.
        //
        // Ctrl+C is intentionally NOT bound anywhere: users strongly associate it
        // with "copy", so an accidental press must do nothing. Quit is ^Q, cancel
        // is ^Z (both with Russian-layout equivalents Й/Я).
        if self.mode != AppMode::Interactive {
            if ctrl_key('q', 'й') {
                self.quit();
                return;
            }
            if ctrl_key('z', 'я') {
                self.cancel_work();
                return;
            }
            // Tab navigation — active in all non-Interactive modes.
            if ctrl_key('n', 'т') {
                self.new_tab();
                return;
            }
            if ctrl_key('w', 'ц') {
                // Ctrl+W closes the active tab (if > 1; last tab quits).
                self.close_tab();
                return;
            }
            if key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::CONTROL) {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.prev_tab();
                } else {
                    self.next_tab();
                }
                return;
            }
            if key.code == KeyCode::BackTab {
                self.prev_tab();
                return;
            }
            // Ctrl+PageDown / Ctrl+PageUp — alternative tab switching.
            if key.code == KeyCode::PageDown
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                self.next_tab();
                return;
            }
            if key.code == KeyCode::PageUp
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                self.prev_tab();
                return;
            }
            // Ctrl+1..9 — direct tab switch.
            if let KeyCode::Char(c) = key.code {
                if key.modifiers.contains(KeyModifiers::CONTROL) && ('1'..='9').contains(&c) {
                    let idx = (c as u8 - b'1') as usize + 1;
                    self.switch_to_tab(idx);
                    return;
                }
            }
        }

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
                    // If the active session already has a live terminal,
                    // show it instead of creating a new one.
                    if self.terminal.is_some() {
                        self.show_interactive_view();
                    } else {
                        self.toggle_interactive = true;
                    }
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
                _ => {}
            },
            AppMode::Interactive => {
                // Ctrl+T toggles the terminal view: hides the terminal while
                // keeping the PTY alive in the background. To fully close the
                // terminal, close the tab (Ctrl+W) or exit the session.
                if ctrl_key('t', 'е') {
                    self.hide_interactive_view();
                    return;
                }
                // Ctrl+N — new tab (local). Intercepted always: the new tab
                // starts in agent mode, the current terminal stays alive in
                // the background. No toggle_interactive — old PTY untouched.
                if ctrl_key('n', 'т') {
                    self.new_tab();
                    return;
                }
                // Ctrl+W — close the active tab. Last tab quits. Intercepted
                // always: the terminal is torn down inside close_tab.
                if ctrl_key('w', 'ц') {
                    self.close_tab();
                    return;
                }
                // Tab navigation when multiple tabs are open: switch tab and
                // exit interactive mode (global PTY, not per-session). Single
                // tab → let keys fall through to PTY unchanged.
                if self.sessions.len() > 1 {
                    let switch = match (key.code, key.modifiers) {
                        (KeyCode::Tab, m) if m.contains(KeyModifiers::CONTROL) => {
                            if m.contains(KeyModifiers::SHIFT) {
                                self.prev_tab();
                            } else {
                                self.next_tab();
                            }
                            true
                        }
                        (KeyCode::BackTab, _) => {
                            self.prev_tab();
                            true
                        }
                        (KeyCode::PageDown, m) if m.contains(KeyModifiers::CONTROL) => {
                            self.next_tab();
                            true
                        }
                        (KeyCode::PageUp, m) if m.contains(KeyModifiers::CONTROL) => {
                            self.prev_tab();
                            true
                        }
                        _ => false,
                    };
                    if switch {
                        return;
                    }
                }
                // PgUp/PgDn — scroll through terminal history (scrollback)
                // when in primary screen. In alt-screen (vim/htop/less)
                // these keys are forwarded to the PTY so the remote
                // application receives them — matching mouse wheel logic.
                // Ctrl+PageUp/Ctrl+PageDown are NOT intercepted here: with
                // a single session they pass to the PTY, with multiple
                // sessions the tab-switch gate above consumes them.
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && (key.code == KeyCode::PageUp || key.code == KeyCode::PageDown)
                {
                    if let Some(t) = self.terminal.as_mut() {
                        if !t.is_alt_screen() {
                            let rows = t.rows() as i32;
                            if key.code == KeyCode::PageUp {
                                t.scroll_display(rows.max(1));
                            } else {
                                t.scroll_display(-rows.max(1));
                            }
                            return;
                        }
                    }
                }
                // Convert the key event to terminal input bytes.
                let bytes = key_to_bytes(key);
                if !bytes.is_empty() {
                    // Reset scrollback to bottom on keyboard input.
                    if let Some(t) = self.terminal.as_mut() {
                        t.scroll_to_bottom();
                    }
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
                            self.secrets.insert(var_name.clone(), password);
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
        // Borderless layout: full height is the visible height.
        let visible_height = self.chat_area.height as usize;
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

        // Mouse events in Interactive mode are handled separately.
        if self.mode == AppMode::Interactive {
            self.handle_interactive_mouse(m);
            return;
        }

        // No mouse events in PasswordInput mode.
        if self.mode == AppMode::PasswordInput {
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
                    // For non-collapsing headers (User, Agent, etc.), fall through
                    // to the text-selection path so users can select header text.
                    if let Some(rl) = self.layout_cache.lines.get(line_idx) {
                        match rl.region {
                            crate::ui::layout_cache::LineRegion::OutputToggle => {
                                if let Some(block_idx) = rl.block_index {
                                    self.toggle_collapse(block_idx);
                                }
                                return;
                            }
                            crate::ui::layout_cache::LineRegion::Header => {
                                if let Some(block_idx) = rl.block_index {
                                    // Only toggle for Command blocks with output.
                                    if matches!(
                                        self.messages.get(block_idx),
                                        Some(ChatBlock::Command { output: Some(_), .. })
                                    ) {
                                        self.toggle_collapse(block_idx);
                                        return;
                                    }
                                }
                                // Non-collapsing header — fall through to selection.
                            }
                            _ => {}
                        }
                    }
                    // --- Text selection ---
                    let char_col = (m.column.saturating_sub(self.chat_area.x)) as usize;
                    // Detect double/triple click (< 400 ms, same position).
                    let now = Instant::now();
                    let is_repeat = self.last_click_time.is_some_and(|t| now.duration_since(t) < Duration::from_millis(400))
                        && self.last_click_pos == Some((line_idx, char_col));
                    if is_repeat {
                        self.click_count = (self.click_count % 3) + 1;
                    } else {
                        self.click_count = 1;
                    }
                    self.last_click_time = Some(now);
                    self.last_click_pos = Some((line_idx, char_col));

                    match self.click_count {
                        2 => {
                            // Double click — select word.
                            self.select_word(line_idx, char_col);
                            self.mouse_drag = Some(DragKind::Selection);
                        }
                        3 => {
                            // Triple click — select line.
                            self.select_line(line_idx);
                            self.mouse_drag = Some(DragKind::Selection);
                        }
                        _ => {
                            // Single click — start char selection.
                            self.selection = Some(Selection {
                                anchor_line: line_idx, anchor_col: char_col,
                                head_line: line_idx, head_col: char_col,
                            });
                            self.mouse_drag = Some(DragKind::Selection);
                        }
                    }
                }
                _ => {}
            },
            // --- Drag ---
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.mouse_drag == Some(DragKind::Scrollbar) {
                    self.update_scrollbar_drag(m.row);
                } else if self.mouse_drag == Some(DragKind::Selection) {
                    // Update selection head to current mouse position.
                    if let Some((line_idx, char_col)) = self.screen_to_line_col(m.column, m.row) {
                        if let Some(sel) = &mut self.selection {
                            sel.head_line = line_idx;
                            sel.head_col = char_col;
                        }
                    }
                    // Auto-scroll when dragging near the top or bottom edge.
                    if self.chat_area.height > 0 {
                        let edge = m.row;
                        let top = self.chat_area.y;
                        let bottom = self.chat_area.y + self.chat_area.height - 1;
                        if edge <= top {
                            self.scroll = self.scroll.saturating_add(1);
                            self.clamp_scroll();
                        } else if edge >= bottom {
                            self.scroll = self.scroll.saturating_sub(1);
                        }
                    }
                }
            }
            // --- Mouse up ---
            MouseEventKind::Up(MouseButton::Left) => {
                if self.mouse_drag == Some(DragKind::Selection) {
                    // If selection is empty (click without drag), clear it.
                    if self.selection.as_ref().is_some_and(|s| s.is_empty()) {
                        self.selection = None;
                    } else {
                        // Copy on select (non-empty selection).
                        self.copy_selection_to_clipboard();
                    }
                }
                self.mouse_drag = None;
            }
            // --- Hover (track which button is under cursor) ---
            // NOTE: hover only updates visual highlighting — it must NOT
            // change confirm_selected, so the Enter safety-default (Deny)
            // is preserved until the user explicitly toggles via keyboard.
            MouseEventKind::Moved => {
                if let HitZone::ConfirmButton(approve) = zone {
                    self.hovered_button = Some(approve);
                } else {
                    self.hovered_button = None;
                }
            }
            _ => {}
        }
    }

    /// Handle a mouse event in Interactive terminal mode.
    ///
    /// If the terminal application has requested mouse events (SGR mode),
    /// all mouse events are encoded as SGR sequences and forwarded to the
    /// terminal input.  Otherwise, the scroll wheel either scrolls the
    /// scrollback history (primary screen) or translates to arrow keys
    /// (alternate screen, e.g. `less`, `man`).
    fn handle_interactive_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;

        let area = self.terminal_area;
        if area.width == 0 || area.height == 0 {
            return;
        }

        // Detect scrollbar column: rightmost column of the terminal area.
        // Mouse events on the scrollbar are intercepted before forwarding to PTY.
        let scrollbar_col = area.x + area.width - 1;
        let on_scrollbar = m.column == scrollbar_col
            && m.row >= area.y
            && m.row < area.y + area.height;

        let dragging_scrollbar = self.mouse_drag == Some(DragKind::Scrollbar);
        if on_scrollbar || dragging_scrollbar {
            match m.kind {
                MouseEventKind::Down(_) if on_scrollbar => {
                    self.mouse_drag = Some(DragKind::Scrollbar);
                    self.terminal_scrollbar_drag(m.row);
                    return;
                }
                MouseEventKind::Drag(_) if dragging_scrollbar => {
                    self.terminal_scrollbar_drag(m.row);
                    return;
                }
                MouseEventKind::Up(_) if dragging_scrollbar => {
                    self.mouse_drag = None;
                    return;
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    // Wheel on scrollbar: handle as scroll (fall through to
                    // the scroll-wheel branch below, which already works).
                }
                _ => return,
            }
        }

        // --- rest of existing handler (outside terminal area, mouse mode, wheel) ---

        // Ignore events outside the terminal area (scrollbar already handled above).
        if m.column < area.x
            || m.column >= area.x + area.width
            || m.row < area.y
            || m.row >= area.y + area.height
        {
            return;
        }

        // 1-based coordinates relative to the terminal area (SGR convention).
        let x = (m.column - area.x + 1) as usize;
        let y = (m.row - area.y + 1) as usize;

        let mouse_mode = self.terminal.as_ref().is_some_and(|t| t.mouse_mode());
        let sgr_mouse = self.terminal.as_ref().is_some_and(|t| t.sgr_mouse());
        let alt_screen = self.terminal.as_ref().is_some_and(|t| t.is_alt_screen());

        if mouse_mode {
            // Forward mouse events to the terminal.
            if sgr_mouse {
                // SGR encoding: \x1b[<{button};{x};{y}M/m
                if let Some(seq) = encode_sgr_mouse(&m, x, y) {
                    self.push_term_input(&seq);
                }
            } else {
                // Legacy encoding: \x1b[M followed by 3 bytes (button+32, x+32, y+32).
                // Coordinates are clamped to 255 (max for legacy format).
                if let Some(seq) = encode_legacy_mouse(&m, x, y) {
                    self.push_term_input(&seq);
                }
            }
            return;
        }

        // No mouse mode — handle scroll wheel only.
        match m.kind {
            MouseEventKind::ScrollUp => {
                if alt_screen {
                    // Translate wheel to arrow keys (3 per tick).
                    let arrows = b"\x1b[A\x1b[A\x1b[A";
                    self.push_term_input(arrows);
                } else if let Some(t) = self.terminal.as_mut() {
                    t.scroll_display(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if alt_screen {
                    let arrows = b"\x1b[B\x1b[B\x1b[B";
                    self.push_term_input(arrows);
                } else if let Some(t) = self.terminal.as_mut() {
                    t.scroll_display(-3);
                }
            }
            _ => {}
        }
    }

    /// Append bytes to the pending terminal input buffer.
    fn push_term_input(&mut self, bytes: &[u8]) {
        match &mut self.pending_term_input {
            Some(existing) => existing.extend_from_slice(bytes),
            None => self.pending_term_input = Some(bytes.to_vec()),
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
            HelpAction::Quit => {
                if self.mode == AppMode::Interactive {
                    // No quit from interactive — Ctrl+T returns to agent mode.
                    self.toggle_interactive = true;
                } else {
                    self.quit();
                }
            }
            HelpAction::CancelWork => {
                self.cancel_work();
            }
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

        // --- Scrollbar (rightmost column of chat area, borderless) ---
        let visible_height = self.chat_area.height as usize;
        let total_lines = self.layout_cache.lines.len();
        let scrollbar_visible = total_lines > visible_height;
        if scrollbar_visible
            && self.chat_area.width > 0
            && col == self.chat_area.x + self.chat_area.width - 1
            && row >= self.chat_area.y
            && row < self.chat_area.y + self.chat_area.height
        {
            return HitZone::Scrollbar;
        }

        // --- Chat content (borderless, excluding scrollbar column) ---
        if self.chat_area.width > 1
            && self.chat_area.height > 0
            && col >= self.chat_area.x
            && col < self.chat_area.x + self.chat_area.width - 1
            && row >= self.chat_area.y
            && row < self.chat_area.y + self.chat_area.height
        {
            let inner_row = (row - self.chat_area.y) as usize;
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
        // Borderless layout: full height is the visible height.
        let visible_height = self.chat_area.height as usize;
        let total_lines = self.layout_cache.lines.len();
        let max_scroll = total_lines.saturating_sub(visible_height);
        if max_scroll == 0 || visible_height == 0 {
            return;
        }
        let track_top = self.chat_area.y; // borderless — no top border
        let relative_row = (row.saturating_sub(track_top)) as usize;
        // Track spans rows 0..=visible_height-1.  Divide by (visible_height - 1)
        // so the bottom row maps to skip=max_scroll → scroll=0.
        let track_span = (visible_height - 1).max(1);
        let skip = relative_row * max_scroll / track_span;
        self.scroll = max_scroll.saturating_sub(skip).min(max_scroll);
    }

    /// Map a mouse row on the interactive terminal scrollbar to a
    /// display_offset delta and apply it. The scrollbar is rendered with
    /// position = scroll_len - offset (bottom-up), so the mapping inverts:
    /// relative_row → position → offset = scroll_len - position → delta.
    fn terminal_scrollbar_drag(&mut self, row: u16) {
        let area = self.terminal_area;
        if area.height < 2 {
            return;
        }
        let Some(ref mut t) = self.terminal else { return };
        let visible_height = (area.height as usize).min(t.rows() as usize);
        if visible_height < 2 {
            return;
        }
        let total_lines = t.total_grid_lines();
        let scroll_len = total_lines.saturating_sub(visible_height);
        if scroll_len == 0 {
            return;
        }
        let track_top = area.y;
        let track_span = visible_height - 1;
        let relative_row = (row.saturating_sub(track_top) as usize).min(track_span);
        let position = relative_row * scroll_len / track_span;
        let desired_offset = (scroll_len - position) as i32;
        let current = t.display_offset() as i32;
        let delta = desired_offset - current;
        t.scroll_display(delta);
    }

    /// Set cursor position from a click in the input area.
    ///
    /// Reverses the `place_cursor` math: `cursor_pos = (row + scroll_offset) * inner_width + col`.
    /// Uses borderless geometry (prompt occupies columns 0..1, no top border).
    fn set_cursor_from_click(&mut self, col: u16, row: u16) {
        if self.input_area.width == 0 {
            return;
        }
        let prompt_width: u16 = 2; // prompt char + space
        let inner_x = self.input_area.x + prompt_width;
        let inner_y = self.input_area.y; // borderless — no top border
        let inner_width = (self.input_area.width.saturating_sub(prompt_width)).max(1) as usize;

        let relative_col = (col.saturating_sub(inner_x)) as usize;
        let relative_row = (row.saturating_sub(inner_y)) as usize;

        let char_count = self.input.chars().count();
        let pos = (relative_row + self.input_scroll_offset) * inner_width + relative_col;
        self.cursor_pos = pos.min(char_count);
    }

    /// Convert a screen `(col, row)` to `(line_idx, char_col)` in layout-cache
    /// space.  Returns `None` if the coordinate is outside the chat content.
    ///
    /// `line_idx` is the absolute index into `layout_cache.lines`.
    /// `char_col` is the character offset within that line (0-based).
    fn screen_to_line_col(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        if self.chat_area.width <= 1 || self.chat_area.height == 0 {
            return None;
        }
        // Exclude scrollbar column (rightmost).
        if col >= self.chat_area.x + self.chat_area.width - 1 {
            return None;
        }
        if col < self.chat_area.x || row < self.chat_area.y || row >= self.chat_area.y + self.chat_area.height {
            return None;
        }
        let visible_height = self.chat_area.height as usize;
        let total_lines = self.layout_cache.lines.len();
        let skip = if total_lines > visible_height {
            total_lines.saturating_sub(visible_height + self.scroll)
        } else {
            0
        };
        let inner_row = (row - self.chat_area.y) as usize;
        let line_idx = skip + inner_row;
        if line_idx >= total_lines {
            return None;
        }
        let char_col = (col - self.chat_area.x) as usize;
        Some((line_idx, char_col))
    }

    /// Extract the plain-text content of a rendered line.
    ///
    /// Concatenates all span contents — stripping style information — to
    /// produce the raw text needed for clipboard copy.
    fn line_text(&self, line_idx: usize) -> String {
        self.layout_cache
            .lines
            .get(line_idx)
            .map(|rl| {
                rl.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .unwrap_or_default()
    }

    /// Extract the selected text from `layout_cache.lines`.
    ///
    /// For the start and end lines, only the portion within the selection
    /// column range is included.  Middle lines are included in full.
    fn selected_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?;
        if sel.is_empty() {
            return None;
        }
        let ((start_line, start_col), (end_line, end_col)) = sel.normalised();
        let mut result = String::new();
        for line_idx in start_line..=end_line {
            let text = self.line_text(line_idx);
            if line_idx == start_line && line_idx == end_line {
                // Single-line selection
                let s = start_col.min(text.chars().count());
                let e = end_col.min(text.chars().count());
                result.push_str(&text.chars().skip(s).take(e.saturating_sub(s)).collect::<String>());
            } else if line_idx == start_line {
                let s = start_col.min(text.chars().count());
                result.push_str(&text.chars().skip(s).collect::<String>());
            } else if line_idx == end_line {
                let e = end_col.min(text.chars().count());
                result.push_str(&text.chars().take(e).collect::<String>());
            } else {
                result.push_str(&text);
            }
            if line_idx < end_line {
                result.push('\n');
            }
        }
        if result.is_empty() { None } else { Some(result) }
    }

    /// Copy the current selection to the system clipboard.
    /// On success, shows a "copied" toast for ~1.5 seconds.
    fn copy_selection_to_clipboard(&mut self) {
        if let Some(text) = self.selected_text() {
            match arboard::Clipboard::new() {
                Ok(mut cb) => {
                    if cb.set_text(&text).is_ok() {
                        self.toast = Some((
                            "copied".to_string(),
                            Instant::now() + Duration::from_millis(1500),
                        ));
                    }
                }
                Err(_) => {
                    // Clipboard not available — silently ignore.
                    // Selection still works visually.
                }
            }
        }
    }

    /// Select a word at the given line and column.
    ///
    /// A "word" is a maximal run of non-whitespace characters.
    fn select_word(&mut self, line_idx: usize, col: usize) {
        let text = self.line_text(line_idx);
        let char_count = text.chars().count();
        if char_count == 0 {
            self.selection = Some(Selection {
                anchor_line: line_idx, anchor_col: 0,
                head_line: line_idx, head_col: 0,
            });
            return;
        }
        let col = col.min(char_count);
        let chars: Vec<char> = text.chars().collect();
        // Find word boundaries.
        let is_word_char = |c: char| !c.is_whitespace();
        // If cursor is on whitespace, select the whitespace run.
        let target_is_word = is_word_char(chars[col.min(char_count - 1)]);
        let mut start = col;
        while start > 0 && is_word_char(chars[start - 1]) == target_is_word {
            start -= 1;
        }
        let mut end = col;
        while end < char_count && is_word_char(chars[end]) == target_is_word {
            end += 1;
        }
        self.selection = Some(Selection {
            anchor_line: line_idx, anchor_col: start,
            head_line: line_idx, head_col: end,
        });
    }

    /// Select an entire line.
    fn select_line(&mut self, line_idx: usize) {
        let char_count = self.line_text(line_idx).chars().count();
        self.selection = Some(Selection {
            anchor_line: line_idx, anchor_col: 0,
            head_line: line_idx, head_col: char_count,
        });
    }

    /// Whether the toast is still active (not expired).
    pub fn toast_text(&self) -> Option<&str> {
        self.toast.as_ref().and_then(|(text, expiry)| {
            if *expiry > Instant::now() { Some(text.as_str()) } else { None }
        })
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

    /// Handle a TUI event (forwarded agent event or TUI-specific event).
    pub fn handle_agent_event(&mut self, event: TuiEvent) {
        let sid = match &event {
            TuiEvent::Agent { session_id, .. } => *session_id,
            TuiEvent::Thinking => self.sessions[self.active].id,
            TuiEvent::ConfirmationRequest { .. } => self.sessions[self.active].id,
            TuiEvent::TransportChanged { .. } => self.sessions[self.active].id,
        };

        // Dispatch to the originating session. Save the active index so we can
        // restore it after applying the event to a non-active tab.
        let orig_active = self.active;
        let is_background = self.sessions[orig_active].id != sid;

        if let Some(idx) = self.find_session_idx(sid) {
            self.active = idx;
        } else {
            // Session closed while event was in flight — discard.
            tracing::debug!(?sid, "discarding event for closed session");
            return;
        }

        let mut auto_scroll = true;
        match event {
            TuiEvent::Agent { event: agent_event, .. } => match agent_event {
                filar_agent::AgentEvent::Started => {
                    self.mode = AppMode::Thinking;
                    self.active_session_mut().background_activity = true;
                }
                filar_agent::AgentEvent::TextDelta(s) => {
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
                    auto_scroll = self.scroll == 0;
                }
                filar_agent::AgentEvent::CommandProposed { command, explanation, .. } => {
                    self.pending_proposal = Some((command, explanation));
                }
                filar_agent::AgentEvent::CommandFinished { command, output, denied } => {
                    if !denied {
                        self.streaming = false;
                        auto_scroll = self.scroll == 0;
                        let explanation = self
                            .pending_proposal
                            .as_ref()
                            .filter(|(cmd, _)| *cmd == command)
                            .map(|(_, expl)| expl.clone())
                            .unwrap_or_default();
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
                                *a = true;
                                updated = true;
                                self.message_rev = self.message_rev.wrapping_add(1);
                            }
                        }
                        if !updated {
                            self.push_message(ChatBlock::Command {
                                command: command.clone(),
                                explanation,
                                output: Some(output.clone()),
                                approved: true,
                            });
                        }
                    } else {
                        self.push_message(ChatBlock::System(format!("Denied: {command}")));
                        auto_scroll = self.scroll == 0;
                    }
                    self.pending_proposal = None;
                }
                filar_agent::AgentEvent::Finished(text) => {
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
                    self.cancellation = None;
                    self.active_session_mut().background_activity = false;
                }
                filar_agent::AgentEvent::Error(err) => {
                    if self.streaming {
                        self.push_message(ChatBlock::System("response interrupted".into()));
                        self.streaming = false;
                        auto_scroll = self.scroll == 0;
                    }
                    self.push_message(ChatBlock::Error(err));
                    self.mode = AppMode::Normal;
                    self.agent_running = false;
                    self.cancellation = None;
                    self.active_session_mut().background_activity = false;
                }
                _ => {}
            },
            TuiEvent::Thinking => {
                self.mode = AppMode::Thinking;
            }
            TuiEvent::ConfirmationRequest {
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
            TuiEvent::TransportChanged { .. } => {
                // Handled by the runner before reaching here — no-op.
            }
        }
        // Auto-scroll to bottom on new content (unless user scrolled up during streaming).
        if auto_scroll {
            self.scroll = 0;
        }
        // Mark background session as having new content if event went to non-active tab.
        if is_background {
            self.active_session_mut().has_new = true;
        }
        // Track awaiting confirmation.
        if self.mode == AppMode::Confirming {
            self.active_session_mut().awaiting_confirmation = true;
        } else {
            self.active_session_mut().awaiting_confirmation = false;
        }
        // Restore the original active tab.
        self.active = orig_active;
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

    /// Hide the interactive view, keeping the terminal alive in the background.
    pub fn hide_interactive_view(&mut self) {
        if self.mode == AppMode::Interactive {
            self.mode = AppMode::Normal;
        }
    }

    /// Show the interactive view for the active session, if a terminal exists.
    pub fn show_interactive_view(&mut self) {
        if self.terminal.is_some() {
            self.mode = AppMode::Interactive;
        }
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

// ---------------------------------------------------------------------------
// SGR mouse encoding for interactive terminal mode
// ---------------------------------------------------------------------------

/// Encode a crossterm mouse event as an SGR mouse sequence.
///
/// Returns `None` for event types that don't have a standard SGR encoding.
///
/// Format: `\x1b[<{button};{x};{y}M` for press/motion, `\x1b[<{button};{x};{y}m`
/// for release.  Coordinates are 1-based.
fn encode_sgr_mouse(m: &crossterm::event::MouseEvent, x: usize, y: usize) -> Option<Vec<u8>> {
    use crossterm::event::{MouseButton, MouseEventKind, KeyModifiers};

    // Base button code.
    let (button, is_release) = match m.kind {
        MouseEventKind::Down(MouseButton::Left) => (0, false),
        MouseEventKind::Down(MouseButton::Right) => (2, false),
        MouseEventKind::Down(MouseButton::Middle) => (1, false),
        MouseEventKind::Up(MouseButton::Left) => (0, true),
        MouseEventKind::Up(MouseButton::Right) => (2, true),
        MouseEventKind::Up(MouseButton::Middle) => (1, true),
        MouseEventKind::Drag(MouseButton::Left) => (32, false),
        MouseEventKind::Drag(MouseButton::Right) => (34, false),
        MouseEventKind::Drag(MouseButton::Middle) => (33, false),
        MouseEventKind::Moved => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        _ => return None,
    };

    // Add modifier flags.
    let mut code = button;
    if m.modifiers.contains(KeyModifiers::SHIFT) {
        code |= 4;
    }
    if m.modifiers.contains(KeyModifiers::ALT) {
        code |= 8;
    }
    if m.modifiers.contains(KeyModifiers::CONTROL) {
        code |= 16;
    }

    let suffix = if is_release { b'm' } else { b'M' };
    Some(format!("\x1b[<{code};{x};{y}").into_bytes())
        .map(|mut v| { v.push(suffix); v })
}

/// Encode a crossterm mouse event using the legacy (pre-SGR) encoding.
///
/// Format: `\x1b[M` followed by 3 bytes: `(button_code + 32)`,
/// `(x + 32)`, `(y + 32)`.  Coordinates are 1-based and clamped to 255.
///
/// Returns `None` for event types that don't have a standard encoding.
fn encode_legacy_mouse(m: &crossterm::event::MouseEvent, x: usize, y: usize) -> Option<Vec<u8>> {
    use crossterm::event::{MouseButton, MouseEventKind, KeyModifiers};

    // Base button code (same as SGR for the low bits).
    let button = match m.kind {
        MouseEventKind::Down(MouseButton::Left) => 0,
        MouseEventKind::Down(MouseButton::Right) => 2,
        MouseEventKind::Down(MouseButton::Middle) => 1,
        MouseEventKind::Up(_) => 3, // Release is button 3 in legacy mode.
        MouseEventKind::Drag(MouseButton::Left) => 32,
        MouseEventKind::Drag(MouseButton::Right) => 34,
        MouseEventKind::Drag(MouseButton::Middle) => 33,
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        _ => return None,
    };

    // Add modifier flags.
    let mut code = button;
    if m.modifiers.contains(KeyModifiers::SHIFT) {
        code |= 4;
    }
    if m.modifiers.contains(KeyModifiers::ALT) {
        code |= 8;
    }
    if m.modifiers.contains(KeyModifiers::CONTROL) {
        code |= 16;
    }

    // Clamp coordinates to legacy max (255 - 32 = 223 usable).
    let bx = (code + 32).min(255) as u8;
    let sx = ((x - 1) + 32).min(255) as u8;
    let sy = ((y - 1) + 32).min(255) as u8;

    Some(vec![0x1b, b'[', b'M', bx, sx, sy])
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

    fn ctrl_key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::CONTROL,
        )
    }

    #[test]
    fn ctrl_c_is_noop_in_normal() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.handle_key(ctrl_key('c'));
        assert!(!app.should_quit, "Ctrl+C must do nothing (users use it to copy)");
        // Russian layout equivalent (с) is likewise a no-op.
        app.handle_key(ctrl_key('с'));
        assert!(!app.should_quit);
    }

    #[test]
    fn ctrl_q_quits_in_normal() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.handle_key(ctrl_key('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_q_russian_layout_quits() {
        // й = q in ЙЦУКЕН layout.
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.handle_key(ctrl_key('й'));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_z_is_noop_in_normal() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.handle_key(ctrl_key('z'));
        assert!(!app.should_quit);
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn ctrl_z_cancels_in_thinking() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Thinking;
        app.agent_running = true;
        app.handle_key(ctrl_key('z'));
        assert_eq!(app.mode, AppMode::Normal);
        assert!(!app.agent_running);
        assert!(!app.should_quit, "Ctrl+Z cancels, it must not quit");
        assert!(matches!(app.messages.last(), Some(ChatBlock::System(s)) if s == "Cancelled."));
    }

    #[test]
    fn ctrl_z_russian_layout_cancels_in_thinking() {
        // я = z in ЙЦУКЕН layout.
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Thinking;
        app.agent_running = true;
        app.handle_key(ctrl_key('я'));
        assert_eq!(app.mode, AppMode::Normal);
        assert!(!app.agent_running);
    }

    #[test]
    fn ctrl_c_is_noop_in_thinking() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Thinking;
        app.agent_running = true;
        app.handle_key(ctrl_key('c'));
        assert_eq!(app.mode, AppMode::Thinking);
        assert!(app.agent_running);
        assert!(!app.should_quit);
    }

    #[test]
    fn ctrl_q_quits_in_thinking() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Thinking;
        app.agent_running = true;
        app.handle_key(ctrl_key('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_q_and_z_are_forwarded_in_interactive() {
        // In Interactive the global hotkey gate is bypassed: ^Q/^Z must reach
        // the PTY as raw control bytes (Ctrl+Q=0x11, Ctrl+Z=0x1A), NOT trigger
        // quit()/cancel_work(). Only Ctrl+T leaves interactive mode.
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Interactive;

        app.handle_key(ctrl_key('q'));
        assert!(!app.should_quit, "^Q must not quit in Interactive");
        assert_eq!(app.mode, AppMode::Interactive);

        app.handle_key(ctrl_key('z'));
        assert_eq!(app.mode, AppMode::Interactive, "^Z must not cancel in Interactive");
        assert!(!app.should_quit);

        let bytes = app
            .pending_term_input
            .clone()
            .expect("keys should be forwarded to the PTY");
        assert!(bytes.contains(&0x11), "Ctrl+Q should forward 0x11, got {bytes:?}");
        assert!(bytes.contains(&0x1a), "Ctrl+Z should forward 0x1A, got {bytes:?}");
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
    fn push_system_log_dedups_consecutive_lines() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let before = app.messages.len();

        app.push_system_log("ssh: reader: channel closed".into());
        app.push_system_log("ssh: reader: channel closed".into());
        app.push_system_log("ssh: reader: channel closed".into());

        // Only one block added; it carries the "… x3" counter.
        assert_eq!(app.messages.len(), before + 1);
        assert!(
            matches!(app.messages.last(), Some(ChatBlock::System(s)) if s == "ssh: reader: channel closed … x3"),
            "got: {:?}",
            app.messages.last()
        );
    }

    #[test]
    fn push_system_log_new_line_breaks_dedup_run() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);

        app.push_system_log("first".into());
        app.push_system_log("second".into());
        app.push_system_log("second".into());

        // Two distinct System blocks; the second collapsed the repeat.
        assert!(matches!(&app.messages[app.messages.len() - 2], ChatBlock::System(s) if s == "first"));
        assert!(matches!(app.messages.last(), Some(ChatBlock::System(s)) if s == "second … x2"));
    }

    #[test]
    fn push_system_log_dedup_key_is_full_line_not_truncated() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        // Narrow chat so both lines clamp to the same rendered text, but their
        // full forms differ only past the clamp point.
        app.chat_area.width = 10;
        let before = app.messages.len();

        app.push_system_log("abcdefghij1".into());
        app.push_system_log("abcdefghij2".into());

        // Distinct full lines → two separate blocks, NOT collapsed into "… x2".
        assert_eq!(app.messages.len(), before + 2);
        for m in &app.messages[before..] {
            match m {
                ChatBlock::System(s) => {
                    assert!(!s.contains(" x2"), "distinct lines must not dedup: {s}");
                    assert!(s.chars().count() <= 10, "clamped to width: {s}");
                }
                other => panic!("expected System, got {other:?}"),
            }
        }
    }

    #[test]
    fn push_system_log_repeat_clamps_suffix_within_width() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area.width = 8;
        let before = app.messages.len();

        for _ in 0..5 {
            app.push_system_log("hello".into());
        }

        // Same full line collapses into a single block…
        assert_eq!(app.messages.len(), before + 1);
        // …and the rendered text (including the "… xN" suffix) stays within
        // the chat width.
        match app.messages.last() {
            Some(ChatBlock::System(s)) => {
                assert!(
                    s.chars().count() <= 8,
                    "final rendered string must be clamped to width: {s} ({} chars)",
                    s.chars().count()
                );
            }
            other => panic!("expected System, got {other:?}"),
        }
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
        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::Finished("hello".into()) });
        assert!(app.message_rev > rev_before);
    }

    #[test]
    fn agent_error_bumps_message_rev() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let rev_before = app.message_rev;
        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::Error("oops".into()) });
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
        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::CommandFinished {
            command: "ls".into(),
            output: "file1\nfile2".into(),
            denied: false,
        }});
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
        // visible_height = 24 (borderless); max_scroll = 30 - 24 = 6

        // Scroll up many times to exceed max.
        for _ in 0..10 {
            app.handle_mouse(mouse_event(
                crossterm::event::MouseEventKind::ScrollUp,
                10,
                10,
            ));
        }
        assert_eq!(app.scroll, 6); // clamped to max_scroll
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
        // visible_height = 24 (borderless); max_scroll = 30 - 24 = 6

        // PageUp many times to exceed max.
        for _ in 0..5 {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageUp,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        // 5 * 5 = 25, clamped to 6
        assert_eq!(app.scroll, 6);
    }

    // ----- Hit-testing tests (issue #16) -----

    /// Helper: set up an app with a chat area and cached lines for hit-testing.
    fn make_hit_test_app() -> App {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        // Chat area: x=0, y=1, w=80, h=24 (borderless: full 80x24 is visible)
        app.chat_area = Rect::new(0, 1, 80, 24);
        // Input area: x=0, y=26, w=80, h=5 (borderless: prompt at col 0-1)
        app.input_area = Rect::new(0, 26, 80, 5);
        // Status bar: y=0, h=1
        app.status_bar_area = Rect::new(0, 0, 80, 1);
        // Help bar: y=31, h=1
        app.help_bar_area = Rect::new(0, 31, 80, 1);
        // 50 cached lines → scrollbar visible (50 > 24)
        app.layout_cache.lines = (0..50)
            .map(|i| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw(format!("line {i}")),
                block_index: Some(i),
                region: crate::ui::layout_cache::LineRegion::Body,
            })
            .collect();
        // scroll = 0 → bottom; skip = 50 - 24 = 26
        app
    }

    #[test]
    fn hit_test_chat_content() {
        let app = make_hit_test_app();
        // Click at col=5, row=1 (first content row, borderless)
        // skip = 26, inner_row = 0, line_idx = 26
        let zone = app.hit_test(5, 1);
        assert_eq!(zone, HitZone::Chat { line_idx: 26 });
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
        // Scrollbar = rightmost column of chat area (col=79), borderless (row 1..24)
        let zone = app.hit_test(79, 10);
        assert_eq!(zone, HitZone::Scrollbar);
    }

    #[test]
    fn hit_test_scrollbar_not_visible_when_content_fits() {
        let mut app = make_hit_test_app();
        // Only 5 lines → fits in visible_height=24, no scrollbar.
        app.layout_cache.lines.truncate(5);
        // Click at rightmost column → scrollbar not visible, col=79 excluded
        // from chat content → Outside.
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
        // scroll=10 → skip = 50 - 24 - 10 = 16
        app.scroll = 10;
        // Click at row=1 (inner_row=0) → line_idx = 16
        let zone = app.hit_test(5, 1);
        assert_eq!(zone, HitZone::Chat { line_idx: 16 });
    }

    // ----- Scrollbar drag tests -----

    #[test]
    fn scrollbar_drag_sets_scroll_proportionally() {
        let mut app = make_hit_test_app();
        // visible_height = 24, max_scroll = 50 - 24 = 26
        // Drag to top of track (row=1, relative_row=0):
        // skip = 0 * 26 / 23 = 0, scroll = 26 - 0 = 26 (top)
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            79,
            1,
        ));
        assert_eq!(app.scroll, 26);
        assert_eq!(app.mouse_drag, Some(DragKind::Scrollbar));

        // Drag to bottom of track (row=24, relative_row=23):
        // track_span = 24 - 1 = 23, skip = 23 * 26 / 23 = 26, scroll = 26 - 26 = 0
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            79,
            24,
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
        // input_area = x=0, y=26, w=80, h=5 (borderless: prompt at col 0-1)
        // inner_x=2, inner_y=26, inner_width=78
        // Click at col=4, row=26 → relative_col=2, relative_row=0 → cursor_pos=2
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            4,
            26,
        ));
        assert_eq!(app.cursor_pos, 2);
    }

    #[test]
    fn click_input_second_row_sets_cursor() {
        let mut app = make_hit_test_app();
        app.mode = AppMode::Normal;
        // 80 chars → wraps to 2 lines at inner_width=78
        app.input = "a".repeat(80);
        // Click at col=2, row=27 (second row of input, relative_row=1)
        // cursor_pos = 1 * 78 + 0 = 78
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            2,
            27,
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
            26,
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
            4,
            26,
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
    fn ctrl_c_is_noop_in_confirming() {
        let mut app = make_confirm_app(false);
        app.handle_key(ctrl_key('c'));
        assert!(!app.should_quit, "Ctrl+C must do nothing in Confirming");
        assert_eq!(app.mode, AppMode::Confirming, "should stay awaiting a choice");
    }

    #[test]
    fn ctrl_q_denies_and_quits_in_confirming() {
        let mut app = make_confirm_app(false);
        app.handle_key(ctrl_key('q'));
        assert!(app.should_quit);
        assert_eq!(app.mode, AppMode::Thinking);
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(!*approved, "Ctrl+Q should deny the pending command");
        } else {
            panic!("expected Command block");
        }
    }

    #[test]
    fn ctrl_z_denies_without_quit_in_confirming() {
        let mut app = make_confirm_app(false);
        app.handle_key(ctrl_key('z'));
        assert!(!app.should_quit, "Ctrl+Z denies but must not quit");
        assert_eq!(app.mode, AppMode::Thinking);
        if let Some(ChatBlock::Command { approved, .. }) = app.messages.last() {
            assert!(!*approved, "Ctrl+Z should deny the pending command");
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
        app.handle_agent_event(TuiEvent::ConfirmationRequest {
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
    fn mouse_hover_does_not_change_confirm_selected() {
        let mut app = make_confirm_app(false);
        app.confirm_button_areas.push((Rect::new(20, 10, 15, 1), true));
        app.confirm_button_areas.push((Rect::new(38, 10, 13, 1), false));
        // Hover over Approve — hovered_button updates but confirm_selected stays Deny.
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Moved,
            25,
            10,
        ));
        assert_eq!(app.hovered_button, Some(true));
        assert!(!app.confirm_selected, "hover must NOT change confirm_selected");
        // Hover over Deny — hovered_button updates, confirm_selected unchanged.
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Moved,
            42,
            10,
        ));
        assert_eq!(app.hovered_button, Some(false));
        assert!(!app.confirm_selected);
        // Hover outside buttons — hovered_button clears.
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Moved,
            0,
            0,
        ));
        assert_eq!(app.hovered_button, None);
        assert!(!app.confirm_selected);
    }

    #[test]
    fn repeated_hover_does_not_change_confirm_selected() {
        let mut app = make_confirm_app(false);
        app.confirm_button_areas.push((Rect::new(20, 10, 15, 1), true));
        app.confirm_button_areas.push((Rect::new(38, 10, 13, 1), false));
        // Multiple hovers over Approve — confirm_selected must remain false.
        for _ in 0..3 {
            app.handle_mouse(mouse_event(
                crossterm::event::MouseEventKind::Moved,
                25,
                10,
            ));
            assert!(!app.confirm_selected);
        }
        // Hover over Deny, then back over Approve — still must remain false.
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Moved,
            42,
            10,
        ));
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Moved,
            25,
            10,
        ));
        assert!(!app.confirm_selected, "hover must never change confirm_selected");
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
        // Click the OutputToggle line (row 2 → inner_row=1 → line_idx 1, borderless).
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            2,
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
        // Click the Header line (row 1 → inner_row=0 → line_idx 0, borderless).
        app.handle_mouse(mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            1,
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

        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta("Hello".into()) });

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

        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta("Hello".into()) });
        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta(" world".into()) });

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
        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta("Partial".into()) });
        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta(" response".into()) });
        assert!(app.streaming);

        // Finished replaces with authoritative text.
        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::Finished(
            "Partial response — finalized".into(),
        )});

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

        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta("Streamed text".into()) });
        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::Finished(String::new()) });

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

        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta("Partial".into()) });
        assert!(app.streaming);

        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::Error("network error".into()) });

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

        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta("Let me check...".into()) });
        assert!(app.streaming);

        let (tx, _rx) = oneshot::channel::<bool>();
        app.handle_agent_event(TuiEvent::ConfirmationRequest {
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

        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta("new text".into()) });

        // Scroll should NOT be reset to 0 — user is reading history.
        assert_eq!(app.scroll, 5);
    }

    #[test]
    fn text_delta_autoscroll_when_at_bottom() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.messages.clear();
        app.mode = AppMode::Thinking;
        app.scroll = 0;

        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta("new text".into()) });

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

        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta("First".into()) });
        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::Finished("First".into()) });
        assert!(!app.streaming);

        // New run.
        app.handle_agent_event(TuiEvent::Thinking);
        app.handle_agent_event(TuiEvent::Agent { session_id: app.sessions[0].id, event: filar_agent::AgentEvent::TextDelta("Second".into()) });

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
    fn help_action_quit_in_thinking_quits() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Thinking;
        app.agent_running = true;
        app.execute_help_action(HelpAction::Quit);
        assert!(app.should_quit, "Quit action should quit, even in Thinking");
    }

    #[test]
    fn help_action_cancelwork_in_thinking_cancels() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Thinking;
        app.agent_running = true;
        app.execute_help_action(HelpAction::CancelWork);
        assert_eq!(app.mode, AppMode::Normal);
        assert!(!app.agent_running);
        assert!(!app.should_quit);
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

    // ----- Text selection tests (issue #21) -----

    #[test]
    fn selection_normalised_forward() {
        let sel = Selection {
            anchor_line: 5, anchor_col: 3,
            head_line: 10, head_col: 7,
        };
        let ((sl, sc), (el, ec)) = sel.normalised();
        assert_eq!((sl, sc), (5, 3));
        assert_eq!((el, ec), (10, 7));
    }

    #[test]
    fn selection_normalised_backward() {
        let sel = Selection {
            anchor_line: 10, anchor_col: 7,
            head_line: 5, head_col: 3,
        };
        let ((sl, sc), (el, ec)) = sel.normalised();
        assert_eq!((sl, sc), (5, 3));
        assert_eq!((el, ec), (10, 7));
    }

    #[test]
    fn selection_is_empty_when_anchor_equals_head() {
        let sel = Selection {
            anchor_line: 5, anchor_col: 3,
            head_line: 5, head_col: 3,
        };
        assert!(sel.is_empty());
    }

    #[test]
    fn selection_not_empty_when_different_line() {
        let sel = Selection {
            anchor_line: 5, anchor_col: 0,
            head_line: 6, head_col: 0,
        };
        assert!(!sel.is_empty());
    }

    #[test]
    fn selected_text_single_line() {
        let mut app = make_hit_test_app();
        // Line 26 = "line 26" — 7 chars
        app.selection = Some(Selection {
            anchor_line: 26, anchor_col: 0,
            head_line: 26, head_col: 4,
        });
        assert_eq!(app.selected_text().unwrap(), "line");
    }

    #[test]
    fn selected_text_multi_line() {
        let mut app = make_hit_test_app();
        // Lines 26-28: "line 26", "line 27", "line 28"
        app.selection = Some(Selection {
            anchor_line: 26, anchor_col: 5,
            head_line: 28, head_col: 2,
        });
        // From line 26 col 5: "26"
        // Full line 27: "line 27"
        // Line 28 cols 0-2: "li"
        assert_eq!(app.selected_text().unwrap(), "26\nline 27\nli");
    }

    #[test]
    fn selected_text_empty_returns_none() {
        let mut app = make_hit_test_app();
        app.selection = Some(Selection {
            anchor_line: 26, anchor_col: 3,
            head_line: 26, head_col: 3,
        });
        assert!(app.selected_text().is_none());
    }

    #[test]
    fn select_word_picks_non_whitespace_run() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.layout_cache.lines = vec![
            crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("hello world test"),
                block_index: Some(0),
                region: crate::ui::layout_cache::LineRegion::Body,
            }
        ];
        // Click on "world" at col 6 (the 'w')
        app.select_word(0, 6);
        let sel = app.selection.unwrap();
        assert_eq!(sel.anchor_col, 6);
        assert_eq!(sel.head_col, 11); // "world" = cols 6..11
    }

    #[test]
    fn select_word_at_start_of_line() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.layout_cache.lines = vec![
            crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("hello world"),
                block_index: Some(0),
                region: crate::ui::layout_cache::LineRegion::Body,
            }
        ];
        // Click at col 0
        app.select_word(0, 0);
        let sel = app.selection.unwrap();
        assert_eq!(sel.anchor_col, 0);
        assert_eq!(sel.head_col, 5); // "hello"
    }

    #[test]
    fn select_line_selects_entire_line() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.layout_cache.lines = vec![
            crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("hello world"),
                block_index: Some(0),
                region: crate::ui::layout_cache::LineRegion::Body,
            }
        ];
        app.select_line(0);
        let sel = app.selection.unwrap();
        assert_eq!(sel.anchor_col, 0);
        assert_eq!(sel.head_col, 11); // entire line
    }

    #[test]
    fn screen_to_line_col_maps_correctly() {
        let app = make_hit_test_app();
        // chat_area: x=0, y=1, w=80, h=24
        // 50 lines, scroll=0 → skip = 26
        // Click at col=5, row=1 → line_idx=26, char_col=5
        let (line_idx, char_col) = app.screen_to_line_col(5, 1).unwrap();
        assert_eq!(line_idx, 26);
        assert_eq!(char_col, 5);
    }

    #[test]
    fn screen_to_line_col_excludes_scrollbar() {
        let app = make_hit_test_app();
        // col=79 is the scrollbar column → None
        assert!(app.screen_to_line_col(79, 10).is_none());
    }

    #[test]
    fn screen_to_line_col_returns_none_outside() {
        let app = make_hit_test_app();
        assert!(app.screen_to_line_col(200, 200).is_none());
    }

    #[test]
    fn mouse_down_in_chat_starts_selection() {
        let mut app = make_hit_test_app();
        // Click at col=5, row=1 → line 26, col 5
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        assert!(app.selection.is_some());
        assert_eq!(app.mouse_drag, Some(DragKind::Selection));
        let sel = app.selection.unwrap();
        assert_eq!(sel.anchor_line, 26);
        assert_eq!(sel.anchor_col, 5);
        assert_eq!(sel.head_line, 26);
        assert_eq!(sel.head_col, 5);
    }

    #[test]
    fn mouse_drag_updates_selection_head() {
        let mut app = make_hit_test_app();
        // Down at col=0, row=1 → line 26, col 0
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        // Drag to col=4, row=2 → line 27, col 4
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 4,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        let sel = app.selection.unwrap();
        assert_eq!(sel.anchor_line, 26);
        assert_eq!(sel.anchor_col, 0);
        assert_eq!(sel.head_line, 27);
        assert_eq!(sel.head_col, 4);
    }

    #[test]
    fn mouse_up_clears_drag() {
        let mut app = make_hit_test_app();
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        assert_eq!(app.mouse_drag, None);
        // Empty selection should be cleared on mouse-up.
        assert!(app.selection.is_none());
    }

    #[test]
    fn push_message_clears_selection() {
        let mut app = make_hit_test_app();
        app.selection = Some(Selection {
            anchor_line: 26, anchor_col: 0,
            head_line: 26, head_col: 4,
        });
        app.push_message(ChatBlock::User("new message".into()));
        assert!(app.selection.is_none());
    }

    #[test]
    fn toast_text_none_when_no_toast() {
        let app = App::new("test".into(), CommandConfirmMode::Always);
        assert!(app.toast_text().is_none());
    }

    #[test]
    fn toast_text_shown_when_active() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.toast = Some(("copied".to_string(), Instant::now() + Duration::from_secs(10)));
        assert_eq!(app.toast_text().unwrap(), "copied");
    }

    #[test]
    fn toast_text_expired_returns_none() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.toast = Some(("copied".to_string(), Instant::now() - Duration::from_secs(1)));
        assert!(app.toast_text().is_none());
    }

    #[test]
    fn header_click_non_collapsing_starts_selection() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 1, 80, 24);
        // A Header line for a User message (not a collapsible Command).
        app.layout_cache.lines = vec![
            crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("  you"),
                block_index: Some(0),
                region: crate::ui::layout_cache::LineRegion::Header,
            },
        ];
        app.messages = vec![ChatBlock::User("test".into())];
        // Click at col=3, row=1 (on the "you" header text)
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 3,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        // Should fall through to selection, not return early.
        assert!(app.selection.is_some());
        assert_eq!(app.mouse_drag, Some(DragKind::Selection));
    }

    // --- Interactive mouse tests ---

    fn make_interactive_app() -> App {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Interactive;
        app.terminal = Some(TerminalModel::new(80, 24));
        app.terminal_area = Rect::new(0, 2, 80, 20);
        app
    }

    #[test]
    fn interactive_scroll_up_primary_screen() {
        let mut app = make_interactive_app();
        // Feed enough lines to create scrollback history.
        if let Some(t) = app.terminal.as_mut() {
            for _ in 0..30 {
                t.feed(b"line\n");
            }
        }
        // Scroll up — should scroll through scrollback.
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        // No pending term input — scroll is internal.
        assert!(app.pending_term_input.is_none());
    }

    #[test]
    fn interactive_scroll_down_primary_screen() {
        let mut app = make_interactive_app();
        if let Some(t) = app.terminal.as_mut() {
            for _ in 0..30 {
                t.feed(b"line\n");
            }
            t.scroll_display(10);
        }
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollDown,
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        assert!(app.pending_term_input.is_none());
    }

    #[test]
    fn interactive_scroll_alt_screen_translates_to_arrows() {
        let mut app = make_interactive_app();
        // Enter alt screen mode via ESC sequence.
        if let Some(t) = app.terminal.as_mut() {
            t.feed(b"\x1b[?1049h");
        }
        assert!(app.terminal.as_ref().unwrap().is_alt_screen());
        // Scroll up → should produce arrow key bytes.
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        let input = app.take_term_input().unwrap();
        assert_eq!(input, b"\x1b[A\x1b[A\x1b[A");

        // Scroll down → should produce down arrow bytes.
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollDown,
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        let input = app.take_term_input().unwrap();
        assert_eq!(input, b"\x1b[B\x1b[B\x1b[B");
    }

    #[test]
    fn interactive_mouse_outside_area_ignored() {
        let mut app = make_interactive_app();
        // Click below terminal area (row 23 > 2+20=22).
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 10,
            row: 23,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        assert!(app.pending_term_input.is_none());
    }

    #[test]
    fn interactive_sgr_mouse_mode_forwarded() {
        let mut app = make_interactive_app();
        // Enable SGR mouse mode via ESC sequence.
        if let Some(t) = app.terminal.as_mut() {
            t.feed(b"\x1b[?1006h\x1b[?1002h"); // SGR + REPORT_CLICK
        }
        assert!(app.terminal.as_ref().unwrap().mouse_mode());

        // Left click at (col=10, row=5) → SGR: x=11, y=4 (1-based).
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        let input = app.take_term_input().unwrap();
        // SGR format: \x1b[<0;11;4M
        assert_eq!(input, b"\x1b[<0;11;4M");
    }

    #[test]
    fn interactive_sgr_mouse_release() {
        let mut app = make_interactive_app();
        if let Some(t) = app.terminal.as_mut() {
            t.feed(b"\x1b[?1006h\x1b[?1002h");
        }
        // Left button release at (col=20, row=10) → x=21, y=9.
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: 20,
            row: 10,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        let input = app.take_term_input().unwrap();
        // Release uses lowercase 'm'.
        assert_eq!(input, b"\x1b[<0;21;9m");
    }

    #[test]
    fn interactive_sgr_mouse_scroll() {
        let mut app = make_interactive_app();
        if let Some(t) = app.terminal.as_mut() {
            t.feed(b"\x1b[?1006h\x1b[?1002h");
        }
        // Scroll up → button code 64.
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 15,
            row: 7,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        let input = app.take_term_input().unwrap();
        // x=16, y=6.
        assert_eq!(input, b"\x1b[<64;16;6M");
    }

    #[test]
    fn interactive_sgr_mouse_with_modifiers() {
        let mut app = make_interactive_app();
        if let Some(t) = app.terminal.as_mut() {
            t.feed(b"\x1b[?1006h\x1b[?1002h");
        }
        // Ctrl+Shift+Left click at (col=5, row=3) → x=6, y=2.
        // code = 0 + 4 (shift) + 16 (ctrl) = 20.
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::SHIFT | crossterm::event::KeyModifiers::CONTROL,
        });
        let input = app.take_term_input().unwrap();
        assert_eq!(input, b"\x1b[<20;6;2M");
    }

    #[test]
    fn encode_sgr_mouse_right_click() {
        let m = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let result = encode_sgr_mouse(&m, 5, 3).unwrap();
        assert_eq!(result, b"\x1b[<2;5;3M");
    }

    #[test]
    fn encode_sgr_mouse_middle_drag() {
        let m = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Middle),
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let result = encode_sgr_mouse(&m, 10, 10).unwrap();
        assert_eq!(result, b"\x1b[<33;10;10M");
    }

    #[test]
    fn encode_sgr_mouse_motion_no_button() {
        let m = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let result = encode_sgr_mouse(&m, 1, 1).unwrap();
        assert_eq!(result, b"\x1b[<35;1;1M");
    }

    // --- TerminalModel new methods tests ---

    #[test]
    fn terminal_model_mouse_mode_default_off() {
        let model = TerminalModel::new(80, 24);
        assert!(!model.mouse_mode());
    }

    #[test]
    fn terminal_model_mouse_mode_sgr_enabled() {
        let mut model = TerminalModel::new(80, 24);
        // Enable SGR mouse + click reporting.
        model.feed(b"\x1b[?1006h\x1b[?1002h");
        assert!(model.mouse_mode());
        assert!(model.sgr_mouse());
    }

    #[test]
    fn terminal_model_mouse_mode_sgr_only_not_tracking() {
        let mut model = TerminalModel::new(80, 24);
        // Enable SGR encoding only — no tracking mode.
        model.feed(b"\x1b[?1006h");
        assert!(!model.mouse_mode()); // SGR alone is not tracking.
        assert!(model.sgr_mouse());
    }

    #[test]
    fn terminal_model_mouse_mode_legacy_tracking() {
        let mut model = TerminalModel::new(80, 24);
        // Enable click tracking without SGR.
        model.feed(b"\x1b[?1000h");
        assert!(model.mouse_mode());
        assert!(!model.sgr_mouse());
    }

    #[test]
    fn terminal_model_alt_screen_default_off() {
        let model = TerminalModel::new(80, 24);
        assert!(!model.is_alt_screen());
    }

    #[test]
    fn terminal_model_alt_screen_enabled() {
        let mut model = TerminalModel::new(80, 24);
        model.feed(b"\x1b[?1049h");
        assert!(model.is_alt_screen());
    }

    #[test]
    fn terminal_model_scroll_display_up() {
        let mut model = TerminalModel::new(80, 5);
        for _ in 0..20 {
            model.feed(b"line\n");
        }
        assert_eq!(model.display_offset(), 0);
        model.scroll_display(3);
        assert_eq!(model.display_offset(), 3);
    }

    #[test]
    fn terminal_model_scroll_to_bottom() {
        let mut model = TerminalModel::new(80, 5);
        for _ in 0..20 {
            model.feed(b"line\n");
        }
        model.scroll_display(5);
        assert_eq!(model.display_offset(), 5);
        model.scroll_to_bottom();
        assert_eq!(model.display_offset(), 0);
    }

    /// PgUp in interactive mode scrolls history up, NOT forwarded to PTY.
    #[test]
    fn interactive_pgup_scrolls_scrollback() {
        let mut app = make_interactive_app();
        if let Some(t) = app.terminal.as_mut() {
            for _ in 0..50 {
                t.feed(b"line\n");
            }
        }
        let rows = app.terminal.as_ref().unwrap().rows() as usize;
        // Scroll to bottom first.
        app.terminal.as_mut().unwrap().scroll_to_bottom();
        // Press PgUp.
        app.handle_key(crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::PageUp,
            modifiers: crossterm::event::KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        let offset = app.terminal.as_ref().unwrap().display_offset();
        assert_eq!(offset, rows, "PgUp should scroll one screen up");

        // PgUp was NOT forwarded to PTY (no pending input).
        assert!(app.take_term_input().is_none(), "PgUp should not be forwarded to PTY");
    }

    /// PgDn in interactive mode scrolls history down.
    #[test]
    fn interactive_pgdn_scrolls_scrollback() {
        let mut app = make_interactive_app();
        if let Some(t) = app.terminal.as_mut() {
            for _ in 0..30 {
                t.feed(b"line\n");
            }
        }
        // Scroll up first.
        app.terminal.as_mut().unwrap().scroll_display(10);
        // Press PgDn.
        app.handle_key(crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::PageDown,
            modifiers: crossterm::event::KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        let offset = app.terminal.as_ref().unwrap().display_offset();
        assert!(offset < 10, "PgDn should decrease the scroll offset");
    }

    #[test]
    fn push_term_input_appends() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.push_term_input(b"abc");
        app.push_term_input(b"def");
        let input = app.take_term_input().unwrap();
        assert_eq!(input, b"abcdef");
    }

    #[test]
    fn push_term_input_new_buffer() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.push_term_input(b"hello");
        assert_eq!(app.take_term_input().unwrap(), b"hello");
        assert!(app.take_term_input().is_none());
    }

    // --- Legacy mouse encoding tests ---

    #[test]
    fn interactive_legacy_mouse_forwarded() {
        let mut app = make_interactive_app();
        // Enable click tracking WITHOUT SGR (legacy mode 1000).
        if let Some(t) = app.terminal.as_mut() {
            t.feed(b"\x1b[?1000h");
        }
        assert!(app.terminal.as_ref().unwrap().mouse_mode());
        assert!(!app.terminal.as_ref().unwrap().sgr_mouse());

        // Left click at (col=10, row=5) → x=11, y=4 (1-based).
        // Legacy: \x1b[M + (0+32), (10+32), (3+32) = \x1b[M \x20 \x2a \x23
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        let input = app.take_term_input().unwrap();
        // button=0+32=32, x=(11-1)+32=42, y=(4-1)+32=35
        assert_eq!(input, vec![0x1b, b'[', b'M', 32, 42, 35]);
    }

    #[test]
    fn interactive_legacy_mouse_release() {
        let mut app = make_interactive_app();
        if let Some(t) = app.terminal.as_mut() {
            t.feed(b"\x1b[?1000h");
        }
        // Release → button 3 in legacy mode.
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        let input = app.take_term_input().unwrap();
        // button=3+32=35, x=(6-1)+32=37, y=(2-1)+32=33
        assert_eq!(input, vec![0x1b, b'[', b'M', 35, 37, 33]);
    }

    #[test]
    fn encode_legacy_mouse_right_click() {
        let m = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        // x=5, y=3 → x byte = 4+32=36, y byte = 2+32=34
        let result = encode_legacy_mouse(&m, 5, 3).unwrap();
        // button=2+32=34
        assert_eq!(result, vec![0x1b, b'[', b'M', 34, 36, 34]);
    }

    #[test]
    fn encode_legacy_mouse_scroll() {
        let m = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        // x=1, y=1 → x byte = 0+32=32, y byte = 0+32=32
        let result = encode_legacy_mouse(&m, 1, 1).unwrap();
        // button=64+32=96
        assert_eq!(result, vec![0x1b, b'[', b'M', 96, 32, 32]);
    }

    #[test]
    fn sgr_only_without_tracking_uses_scrollback() {
        let mut app = make_interactive_app();
        // Enable SGR encoding only — no tracking mode.
        if let Some(t) = app.terminal.as_mut() {
            t.feed(b"\x1b[?1006h");
        }
        assert!(!app.terminal.as_ref().unwrap().mouse_mode());
        assert!(app.terminal.as_ref().unwrap().sgr_mouse());

        // Scroll up → should NOT be forwarded as SGR, should use scrollback.
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        // No pending term input — scroll was internal.
        assert!(app.pending_term_input.is_none());
    }

    // --- Scroll clamp edge cases ---

    #[test]
    fn clamp_scroll_zero_when_content_fits() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = (0..10)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();
        // 10 lines fit in 24 → max_scroll = 0
        app.scroll = 5;
        app.clamp_scroll();
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn clamp_scroll_zero_height_no_panic() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 0, 80, 0); // height = 0
        app.scroll = 10;
        app.clamp_scroll(); // should not panic
        assert_eq!(app.scroll, 10); // early return, scroll unchanged
    }

    #[test]
    fn clamp_scroll_exact_fit() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 1, 80, 24);
        app.layout_cache.lines = (0..24)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();
        // 24 lines in 24 height → max_scroll = 0
        app.scroll = 3;
        app.clamp_scroll();
        assert_eq!(app.scroll, 0);
    }

    /// At scroll = 0 (bottom) the scrollbar thumb must reach the end of the
    /// track. `ui::chat::scrollbar_content_len` is the production helper used
    /// by `render_chat_history`; calling it directly verifies that the formula
    /// matches the skip-at-bottom invariant.
    #[test]
    fn scrollbar_content_length_at_bottom() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 1, 80, 24); // height=24
        // 50 lines → visible_height=24, max_scroll = 50-24 = 26
        app.layout_cache.lines = (0..50)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();
        let visible_height = app.chat_area.height as usize;
        let total_lines = app.layout_cache.lines.len();

        // Production helper — the same function render_chat_history calls.
        let content_len = crate::ui::scrollbar_content_len(total_lines, visible_height);
        assert_eq!(content_len, 26);

        // At scroll = 0 (bottom), the skip equals content_len — thumb at end.
        app.scroll = 0;
        let skip = total_lines.saturating_sub(visible_height + app.scroll);
        assert_eq!(skip, content_len, "at bottom, skip should match scrollbar content_length");

        // Edge: content fits in viewport → content_len = 0, scrollbar hidden.
        app.layout_cache.lines = (0..10)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();
        let short = crate::ui::scrollbar_content_len(
            app.layout_cache.lines.len(),
            visible_height,
        );
        assert_eq!(short, 0, "content fits → no scrollable positions");

        // Edge: content exactly equals viewport → content_len = 0.
        app.layout_cache.lines = (0..visible_height)
            .map(|_| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw("test"),
                block_index: None,
                region: crate::ui::layout_cache::LineRegion::Spacer,
            })
            .collect();
        let exact = crate::ui::scrollbar_content_len(
            app.layout_cache.lines.len(),
            visible_height,
        );
        assert_eq!(exact, 0, "exact fit → no scrollable positions");
    }

    // --- Hit test small terminal ---

    #[test]
    fn hit_test_tiny_terminal() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.chat_area = Rect::new(0, 1, 40, 5);
        app.input_area = Rect::new(0, 6, 40, 1);
        app.status_bar_area = Rect::new(0, 0, 40, 1);
        app.help_bar_area = Rect::new(0, 7, 40, 1);
        // Populate chat lines so hit_test exercises the Chat branch, not ChatEmpty.
        app.layout_cache.lines = (0..3)
            .map(|i| crate::ui::layout_cache::RenderedLine {
                line: ratatui::text::Line::raw(format!("line {i}")),
                block_index: Some(i),
                region: crate::ui::layout_cache::LineRegion::Body,
            })
            .collect();
        // Click in chat area — should hit a real Chat zone, not ChatEmpty.
        let zone = app.hit_test(5, 3);
        assert!(matches!(zone, HitZone::Chat { .. }));
        // Click in input area
        let zone = app.hit_test(5, 6);
        assert!(matches!(zone, HitZone::Input));
    }

    #[test]
    fn interactive_ctrl_tab_switches_without_exiting() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        app.new_tab(); // now 2 sessions, active = 1
        let model = crate::terminal::TerminalModel::new(80, 24);
        app.enter_interactive(model);
        let before = app.active;

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL));

        assert_ne!(app.active, before, "Ctrl+Tab must switch tab");
        assert!(!app.take_toggle_interactive(), "Ctrl+Tab must NOT request interactive teardown");
        assert!(app.take_term_input().is_none(), "Ctrl+Tab must not be forwarded to PTY");
    }

    #[test]
    fn interactive_plain_key_still_goes_to_pty() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        app.new_tab();
        let model = crate::terminal::TerminalModel::new(80, 24);
        app.enter_interactive(model);
        let before = app.active;

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));

        assert_eq!(app.active, before, "plain key must not switch tab");
        assert_eq!(app.take_term_input().as_deref(), Some(&b"a"[..]));
    }

    #[test]
    fn interactive_ctrl_n_creates_new_tab_without_exiting() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        let model = crate::terminal::TerminalModel::new(80, 24);
        app.enter_interactive(model);
        assert_eq!(app.sessions.len(), 1);

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));

        assert_eq!(app.sessions.len(), 2, "Ctrl+N must create a new tab");
        assert!(!app.take_toggle_interactive(), "Ctrl+N must NOT request interactive teardown");
        assert!(app.take_term_input().is_none(), "Ctrl+N must not be forwarded to PTY");
        // Old tab's terminal still alive in background.
        assert_eq!(app.sessions[0].mode, AppMode::Interactive);
        assert!(app.sessions[0].terminal.is_some(), "old tab terminal must persist");
    }

    #[test]
    fn interactive_ctrl_w_closes_tab() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        app.new_tab(); // 2 sessions, active = 1
        let model = crate::terminal::TerminalModel::new(80, 24);
        app.enter_interactive(model);

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));

        assert_eq!(app.sessions.len(), 1, "Ctrl+W must close the active tab");
        assert!(app.take_term_input().is_none(), "Ctrl+W must not be forwarded to PTY");
    }

    #[test]
    fn hide_view_keeps_terminal_alive() {
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        app.enter_interactive(crate::terminal::TerminalModel::new(80, 24));
        app.hide_interactive_view();
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.terminal.is_some(), "terminal model must persist");
    }

    #[test]
    fn show_view_restores_interactive() {
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        app.enter_interactive(crate::terminal::TerminalModel::new(80, 24));
        app.hide_interactive_view();
        app.show_interactive_view();
        assert_eq!(app.mode, AppMode::Interactive);
    }

    #[test]
    fn cyttrl_t_in_normal_shows_hidden_terminal() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("t0".into(), CommandConfirmMode::Always);
        app.enter_interactive(crate::terminal::TerminalModel::new(80, 24));
        app.hide_interactive_view();

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));

        assert_eq!(app.mode, AppMode::Interactive, "Ctrl+T must show hidden terminal");
        assert!(!app.take_toggle_interactive(), "must not request runner teardown");
    }
}
