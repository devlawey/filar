//! Native file/folder picker for inserting paths into agent input (#344).
//!
//! Opens an OS dialog while temporarily leaving ratatui raw mode so the
//! picker can display. Local client filesystem only — not remote SSH paths.

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Which native picker to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPickerKind {
    File,
    Folder,
}

/// True when `/` at the cursor would start an absolute-path token (input start or after space).
pub fn path_token_starts_at_cursor(input: &str, cursor_pos: usize) -> bool {
    if cursor_pos == 0 {
        return true;
    }
    input
        .char_indices()
        .nth(cursor_pos.saturating_sub(1))
        .map(|(_, c)| c.is_whitespace())
        .unwrap_or(false)
}

/// Format a filesystem path for insertion into the single-line input field.
pub fn format_path_for_input(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.contains(' ') || s.contains('\t') {
        let escaped = s.replace('\'', "'\\''");
        format!("'{escaped}'")
    } else {
        s.into_owned()
    }
}

/// Blockingly open the native picker (call from `spawn_blocking`).
pub fn pick_path(kind: PathPickerKind) -> Option<PathBuf> {
    let dialog = rfd::FileDialog::new();
    match kind {
        PathPickerKind::File => dialog.pick_file(),
        PathPickerKind::Folder => dialog.pick_folder(),
    }
}

/// Leave TUI mode so a native dialog can appear.
pub fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
    disable_raw_mode().ok();
    execute!(
        io::stdout(),
        DisableBracketedPaste,
        crossterm::event::DisableMouseCapture,
        LeaveAlternateScreen
    )
    .ok();
    terminal.hide_cursor().ok();
}

/// Re-enter TUI mode after the native dialog closes.
pub fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
    enable_raw_mode().ok();
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        EnableBracketedPaste
    )
    .ok();
    terminal.clear().ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_token_start_at_beginning_or_after_space() {
        assert!(path_token_starts_at_cursor("", 0));
        assert!(path_token_starts_at_cursor("cd /tmp", 3));
        assert!(!path_token_starts_at_cursor("/tmp", 1));
        assert!(!path_token_starts_at_cursor("foo/bar", 3));
    }

    #[test]
    fn format_path_quotes_spaces() {
        assert_eq!(format_path_for_input(Path::new("/tmp/a")), "/tmp/a");
        assert_eq!(
            format_path_for_input(Path::new("/tmp/my dir")),
            "'/tmp/my dir'"
        );
    }
}
