//! File panels (Ctrl+G, #535): two side-by-side directory views, each over
//! its own source — the local disk or an SSH host — like a two-pane file
//! manager. This module is the state and the key handling; listing and
//! connecting happen in the runner, which drains [`FilePanels::requests`]
//! and answers with [`PanelEvent`]s.
//!
//! Nothing here writes anywhere: browsing only. Copying is #536.

use std::time::SystemTime;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use filar_transport::{DirEntry, EntryKind, PathStyle};

use crate::app::SessionId;

/// Most entries a panel shows; more are cut with a visible warning.
pub const MAX_PANEL_ENTRIES: usize = 5000;

/// Which panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Left panel.
    Left,
    /// Right panel.
    Right,
}

impl Side {
    /// The other panel.
    pub fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}

/// What a panel shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelSource {
    /// The client's own disk.
    Local,
    /// A configured SSH target (index into `App::ssh_targets`).
    Target(usize),
    /// The host of a tab: its SFTP channel rides on the tab's connection.
    TabHost(SessionId),
}

/// Loading state of a panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelStatus {
    /// Nothing chosen yet (waiting for the host overlay).
    Empty,
    /// Opening the source (connecting / starting SFTP).
    Connecting,
    /// Listing a directory.
    Loading,
    /// Listing shown.
    Ready,
    /// The last action failed; the message is shown in the panel.
    Error(String),
}

/// One row of a panel: `..` or a directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row<'a> {
    /// The parent directory.
    Parent,
    /// An entry of the current directory.
    Entry(&'a DirEntry),
}

/// One directory view.
#[derive(Debug, Clone)]
pub struct Panel {
    /// Source shown; `None` until one is chosen.
    pub source: Option<PanelSource>,
    /// Header label (`local`, a target name).
    pub label: String,
    /// Writes refused on this source (read-only host group, #419).
    pub read_only: bool,
    /// Path conventions of the source.
    pub style: PathStyle,
    /// Directory shown.
    pub path: String,
    /// Entries of `path`, sorted: directories first, then by name.
    pub entries: Vec<DirEntry>,
    /// Listing was cut at [`MAX_PANEL_ENTRIES`].
    pub truncated: bool,
    /// Cursor row (row 0 is `..` when the directory has a parent).
    pub cursor: usize,
    /// First visible row (kept by the renderer).
    pub scroll: usize,
    /// Current state.
    pub status: PanelStatus,
    /// Bumped on every new request; answers for an older one are dropped.
    pub generation: u64,
    /// Name to put the cursor on after the next listing (after going up).
    pub focus_after_load: Option<String>,
}

impl Panel {
    fn empty() -> Self {
        Self {
            source: None,
            label: String::new(),
            read_only: false,
            style: PathStyle::Posix,
            path: String::new(),
            entries: Vec::new(),
            truncated: false,
            cursor: 0,
            scroll: 0,
            status: PanelStatus::Empty,
            generation: 0,
            focus_after_load: None,
        }
    }

    /// Whether `..` is shown (the directory has a parent).
    pub fn has_parent(&self) -> bool {
        self.style.parent(&self.path).is_some()
    }

    /// Number of rows, `..` included.
    pub fn row_count(&self) -> usize {
        self.entries.len() + usize::from(self.has_parent())
    }

    /// Row `i`.
    pub fn row(&self, i: usize) -> Option<Row<'_>> {
        if self.has_parent() {
            if i == 0 {
                return Some(Row::Parent);
            }
            self.entries.get(i - 1).map(Row::Entry)
        } else {
            self.entries.get(i).map(Row::Entry)
        }
    }

    /// The row under the cursor.
    pub fn current(&self) -> Option<Row<'_>> {
        self.row(self.cursor)
    }

    fn move_cursor(&mut self, delta: isize) {
        let n = self.row_count();
        if n == 0 {
            self.cursor = 0;
            return;
        }
        let max = n as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
    }

    /// Whether the panel can be navigated (a listing is shown or failed).
    fn browsable(&self) -> bool {
        matches!(self.status, PanelStatus::Ready | PanelStatus::Error(_)) && !self.path.is_empty()
    }
}

/// Work for the runner.
#[derive(Debug, Clone)]
pub enum PanelRequest {
    /// Open `source` for `side`, then list `start_dir` (or the home
    /// directory when `None` or unreadable).
    Open {
        /// Panel.
        side: Side,
        /// What to open.
        source: PanelSource,
        /// Directory to show first.
        start_dir: Option<String>,
        /// Request generation.
        generation: u64,
    },
    /// List `path` on the source already open for `side`.
    List {
        /// Panel.
        side: Side,
        /// Directory.
        path: String,
        /// Request generation.
        generation: u64,
    },
    /// Open an SSH target whose secret the user just typed (no saved one).
    OpenTarget {
        /// Panel.
        side: Side,
        /// Target with the typed password / key passphrase filled in.
        target: filar_core::SshTarget,
        /// Request generation.
        generation: u64,
    },
}

/// Answers from the runner.
#[derive(Debug, Clone)]
pub enum PanelEvent {
    /// The source of `side` is open.
    Opened {
        /// Panel.
        side: Side,
        /// Generation of the request.
        generation: u64,
        /// Header label.
        label: String,
        /// Path conventions.
        style: PathStyle,
        /// Writes refused.
        read_only: bool,
    },
    /// A listing (or its failure).
    Listed {
        /// Panel.
        side: Side,
        /// Generation of the request.
        generation: u64,
        /// Directory listed.
        path: String,
        /// Entries, or the error text.
        result: Result<Vec<DirEntry>, String>,
    },
    /// No saved secret for the target: ask for it (masked).
    NeedSecret {
        /// Panel.
        side: Side,
        /// Generation of the request.
        generation: u64,
        /// The target to fill.
        target: filar_core::SshTarget,
        /// A typed key passphrase was wrong — ask again.
        retry: bool,
    },
    /// Opening failed.
    OpenFailed {
        /// Panel.
        side: Side,
        /// Generation of the request.
        generation: u64,
        /// What went wrong.
        error: String,
    },
}

/// What a key in the panels asks the app to do beyond panel state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelsAction {
    /// Nothing more.
    None,
    /// Close the panels.
    Close,
    /// Open the host overlay to choose the source of the active panel.
    ChooseSource(Side),
}

/// Masked entry of a password or key passphrase for a panel's host — only
/// when no saved secret exists, exactly as Ctrl+O asks.
#[derive(Clone)]
pub struct SecretPrompt {
    /// Panel being opened.
    pub side: Side,
    /// Generation of the open request.
    pub generation: u64,
    /// The target to fill (its `Debug` redacts secrets).
    pub target: filar_core::SshTarget,
    /// What was typed; never rendered, only its length as `*`.
    input: String,
    /// The previous key passphrase was wrong.
    pub retry: bool,
}

impl std::fmt::Debug for SecretPrompt {
    /// Never prints what was typed (AGENTS.md, invariant 3).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretPrompt")
            .field("side", &self.side)
            .field("generation", &self.generation)
            .field("target", &self.target)
            .field("input", &"<redacted>")
            .field("retry", &self.retry)
            .finish()
    }
}

impl SecretPrompt {
    /// Number of characters typed (rendered as `*`).
    pub fn typed_len(&self) -> usize {
        self.input.chars().count()
    }

    /// Whether the target logs in with a key (asks for its passphrase).
    pub fn is_key(&self) -> bool {
        matches!(self.target.auth, filar_core::SshAuth::Key { .. })
    }
}

/// Both panels.
#[derive(Debug, Clone)]
pub struct FilePanels {
    /// Left panel.
    pub left: Panel,
    /// Right panel.
    pub right: Panel,
    /// Panel receiving keys.
    pub active: Side,
    /// Pending work for the runner.
    pub requests: Vec<PanelRequest>,
    /// Masked secret entry in progress.
    pub prompt: Option<SecretPrompt>,
    next_generation: u64,
}

impl Default for FilePanels {
    fn default() -> Self {
        Self::new()
    }
}

impl FilePanels {
    /// Two empty panels, left active.
    pub fn new() -> Self {
        Self {
            left: Panel::empty(),
            right: Panel::empty(),
            active: Side::Left,
            requests: Vec::new(),
            prompt: None,
            next_generation: 1,
        }
    }

    /// Panel `side`.
    pub fn panel(&self, side: Side) -> &Panel {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    /// Panel `side`, mutable.
    pub fn panel_mut(&mut self, side: Side) -> &mut Panel {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }

    fn bump(&mut self, side: Side) -> u64 {
        let g = self.next_generation;
        self.next_generation += 1;
        self.panel_mut(side).generation = g;
        g
    }

    /// Show `source` in `side`, starting in `start_dir` when given.
    pub fn set_source(&mut self, side: Side, source: PanelSource, start_dir: Option<String>) {
        let generation = self.bump(side);
        let p = self.panel_mut(side);
        *p = Panel {
            generation,
            source: Some(source.clone()),
            status: PanelStatus::Connecting,
            ..Panel::empty()
        };
        self.requests.push(PanelRequest::Open {
            side,
            source,
            start_dir,
            generation,
        });
    }

    fn list(&mut self, side: Side, path: String) {
        let generation = self.bump(side);
        self.panel_mut(side).status = PanelStatus::Loading;
        self.requests.push(PanelRequest::List {
            side,
            path,
            generation,
        });
    }

    /// Apply an answer from the runner. Stale answers are ignored.
    pub fn apply(&mut self, event: PanelEvent) {
        match event {
            PanelEvent::Opened {
                side,
                generation,
                label,
                style,
                read_only,
            } => {
                let p = self.panel_mut(side);
                if p.generation != generation {
                    return;
                }
                p.label = label;
                p.style = style;
                p.read_only = read_only;
                // The listing follows from the same request.
                p.status = PanelStatus::Loading;
            }
            PanelEvent::NeedSecret {
                side,
                generation,
                target,
                retry,
            } => {
                if self.panel(side).generation != generation {
                    return;
                }
                self.panel_mut(side).label = target.name.clone();
                self.prompt = Some(SecretPrompt {
                    side,
                    generation,
                    target,
                    input: String::new(),
                    retry,
                });
            }
            PanelEvent::OpenFailed {
                side,
                generation,
                error,
            } => {
                let p = self.panel_mut(side);
                if p.generation != generation {
                    return;
                }
                p.status = PanelStatus::Error(error);
            }
            PanelEvent::Listed {
                side,
                generation,
                path,
                result,
            } => {
                let p = self.panel_mut(side);
                if p.generation != generation {
                    return;
                }
                match result {
                    Ok(mut entries) => {
                        sort_entries(&mut entries);
                        p.truncated = entries.len() > MAX_PANEL_ENTRIES;
                        entries.truncate(MAX_PANEL_ENTRIES);
                        p.entries = entries;
                        p.path = path;
                        p.status = PanelStatus::Ready;
                        p.scroll = 0;
                        p.cursor = 0;
                        if let Some(name) = p.focus_after_load.take() {
                            let offset = usize::from(p.has_parent());
                            if let Some(i) = p.entries.iter().position(|e| e.name == name) {
                                p.cursor = i + offset;
                            }
                        }
                    }
                    Err(e) => {
                        p.focus_after_load = None;
                        // Keep the previous listing visible; the error is
                        // shown above it.
                        p.status = PanelStatus::Error(e);
                    }
                }
            }
        }
    }

    /// Handle a key. Returns what the app must do besides panel state.
    pub fn handle_key(&mut self, key: KeyEvent) -> PanelsAction {
        if self.prompt.is_some() {
            self.prompt_key(key);
            return PanelsAction::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl {
            return match key.code {
                KeyCode::Char('g' | 'G' | 'п' | 'П') => PanelsAction::Close,
                KeyCode::Char('o' | 'O' | 'щ' | 'Щ') => PanelsAction::ChooseSource(self.active),
                _ => PanelsAction::None,
            };
        }
        let side = self.active;
        match key.code {
            KeyCode::Esc => return PanelsAction::Close,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                self.active = self.active.other();
            }
            KeyCode::Up | KeyCode::Char('k' | 'л') => self.panel_mut(side).move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j' | 'о') => self.panel_mut(side).move_cursor(1),
            KeyCode::PageUp => self.panel_mut(side).move_cursor(-PAGE),
            KeyCode::PageDown => self.panel_mut(side).move_cursor(PAGE),
            KeyCode::Home => self.panel_mut(side).cursor = 0,
            KeyCode::End => {
                let p = self.panel_mut(side);
                p.cursor = p.row_count().saturating_sub(1);
            }
            KeyCode::Enter => self.enter(side),
            KeyCode::Backspace => self.go_up(side),
            KeyCode::Char('r' | 'R' | 'к' | 'К') => self.refresh(side),
            _ => {}
        }
        PanelsAction::None
    }

    /// Keys while the secret prompt is open: type, erase, submit, cancel.
    fn prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                let side = prompt.side;
                self.prompt = None;
                self.panel_mut(side).status = PanelStatus::Error("cancelled".into());
            }
            KeyCode::Backspace => {
                prompt.input.pop();
            }
            KeyCode::Enter => {
                let Some(mut prompt) = self.prompt.take() else {
                    return;
                };
                let secret = std::mem::take(&mut prompt.input);
                match &mut prompt.target.auth {
                    filar_core::SshAuth::Key { passphrase, .. } => *passphrase = Some(secret),
                    auth => {
                        *auth = filar_core::SshAuth::Password {
                            password: Some(secret),
                        }
                    }
                }
                let side = prompt.side;
                let generation = self.bump(side);
                self.panel_mut(side).status = PanelStatus::Connecting;
                self.requests.push(PanelRequest::OpenTarget {
                    side,
                    target: prompt.target,
                    generation,
                });
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                prompt.input.push(c);
            }
            _ => {}
        }
    }

    fn enter(&mut self, side: Side) {
        let p = self.panel(side);
        if !p.browsable() {
            return;
        }
        let target = match p.current() {
            Some(Row::Parent) => {
                self.go_up(side);
                return;
            }
            Some(Row::Entry(e)) if is_dir_like(e) => p.style.join(&p.path, &e.name),
            _ => return,
        };
        self.list(side, target);
    }

    fn go_up(&mut self, side: Side) {
        let p = self.panel(side);
        if !p.browsable() {
            return;
        }
        let Some(parent) = p.style.parent(&p.path) else {
            return;
        };
        // Leaving a drive root for the drive list (Windows): the drive is
        // listed under its full root name, `C:\`.
        let child = if parent.is_empty() {
            p.path.clone()
        } else {
            p.style.file_name(&p.path)
        };
        self.panel_mut(side).focus_after_load = Some(child);
        self.list(side, parent);
    }

    /// Re-list the current directory, or re-open a source whose opening failed.
    fn refresh(&mut self, side: Side) {
        let p = self.panel(side);
        if !p.path.is_empty() {
            let focus = match p.current() {
                Some(Row::Entry(e)) => Some(e.name.clone()),
                _ => None,
            };
            let path = p.path.clone();
            self.panel_mut(side).focus_after_load = focus;
            self.list(side, path);
        } else if let Some(source) = p.source.clone() {
            self.set_source(side, source, None);
        }
    }
}

/// Rows a page key moves.
const PAGE: isize = 15;

/// A directory, or a symlink that may point at one (entered by trying).
fn is_dir_like(e: &DirEntry) -> bool {
    matches!(e.metadata.kind, EntryKind::Dir | EntryKind::Symlink)
}

/// Directories first, then by name (case-insensitive, then exact).
pub fn sort_entries(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| {
        let da = a.metadata.kind == EntryKind::Dir;
        let db = b.metadata.kind == EntryKind::Dir;
        db.cmp(&da)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Human size: `999`, `1.2K`, `34M`, `5.0G`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["K", "M", "G", "T", "P"];
    if bytes < 1024 {
        return bytes.to_string();
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1}{}", UNITS[unit])
    } else {
        format!("{:.0}{}", v, UNITS[unit])
    }
}

/// Modification time as `YYYY-MM-DD HH:MM` in local time, or blank.
pub fn format_mtime(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filar_transport::Metadata;

    fn entry(name: &str, kind: EntryKind) -> DirEntry {
        DirEntry {
            name: name.into(),
            metadata: Metadata {
                kind,
                size: 10,
                modified: None,
                mode: None,
            },
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Panels with `side` opened on a POSIX source and listed at `path`.
    fn opened(side: Side, path: &str, entries: Vec<DirEntry>) -> FilePanels {
        let mut fp = FilePanels::new();
        fp.set_source(side, PanelSource::Local, None);
        let generation = fp.panel(side).generation;
        fp.apply(PanelEvent::Opened {
            side,
            generation,
            label: "local".into(),
            style: PathStyle::Posix,
            read_only: false,
        });
        fp.apply(PanelEvent::Listed {
            side,
            generation,
            path: path.into(),
            result: Ok(entries),
        });
        fp.requests.clear();
        fp
    }

    #[test]
    fn listing_sorts_dirs_first_and_shows_parent() {
        let fp = opened(
            Side::Left,
            "/srv",
            vec![
                entry("b.txt", EntryKind::File),
                entry("Zeta", EntryKind::Dir),
                entry("alpha", EntryKind::Dir),
                entry("A.txt", EntryKind::File),
            ],
        );
        let p = fp.panel(Side::Left);
        assert_eq!(p.status, PanelStatus::Ready);
        assert_eq!(p.row(0), Some(Row::Parent));
        let names: Vec<_> = p.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "Zeta", "A.txt", "b.txt"]);
        assert_eq!(p.row_count(), 5);
    }

    #[test]
    fn root_has_no_parent_row() {
        let fp = opened(Side::Left, "/", vec![entry("etc", EntryKind::Dir)]);
        let p = fp.panel(Side::Left);
        assert!(!p.has_parent());
        assert_eq!(p.row(0), Some(Row::Entry(&p.entries[0])));
    }

    #[test]
    fn enter_on_a_dir_lists_it_and_backspace_goes_up_with_focus() {
        let mut fp = opened(
            Side::Left,
            "/srv",
            vec![entry("app", EntryKind::Dir), entry("f", EntryKind::File)],
        );
        fp.handle_key(key(KeyCode::Down)); // onto "app"
        fp.handle_key(key(KeyCode::Enter));
        let g = fp.left.generation;
        assert!(matches!(
            fp.requests.pop(),
            Some(PanelRequest::List { side: Side::Left, ref path, generation })
                if path == "/srv/app" && generation == g
        ));
        fp.apply(PanelEvent::Listed {
            side: Side::Left,
            generation: g,
            path: "/srv/app".into(),
            result: Ok(vec![]),
        });
        fp.handle_key(key(KeyCode::Backspace));
        let g = fp.left.generation;
        assert!(matches!(
            fp.requests.pop(),
            Some(PanelRequest::List { side: Side::Left, ref path, generation })
                if path == "/srv" && generation == g
        ));
        fp.apply(PanelEvent::Listed {
            side: Side::Left,
            generation: g,
            path: "/srv".into(),
            result: Ok(vec![
                entry("app", EntryKind::Dir),
                entry("f", EntryKind::File),
            ]),
        });
        // Cursor comes back onto the directory we left.
        assert_eq!(fp.left.current(), Some(Row::Entry(&fp.left.entries[0])));
    }

    #[test]
    fn enter_on_a_file_does_nothing_and_parent_row_goes_up() {
        let mut fp = opened(Side::Left, "/srv", vec![entry("f", EntryKind::File)]);
        fp.handle_key(key(KeyCode::Down));
        fp.handle_key(key(KeyCode::Enter));
        assert!(fp.requests.is_empty());
        fp.handle_key(key(KeyCode::Home));
        fp.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            fp.requests.pop(),
            Some(PanelRequest::List { path, .. }) if path == "/"
        ));
    }

    #[test]
    fn stale_answers_are_dropped() {
        let mut fp = opened(Side::Left, "/srv", vec![entry("a", EntryKind::Dir)]);
        let old = fp.left.generation;
        fp.handle_key(key(KeyCode::Char('r')));
        fp.apply(PanelEvent::Listed {
            side: Side::Left,
            generation: old,
            path: "/elsewhere".into(),
            result: Ok(vec![]),
        });
        assert_eq!(fp.left.path, "/srv");
        assert_eq!(fp.left.status, PanelStatus::Loading);
    }

    #[test]
    fn tab_switches_and_keys_go_to_the_active_panel_only() {
        let mut fp = opened(
            Side::Left,
            "/a",
            vec![entry("x", EntryKind::File), entry("y", EntryKind::File)],
        );
        assert_eq!(fp.active, Side::Left);
        fp.handle_key(key(KeyCode::Tab));
        assert_eq!(fp.active, Side::Right);
        fp.handle_key(key(KeyCode::Down));
        assert_eq!(fp.left.cursor, 0);
        fp.handle_key(key(KeyCode::Tab));
        fp.handle_key(key(KeyCode::End));
        assert_eq!(fp.left.cursor, 2);
    }

    #[test]
    fn ctrl_keys_close_and_choose_source_on_both_layouts() {
        let mut fp = FilePanels::new();
        assert_eq!(fp.handle_key(ctrl('g')), PanelsAction::Close);
        assert_eq!(fp.handle_key(ctrl('п')), PanelsAction::Close);
        assert_eq!(fp.handle_key(key(KeyCode::Esc)), PanelsAction::Close);
        fp.active = Side::Right;
        assert_eq!(
            fp.handle_key(ctrl('o')),
            PanelsAction::ChooseSource(Side::Right)
        );
        assert_eq!(
            fp.handle_key(ctrl('щ')),
            PanelsAction::ChooseSource(Side::Right)
        );
    }

    #[test]
    fn a_failed_listing_keeps_the_old_one_and_shows_the_error() {
        let mut fp = opened(Side::Left, "/srv", vec![entry("a", EntryKind::Dir)]);
        fp.handle_key(key(KeyCode::Down));
        fp.handle_key(key(KeyCode::Enter));
        let g = fp.left.generation;
        fp.apply(PanelEvent::Listed {
            side: Side::Left,
            generation: g,
            path: "/srv/a".into(),
            result: Err("permission denied".into()),
        });
        assert_eq!(fp.left.path, "/srv");
        assert_eq!(fp.left.entries.len(), 1);
        assert_eq!(
            fp.left.status,
            PanelStatus::Error("permission denied".into())
        );
        // Still navigable after the error.
        fp.handle_key(key(KeyCode::Enter));
        assert!(!fp.requests.is_empty());
    }

    #[test]
    fn refresh_after_a_failed_open_reopens_the_source() {
        let mut fp = FilePanels::new();
        fp.set_source(Side::Right, PanelSource::Target(2), None);
        let g = fp.right.generation;
        fp.requests.clear();
        fp.apply(PanelEvent::OpenFailed {
            side: Side::Right,
            generation: g,
            error: "no route".into(),
        });
        fp.active = Side::Right;
        fp.handle_key(key(KeyCode::Char('r')));
        assert!(matches!(
            fp.requests.pop(),
            Some(PanelRequest::Open {
                source: PanelSource::Target(2),
                ..
            })
        ));
    }

    #[test]
    fn listing_is_capped() {
        let many: Vec<_> = (0..MAX_PANEL_ENTRIES + 3)
            .map(|i| entry(&format!("f{i}"), EntryKind::File))
            .collect();
        let fp = opened(Side::Left, "/big", many);
        assert!(fp.left.truncated);
        assert_eq!(fp.left.entries.len(), MAX_PANEL_ENTRIES);
    }

    #[test]
    fn windows_paths_go_up_to_the_drive_list() {
        let mut fp = FilePanels::new();
        fp.set_source(Side::Left, PanelSource::Local, None);
        let g = fp.left.generation;
        fp.apply(PanelEvent::Opened {
            side: Side::Left,
            generation: g,
            label: "local".into(),
            style: PathStyle::Windows,
            read_only: false,
        });
        fp.apply(PanelEvent::Listed {
            side: Side::Left,
            generation: g,
            path: "C:\\".into(),
            result: Ok(vec![entry("Users", EntryKind::Dir)]),
        });
        fp.requests.clear();
        fp.handle_key(key(KeyCode::Backspace));
        assert!(matches!(
            fp.requests.pop(),
            Some(PanelRequest::List { path, .. }) if path.is_empty()
        ));
        assert_eq!(fp.left.focus_after_load.as_deref(), Some("C:\\"));
    }

    fn password_target() -> filar_core::SshTarget {
        filar_core::SshTarget {
            name: "web1".into(),
            host: "10.0.0.5".into(),
            port: 22,
            user: "admin".into(),
            auth: filar_core::SshAuth::Password { password: None },
            host_key_policy: filar_core::HostKeyPolicy::Tofu,
            tags: Vec::new(),
        }
    }

    #[test]
    fn a_missing_secret_is_typed_masked_and_sent_with_the_target() {
        let mut fp = FilePanels::new();
        fp.set_source(Side::Right, PanelSource::Target(0), None);
        let g = fp.right.generation;
        fp.requests.clear();
        fp.apply(PanelEvent::NeedSecret {
            side: Side::Right,
            generation: g,
            target: password_target(),
            retry: false,
        });
        assert!(fp.prompt.is_some());
        for c in "s3cr".chars() {
            fp.handle_key(key(KeyCode::Char(c)));
        }
        fp.handle_key(key(KeyCode::Backspace));
        fp.handle_key(key(KeyCode::Char('t')));
        // Keys go to the prompt, not the panel.
        assert_eq!(fp.handle_key(key(KeyCode::Tab)), PanelsAction::None);
        assert_eq!(fp.active, Side::Left);
        assert_eq!(fp.prompt.as_ref().map(SecretPrompt::typed_len), Some(4));
        fp.handle_key(key(KeyCode::Enter));
        assert!(fp.prompt.is_none());
        match fp.requests.pop() {
            Some(PanelRequest::OpenTarget {
                side,
                target,
                generation,
            }) => {
                assert_eq!(side, Side::Right);
                assert_eq!(generation, fp.right.generation);
                assert!(matches!(
                    target.auth,
                    filar_core::SshAuth::Password { password: Some(ref p) } if p == "s3ct"
                ));
            }
            other => panic!("expected OpenTarget, got {other:?}"),
        }
        assert_eq!(fp.right.status, PanelStatus::Connecting);
    }

    #[test]
    fn debug_never_shows_the_typed_secret() {
        let mut fp = FilePanels::new();
        fp.set_source(Side::Left, PanelSource::Target(0), None);
        let g = fp.left.generation;
        fp.apply(PanelEvent::NeedSecret {
            side: Side::Left,
            generation: g,
            target: password_target(),
            retry: false,
        });
        for c in "hunter2".chars() {
            fp.handle_key(key(KeyCode::Char(c)));
        }
        let dump = format!("{fp:?}");
        assert!(!dump.contains("hunter2"), "{dump}");
    }

    #[test]
    fn esc_cancels_the_secret_prompt() {
        let mut fp = FilePanels::new();
        fp.set_source(Side::Left, PanelSource::Target(0), None);
        let g = fp.left.generation;
        fp.apply(PanelEvent::NeedSecret {
            side: Side::Left,
            generation: g,
            target: password_target(),
            retry: false,
        });
        assert_eq!(fp.handle_key(key(KeyCode::Esc)), PanelsAction::None);
        assert!(fp.prompt.is_none());
        assert_eq!(fp.left.status, PanelStatus::Error("cancelled".into()));
        // The next Esc closes the panels.
        assert_eq!(fp.handle_key(key(KeyCode::Esc)), PanelsAction::Close);
    }

    #[test]
    fn sizes_read_like_ls_h() {
        assert_eq!(human_size(999), "999");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(35 * 1024 * 1024), "35M");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0G");
    }
}
