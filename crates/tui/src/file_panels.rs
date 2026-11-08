//! File panels (Ctrl+G, #535): two side-by-side directory views, each over
//! its own source — the local disk or an SSH host — like a two-pane file
//! manager. This module is the state and the key handling; listing and
//! connecting happen in the runner, which drains [`FilePanels::requests`]
//! and answers with [`PanelEvent`]s.
//!
//! Copying (#536) is planned here (marks, confirmation) and run by the
//! runner over the copy engine; [`FileCopy`] tracks one copy, which outlives
//! the panels (it keeps going in the background, listed under Ctrl+J).

use std::collections::BTreeSet;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use filar_transport::{CopyEvent, CopyReport, CopySource, DirEntry, EntryKind, PathStyle};

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
    /// Entries marked for copying (by name, in the directory shown).
    pub marked: BTreeSet<String>,
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
            marked: BTreeSet::new(),
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
    /// Create folder `name` in `dir` of the source open for `side` (#546).
    CreateDir {
        /// Panel.
        side: Side,
        /// Directory to create it in.
        dir: String,
        /// New folder's name (already checked).
        name: String,
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
    /// A folder was created — or could not be (#546).
    DirCreated {
        /// Panel.
        side: Side,
        /// Directory it was created in.
        dir: String,
        /// Its name.
        name: String,
        /// What went wrong, if anything.
        result: Result<(), String>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelsAction {
    /// Nothing more.
    None,
    /// Close the panels.
    Close,
    /// Open the host overlay to choose the source of the active panel.
    ChooseSource(Side),
    /// The user confirmed this copy: start it.
    StartCopy(CopyPlan),
    /// Cancel the running copy (Ctrl+Z).
    CancelCopy,
}

/// A copy the user asked for, waiting for (or past) confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyPlan {
    /// Panel copied from.
    pub from: Side,
    /// Source of the `from` panel.
    pub from_source: PanelSource,
    /// Source of the target panel.
    pub to_source: PanelSource,
    /// What to copy (full paths on the source side).
    pub sources: Vec<CopySource>,
    /// Target directory.
    pub dst_dir: String,
    /// Target label (`host`), for messages.
    pub dst_label: String,
    /// Files among the sources (directories are walked by the engine).
    pub files: usize,
    /// Directories among the sources.
    pub dirs: usize,
    /// Size of the files among the sources.
    pub bytes: u64,
}

impl CopyPlan {
    /// `3 files (12M) and 2 folders` — what the confirmation names.
    pub fn what(&self) -> String {
        let files = match self.files {
            0 => String::new(),
            1 => format!("1 file ({})", human_size(self.bytes)),
            n => format!("{n} files ({})", human_size(self.bytes)),
        };
        let dirs = match self.dirs {
            0 => String::new(),
            1 => "1 folder".to_string(),
            n => format!("{n} folders"),
        };
        match (files.is_empty(), dirs.is_empty()) {
            (false, false) => format!("{files} and {dirs}"),
            (false, true) => files,
            _ => dirs,
        }
    }

    /// `host:/dir` of the target.
    pub fn target(&self) -> String {
        format!("{}:{}", self.dst_label, self.dst_dir)
    }
}

/// Work for the runner about copies (kept in the app: copies outlive the
/// panels).
#[derive(Debug, Clone)]
pub enum CopyRequest {
    /// Start copy `id`.
    Start {
        /// Copy number.
        id: u64,
        /// What to copy.
        plan: CopyPlan,
    },
    /// Cancel copy `id`.
    Cancel {
        /// Copy number.
        id: u64,
    },
    /// The user's answer to copy `id`'s overwrite question.
    Answer {
        /// Copy number.
        id: u64,
        /// The answer.
        decision: filar_transport::OverwriteDecision,
    },
}

/// A modal question inside the panels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelDialog {
    /// "Copy … → host:/dir?" — every copy is confirmed (#536).
    ConfirmCopy(CopyPlan),
    /// Name of a new folder in the active panel's directory (#546).
    NewFolder {
        /// Panel the folder goes into.
        side: Side,
        /// Name typed so far.
        input: String,
        /// Why the last Enter was refused.
        error: Option<String>,
    },
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
    /// Open question (copy confirmation).
    pub dialog: Option<PanelDialog>,
    /// One-line notice under the panels (why a copy cannot start, …).
    pub notice: Option<String>,
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
            dialog: None,
            notice: None,
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
            PanelEvent::DirCreated {
                side,
                dir,
                name,
                result,
            } => match result {
                Ok(()) => {
                    self.notice = Some(format!("Folder {name} created"));
                    if self.panel(side).path == dir {
                        self.panel_mut(side).focus_after_load = Some(name);
                        self.list(side, dir);
                    }
                }
                Err(e) => self.notice = Some(format!("Could not create {name}: {e}")),
            },
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
                        if p.path != path {
                            p.marked.clear();
                        } else {
                            p.marked.retain(|m| entries.iter().any(|e| &e.name == m));
                        }
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
        if let Some(PanelDialog::NewFolder { .. }) = self.dialog {
            self.new_folder_key(key);
            return PanelsAction::None;
        }
        if let Some(PanelDialog::ConfirmCopy(plan)) = self.dialog.take() {
            return match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y' | 'н' | 'Н') => {
                    self.panel_mut(plan.from).marked.clear();
                    PanelsAction::StartCopy(plan)
                }
                KeyCode::Esc | KeyCode::Char('n' | 'N' | 'т' | 'Т') => PanelsAction::None,
                _ => {
                    self.dialog = Some(PanelDialog::ConfirmCopy(plan));
                    PanelsAction::None
                }
            };
        }
        self.notice = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl {
            return match key.code {
                KeyCode::Char('g' | 'G' | 'п' | 'П') => PanelsAction::Close,
                KeyCode::Char('o' | 'O' | 'щ' | 'Щ') => PanelsAction::ChooseSource(self.active),
                KeyCode::Char('z' | 'Z' | 'я' | 'Я') => PanelsAction::CancelCopy,
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
            KeyCode::Char(' ') | KeyCode::Insert => self.toggle_mark(side),
            KeyCode::F(5) | KeyCode::Char('c' | 'C' | 'с' | 'С') => self.plan_copy(),
            KeyCode::F(7) | KeyCode::Char('m' | 'M' | 'ь' | 'Ь') => self.open_new_folder(side),
            _ => {}
        }
        PanelsAction::None
    }

    /// Ask for the name of a new folder in `side`'s directory (#546) — or
    /// say why there cannot be one there.
    fn open_new_folder(&mut self, side: Side) {
        let p = self.panel(side);
        let why = if p.source.is_none() {
            Some("Choose a host for this panel first (Ctrl+O)".to_string())
        } else if p.status != PanelStatus::Ready {
            Some("The panel is not ready".to_string())
        } else if p.style == PathStyle::Windows && p.path.is_empty() {
            Some("This is the list of drives: open a drive to create a folder there".to_string())
        } else if p.path.is_empty() {
            Some("The panel is not ready".to_string())
        } else if p.read_only {
            Some(format!("{} is read-only: no folder can be created there", p.label))
        } else {
            None
        };
        match why {
            Some(why) => self.notice = Some(why),
            None => {
                self.dialog = Some(PanelDialog::NewFolder {
                    side,
                    input: String::new(),
                    error: None,
                })
            }
        }
    }

    /// Keys in the new-folder dialog: type, erase, create, cancel.
    fn new_folder_key(&mut self, key: KeyEvent) {
        let Some(PanelDialog::NewFolder { side, input, error }) = self.dialog.as_mut() else {
            return;
        };
        let side = *side;
        match key.code {
            KeyCode::Esc => self.dialog = None,
            KeyCode::Backspace => {
                input.pop();
                *error = None;
            }
            KeyCode::Enter => {
                let name = input.clone();
                let p = self.panel(side);
                match check_folder_name(p.style, &name) {
                    Err(e) => {
                        if let Some(PanelDialog::NewFolder { error, .. }) = self.dialog.as_mut() {
                            *error = Some(e);
                        }
                    }
                    Ok(()) => {
                        let dir = p.path.clone();
                        self.dialog = None;
                        self.requests.push(PanelRequest::CreateDir { side, dir, name });
                    }
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                input.push(c);
                *error = None;
            }
            _ => {}
        }
    }

    /// Mark or unmark the entry under the cursor, then move down.
    fn toggle_mark(&mut self, side: Side) {
        let p = self.panel_mut(side);
        if !p.browsable() {
            return;
        }
        if let Some(Row::Entry(e)) = p.current() {
            let name = e.name.clone();
            if !p.marked.remove(&name) {
                p.marked.insert(name);
            }
        }
        p.move_cursor(1);
    }

    /// Plan a copy from the active panel into the other one's directory and
    /// ask for confirmation — or say why it cannot happen.
    fn plan_copy(&mut self) {
        match self.copy_plan() {
            Ok(plan) => self.dialog = Some(PanelDialog::ConfirmCopy(plan)),
            Err(why) => self.notice = Some(why),
        }
    }

    /// The copy the keys ask for: marked entries of the active panel (or the
    /// one under the cursor) into the other panel's directory.
    pub fn copy_plan(&self) -> Result<CopyPlan, String> {
        let from = self.active;
        let src = self.panel(from);
        let dst = self.panel(from.other());
        let (Some(from_source), Some(to_source)) = (src.source.clone(), dst.source.clone()) else {
            return Err("Choose a host for both panels first (Ctrl+O)".into());
        };
        if src.status != PanelStatus::Ready || src.path.is_empty() {
            return Err("The source panel is not ready".into());
        }
        if dst.status != PanelStatus::Ready || dst.path.is_empty() {
            return Err("The target panel is not ready".into());
        }
        if dst.read_only {
            return Err(format!("{} is read-only: nothing can be copied there", dst.label));
        }
        if dst.style == PathStyle::Windows && dst.path.is_empty() {
            return Err("Open a drive in the target panel first".into());
        }
        let picked: Vec<&DirEntry> = if src.marked.is_empty() {
            match src.current() {
                Some(Row::Entry(e)) => vec![e],
                _ => return Err("Nothing to copy: put the cursor on a file or mark some (Space)".into()),
            }
        } else {
            src.entries.iter().filter(|e| src.marked.contains(&e.name)).collect()
        };
        if picked.is_empty() {
            return Err("Nothing to copy".into());
        }
        let same_fs = from_source == to_source;
        if same_fs && src.path == dst.path {
            return Err("Source and target are the same folder".into());
        }
        let mut plan = CopyPlan {
            from,
            from_source,
            to_source,
            sources: Vec::new(),
            dst_dir: dst.path.clone(),
            dst_label: dst.label.clone(),
            files: 0,
            dirs: 0,
            bytes: 0,
        };
        for e in picked {
            let path = src.style.join(&src.path, &e.name);
            if same_fs && e.metadata.kind == EntryKind::Dir && is_within(src.style, &dst.path, &path) {
                return Err(format!("Cannot copy {} into itself", e.name));
            }
            if e.metadata.kind == EntryKind::Dir {
                plan.dirs += 1;
            } else {
                plan.files += 1;
                plan.bytes += e.metadata.size;
            }
            plan.sources.push(CopySource {
                path,
                kind: e.metadata.kind,
            });
        }
        Ok(plan)
    }

    /// Re-list a panel that shows `dir` of `source` (after a copy into it).
    pub fn refresh_if_showing(&mut self, source: &PanelSource, dir: &str) {
        for side in [Side::Left, Side::Right] {
            let p = self.panel(side);
            if p.source.as_ref() == Some(source) && p.path == dir && p.status == PanelStatus::Ready {
                let focus = match p.current() {
                    Some(Row::Entry(e)) => Some(e.name.clone()),
                    _ => None,
                };
                self.panel_mut(side).focus_after_load = focus;
                self.list(side, dir.to_string());
            }
        }
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

/// Whether `name` can be a new folder's name on a side with `style` (#546).
/// Checked before anything is sent: a name that would land elsewhere (`..`,
/// a separator) or that the side's filesystem refuses never leaves.
pub fn check_folder_name(style: PathStyle, name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("Type a name".into());
    }
    if name == "." || name == ".." {
        return Err("\".\" and \"..\" are not folder names".into());
    }
    if name.chars().any(char::is_control) {
        return Err("The name has a control character".into());
    }
    match style {
        PathStyle::Posix => {
            if name.contains('/') {
                return Err("A name cannot contain /".into());
            }
        }
        PathStyle::Windows => {
            if let Some(c) = name.chars().find(|c| "\\/:*?\"<>|".contains(*c)) {
                return Err(format!("A name cannot contain {c}"));
            }
            if name.ends_with(' ') || name.ends_with('.') {
                return Err("A name cannot end with a space or a dot".into());
            }
        }
    }
    Ok(())
}

/// `path` is `dir` itself or lies below it.
fn is_within(style: PathStyle, path: &str, dir: &str) -> bool {
    let mut cur = Some(path.to_string());
    while let Some(c) = cur {
        if c == dir {
            return true;
        }
        cur = style.parent(&c);
    }
    false
}

/// How a copy ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyState {
    /// Still copying.
    Running,
    /// Every file copied or skipped on purpose.
    Done,
    /// At least one file failed.
    Failed,
    /// Stopped by the user.
    Cancelled,
}

/// One copy started from the panels; lives in the app, not in the panels,
/// because it keeps running after they close (#536).
#[derive(Debug, Clone)]
pub struct FileCopy {
    /// Number of this copy in the session (`copy-N` in Ctrl+J).
    pub id: u64,
    /// Tab the copy was started from.
    pub session: SessionId,
    /// What was asked for.
    pub plan: CopyPlan,
    /// Files and bytes to copy, once the engine walked the sources.
    pub files_total: u64,
    /// Total bytes planned.
    pub bytes_total: u64,
    /// Bytes done (copied, skipped or failed).
    pub done: u64,
    /// Files finished (copied, skipped or failed).
    pub files_done: u64,
    /// File being copied.
    pub current: Option<String>,
    /// When it started.
    pub started: Instant,
    /// When it ended.
    pub ended: Option<Instant>,
    /// Where it stands.
    pub state: CopyState,
    /// Final report.
    pub report: Option<CopyReport>,
    /// Existing target the engine is asking about (overwrite question).
    pub asking: Option<String>,
}

impl FileCopy {
    /// A copy that just started.
    pub fn new(id: u64, session: SessionId, plan: CopyPlan) -> Self {
        Self {
            id,
            session,
            bytes_total: plan.bytes,
            files_total: plan.files as u64,
            plan,
            done: 0,
            files_done: 0,
            current: None,
            started: Instant::now(),
            ended: None,
            state: CopyState::Running,
            report: None,
            asking: None,
        }
    }

    /// Apply a progress event from the engine.
    pub fn apply(&mut self, event: CopyEvent) {
        match event {
            CopyEvent::Planned { files, bytes } => {
                self.files_total = files;
                self.bytes_total = bytes;
            }
            CopyEvent::FileStarted { dst, .. } => self.current = Some(dst),
            CopyEvent::Progress { done, total } => {
                self.done = done;
                self.bytes_total = total;
            }
            CopyEvent::FileDone { .. } | CopyEvent::FileSkipped { .. } => {
                self.files_done += 1;
                self.current = None;
            }
            CopyEvent::Failed { .. } => {
                self.files_done += 1;
                self.current = None;
            }
            CopyEvent::Warning(_) => {}
        }
    }

    /// The engine finished.
    pub fn finish(&mut self, report: CopyReport) {
        self.state = if report.cancelled {
            CopyState::Cancelled
        } else if !report.failed.is_empty() {
            CopyState::Failed
        } else {
            CopyState::Done
        };
        self.asking = None;
        self.current = None;
        self.ended = Some(Instant::now());
        self.report = Some(report);
    }

    /// Whether the copy is still going.
    pub fn running(&self) -> bool {
        self.state == CopyState::Running
    }

    /// Whole percent done, when the total is known.
    pub fn percent(&self) -> Option<u64> {
        (self.bytes_total > 0).then(|| (self.done.min(self.bytes_total) * 100) / self.bytes_total)
    }

    /// Bytes per second so far.
    pub fn speed(&self) -> u64 {
        let elapsed = self.ended.unwrap_or_else(Instant::now) - self.started;
        let secs = elapsed.max(Duration::from_millis(100)).as_secs_f64();
        (self.done as f64 / secs) as u64
    }

    /// One line on where the copy stands.
    pub fn status_line(&self) -> String {
        match self.state {
            CopyState::Running => {
                let pct = self.percent().map(|p| format!("{p}% · ")).unwrap_or_default();
                let current = self
                    .current
                    .as_deref()
                    .map(|c| format!(" · {c}"))
                    .unwrap_or_default();
                format!(
                    "Copying {}/{} · {pct}{}/{} · {}/s{current}",
                    self.files_done,
                    self.files_total,
                    human_size(self.done),
                    human_size(self.bytes_total),
                    human_size(self.speed()),
                )
            }
            _ => self.summary(),
        }
    }

    /// The result in one sentence, with what the user must know: failures
    /// and part files that could not be removed (#531).
    pub fn summary(&self) -> String {
        let Some(r) = &self.report else {
            return "Copy running".into();
        };
        let head = match self.state {
            CopyState::Cancelled => "Copy cancelled",
            CopyState::Failed => "Copy finished with errors",
            _ => "Copy done",
        };
        let mut out = format!(
            "{head} → {}: {} copied, {} skipped, {} failed",
            self.plan.target(),
            r.copied,
            r.skipped,
            r.failed.len()
        );
        if let Some((path, why)) = r.failed.first() {
            out.push_str(&format!(" (first: {path}: {why})"));
        }
        if !r.leftover_parts.is_empty() {
            out.push_str(&format!(
                ". Left behind, remove by hand: {}",
                r.leftover_parts.join(", ")
            ));
        }
        out
    }

    /// This copy as a background operation for the Ctrl+J panel.
    pub fn operation(&self) -> crate::ops::Operation {
        use crate::ops::{HostOpState, OpHost, Operation};
        let state = match self.state {
            CopyState::Running => HostOpState::Running,
            CopyState::Done => HostOpState::Done,
            CopyState::Failed => HostOpState::Failed,
            CopyState::Cancelled => HostOpState::Cancelled,
        };
        Operation {
            session: self.session,
            id: format!("copy-{}", self.id),
            label: format!("copy {} → {}", self.plan.what(), self.plan.target()),
            hosts: vec![OpHost {
                name: self.plan.dst_label.clone(),
                state,
                exit_code: None,
                tail: self.status_line(),
                stale: false,
                settled: true,
            }],
        }
    }
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

    // ── Copy (#536) ───────────────────────────────────────────────────

    /// Left: local `/src` with a dir and two files; right: target `/dst`.
    fn two_panels() -> FilePanels {
        let mut fp = opened(
            Side::Left,
            "/src",
            vec![
                entry("dir", EntryKind::Dir),
                entry("a.txt", EntryKind::File),
                entry("b.txt", EntryKind::File),
            ],
        );
        fp.set_source(Side::Right, PanelSource::Target(0), None);
        let g = fp.right.generation;
        fp.apply(PanelEvent::Opened {
            side: Side::Right,
            generation: g,
            label: "web1".into(),
            style: PathStyle::Posix,
            read_only: false,
        });
        fp.apply(PanelEvent::Listed {
            side: Side::Right,
            generation: g,
            path: "/dst".into(),
            result: Ok(vec![]),
        });
        fp.requests.clear();
        fp
    }

    #[test]
    fn copy_takes_the_marked_entries_and_asks_first() {
        let mut fp = two_panels();
        fp.handle_key(key(KeyCode::Down)); // dir
        fp.handle_key(key(KeyCode::Char(' '))); // mark dir, cursor -> a.txt
        fp.handle_key(key(KeyCode::Down)); // b.txt
        fp.handle_key(key(KeyCode::Insert)); // mark b.txt
        assert_eq!(fp.left.marked.len(), 2);
        assert_eq!(fp.handle_key(key(KeyCode::F(5))), PanelsAction::None);
        let Some(PanelDialog::ConfirmCopy(plan)) = fp.dialog.clone() else {
            panic!("expected a confirmation");
        };
        assert_eq!((plan.files, plan.dirs, plan.bytes), (1, 1, 10));
        assert_eq!(plan.what(), "1 file (10) and 1 folder");
        assert_eq!(plan.target(), "web1:/dst");
        let paths: Vec<_> = plan.sources.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, vec!["/src/dir", "/src/b.txt"]);
        // Any other key keeps the question open.
        assert_eq!(fp.handle_key(key(KeyCode::Down)), PanelsAction::None);
        assert!(fp.dialog.is_some());
        assert_eq!(fp.handle_key(key(KeyCode::Enter)), PanelsAction::StartCopy(plan));
        assert!(fp.left.marked.is_empty(), "marks are used up");
    }

    #[test]
    fn without_marks_the_entry_under_the_cursor_is_copied_and_esc_cancels() {
        let mut fp = two_panels();
        fp.handle_key(key(KeyCode::Down));
        fp.handle_key(key(KeyCode::Down)); // a.txt
        fp.handle_key(key(KeyCode::Char('с'))); // Russian layout
        let Some(PanelDialog::ConfirmCopy(plan)) = fp.dialog.clone() else {
            panic!("expected a confirmation");
        };
        assert_eq!(plan.sources.len(), 1);
        assert_eq!(plan.sources[0].path, "/src/a.txt");
        assert_eq!(fp.handle_key(key(KeyCode::Esc)), PanelsAction::None);
        assert!(fp.dialog.is_none());
        // Esc closed only the question, not the panels.
        assert_eq!(fp.handle_key(key(KeyCode::Esc)), PanelsAction::Close);
    }

    #[test]
    fn a_copy_that_cannot_happen_says_why() {
        // The cursor on `..`: nothing to copy.
        let mut fp = two_panels();
        fp.handle_key(key(KeyCode::F(5)));
        assert!(fp.dialog.is_none());
        assert!(fp.notice.as_deref().is_some_and(|n| n.contains("Nothing to copy")));

        // Read-only target.
        let mut fp = two_panels();
        fp.right.read_only = true;
        fp.handle_key(key(KeyCode::Down));
        fp.handle_key(key(KeyCode::F(5)));
        assert!(fp.notice.as_deref().is_some_and(|n| n.contains("read-only")));

        // Target panel not chosen yet.
        let mut fp = opened(Side::Left, "/src", vec![entry("a", EntryKind::File)]);
        fp.handle_key(key(KeyCode::Down));
        fp.handle_key(key(KeyCode::F(5)));
        assert!(fp.notice.as_deref().is_some_and(|n| n.contains("Ctrl+O")));
    }

    #[test]
    fn same_folder_and_into_itself_are_refused() {
        let mut fp = two_panels();
        fp.right.source = Some(PanelSource::Local);
        fp.right.path = "/src".into();
        fp.handle_key(key(KeyCode::Down));
        fp.handle_key(key(KeyCode::F(5)));
        assert!(fp.notice.as_deref().is_some_and(|n| n.contains("same folder")));

        let mut fp = two_panels();
        fp.right.source = Some(PanelSource::Local);
        fp.right.path = "/src/dir/deeper".into();
        fp.handle_key(key(KeyCode::Down)); // dir
        fp.handle_key(key(KeyCode::F(5)));
        assert!(fp.notice.as_deref().is_some_and(|n| n.contains("into itself")), "{:?}", fp.notice);
    }

    #[test]
    fn marks_survive_a_refresh_but_not_a_new_folder() {
        let mut fp = two_panels();
        fp.handle_key(key(KeyCode::Down));
        fp.handle_key(key(KeyCode::Down));
        fp.handle_key(key(KeyCode::Char(' '))); // a.txt
        fp.handle_key(key(KeyCode::Char('r')));
        let g = fp.left.generation;
        fp.apply(PanelEvent::Listed {
            side: Side::Left,
            generation: g,
            path: "/src".into(),
            result: Ok(vec![entry("a.txt", EntryKind::File)]),
        });
        assert!(fp.left.marked.contains("a.txt"));
        fp.handle_key(key(KeyCode::Home));
        fp.handle_key(key(KeyCode::Enter)); // ..
        let g = fp.left.generation;
        fp.apply(PanelEvent::Listed {
            side: Side::Left,
            generation: g,
            path: "/".into(),
            result: Ok(vec![entry("src", EntryKind::Dir)]),
        });
        assert!(fp.left.marked.is_empty());
    }

    #[test]
    fn ctrl_z_asks_to_cancel_the_copy() {
        let mut fp = two_panels();
        assert_eq!(fp.handle_key(ctrl('z')), PanelsAction::CancelCopy);
        assert_eq!(fp.handle_key(ctrl('я')), PanelsAction::CancelCopy);
    }

    fn a_plan() -> CopyPlan {
        let mut fp = two_panels();
        fp.handle_key(key(KeyCode::Down));
        fp.handle_key(key(KeyCode::Down));
        fp.copy_plan().expect("plan")
    }

    #[test]
    fn a_copy_tracks_progress_and_sums_up_what_matters() {
        let mut c = FileCopy::new(1, SessionId(1), a_plan());
        c.apply(CopyEvent::Planned { files: 2, bytes: 200 });
        c.apply(CopyEvent::FileStarted {
            src: "/src/a.txt".into(),
            dst: "/dst/a.txt".into(),
            size: 100,
        });
        c.apply(CopyEvent::Progress { done: 50, total: 200 });
        assert_eq!(c.percent(), Some(25));
        assert!(c.status_line().contains("Copying 0/2"), "{}", c.status_line());
        assert!(c.status_line().contains("/dst/a.txt"));
        c.apply(CopyEvent::FileDone { dst: "/dst/a.txt".into() });
        assert_eq!(c.files_done, 1);
        c.finish(CopyReport {
            copied: 1,
            failed: vec![("/src/b.txt".into(), "permission denied".into())],
            leftover_parts: vec!["/dst/b.txt.filar-part".into()],
            ..Default::default()
        });
        assert_eq!(c.state, CopyState::Failed);
        let s = c.summary();
        assert!(s.contains("1 copied") && s.contains("1 failed"), "{s}");
        assert!(s.contains("permission denied"), "{s}");
        assert!(s.contains("/dst/b.txt.filar-part"), "the leftover path must be shown: {s}");
        let op = c.operation();
        assert_eq!(op.id, "copy-1");
        assert_eq!(op.state(), crate::ops::HostOpState::Failed);
    }

    #[test]
    fn a_cancelled_copy_reads_as_cancelled() {
        let mut c = FileCopy::new(2, SessionId(1), a_plan());
        c.finish(CopyReport {
            cancelled: true,
            ..Default::default()
        });
        assert_eq!(c.state, CopyState::Cancelled);
        assert!(c.summary().starts_with("Copy cancelled"));
        assert_eq!(c.operation().state(), crate::ops::HostOpState::Cancelled);
    }

    // ── New folder (#546) ─────────────────────────────────────────────

    #[test]
    fn folder_names_are_checked_for_each_side() {
        let p = PathStyle::Posix;
        let w = PathStyle::Windows;
        assert!(check_folder_name(p, "logs-2026").is_ok());
        assert!(check_folder_name(p, "новая папка").is_ok());
        assert!(check_folder_name(p, "a:b").is_ok(), ": is fine on POSIX");
        for bad in ["", "   ", ".", "..", "a/b", "a\tb"] {
            assert!(check_folder_name(p, bad).is_err(), "{bad:?}");
        }
        assert!(check_folder_name(w, "Reports").is_ok());
        for bad in ["a:b", "a\\b", "a/b", "what?", "x*", "q\"", "<a>", "a|b", "trail.", "trail "] {
            assert!(check_folder_name(w, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn f7_asks_for_a_name_and_enter_creates_it_in_the_active_panel() {
        let mut fp = two_panels();
        fp.handle_key(key(KeyCode::Tab)); // right: web1:/dst
        fp.handle_key(key(KeyCode::F(7)));
        assert!(matches!(fp.dialog, Some(PanelDialog::NewFolder { side: Side::Right, .. })));
        for c in "logs".chars() {
            fp.handle_key(key(KeyCode::Char(c)));
        }
        // Keys go to the name, not the panel.
        assert_eq!(fp.active, Side::Right);
        fp.handle_key(key(KeyCode::Enter));
        assert!(fp.dialog.is_none());
        assert!(matches!(
            fp.requests.pop(),
            Some(PanelRequest::CreateDir { side: Side::Right, ref dir, ref name })
                if dir == "/dst" && name == "logs"
        ));
    }

    #[test]
    fn a_bad_name_keeps_the_dialog_open_with_the_reason() {
        let mut fp = two_panels();
        fp.handle_key(key(KeyCode::Char('ь'))); // `m`, Russian layout
        for c in "a/b".chars() {
            fp.handle_key(key(KeyCode::Char(c)));
        }
        fp.handle_key(key(KeyCode::Enter));
        assert!(fp.requests.is_empty());
        assert!(matches!(
            &fp.dialog,
            Some(PanelDialog::NewFolder { error: Some(e), .. }) if e.contains('/')
        ));
        fp.handle_key(key(KeyCode::Esc));
        assert!(fp.dialog.is_none());
        assert!(fp.requests.is_empty());
        // Esc closed only the dialog.
        assert_eq!(fp.handle_key(key(KeyCode::Esc)), PanelsAction::Close);
    }

    #[test]
    fn no_folder_on_a_read_only_host_or_the_drive_list() {
        let mut fp = two_panels();
        fp.right.read_only = true;
        fp.handle_key(key(KeyCode::Tab));
        fp.handle_key(key(KeyCode::F(7)));
        assert!(fp.dialog.is_none());
        assert!(fp.notice.as_deref().is_some_and(|n| n.contains("read-only")));

        let mut fp = two_panels();
        fp.left.style = PathStyle::Windows;
        fp.left.path = String::new();
        fp.handle_key(key(KeyCode::F(7)));
        assert!(fp.dialog.is_none());
        assert!(fp.notice.as_deref().is_some_and(|n| n.contains("drives")));

        let mut fp = opened(Side::Left, "/src", vec![]);
        fp.handle_key(key(KeyCode::Tab)); // right panel has no host
        fp.handle_key(key(KeyCode::F(7)));
        assert!(fp.notice.as_deref().is_some_and(|n| n.contains("Ctrl+O")));
    }

    #[test]
    fn a_created_folder_is_listed_and_selected_and_a_failure_is_said() {
        let mut fp = two_panels();
        fp.apply(PanelEvent::DirCreated {
            side: Side::Right,
            dir: "/dst".into(),
            name: "logs".into(),
            result: Ok(()),
        });
        assert_eq!(fp.right.focus_after_load.as_deref(), Some("logs"));
        let g = fp.right.generation;
        assert!(matches!(
            fp.requests.pop(),
            Some(PanelRequest::List { side: Side::Right, ref path, generation }) if path == "/dst" && generation == g
        ));
        fp.apply(PanelEvent::Listed {
            side: Side::Right,
            generation: g,
            path: "/dst".into(),
            result: Ok(vec![entry("a", EntryKind::Dir), entry("logs", EntryKind::Dir)]),
        });
        assert!(matches!(fp.right.current(), Some(Row::Entry(e)) if e.name == "logs"));

        fp.apply(PanelEvent::DirCreated {
            side: Side::Right,
            dir: "/dst".into(),
            name: "logs".into(),
            result: Err("already exists".into()),
        });
        assert!(fp.notice.as_deref().is_some_and(|n| n.contains("already exists")));
        assert!(fp.requests.is_empty());
    }
}
