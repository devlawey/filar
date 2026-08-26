//! In-TUI file/folder picker for inserting paths into agent input (#344, #351).
//!
//! Lists directories on the active target: local FS via `std::fs`, remote via
//! readonly `ls` through the session executor (zero-install).

use std::path::{Path, PathBuf};

use filar_core::{CoreError, Result};
use filar_transport::posix_shell_quote;

/// Which picker mode to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPickerKind {
    File,
    Folder,
}

/// One row in the picker list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Maximum directory entries shown (extra rows trigger a warning).
pub const MAX_ENTRIES: usize = 500;

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

/// Format a path string for insertion into the single-line input field.
pub fn format_path_for_input(path: &str) -> String {
    if path.contains(' ') || path.contains('\t') {
        let escaped = path.replace('\'', "'\\''");
        format!("'{escaped}'")
    } else {
        path.to_string()
    }
}

/// Join `base` and `name` into an absolute path string.
pub fn join_path(base: &str, name: &str) -> String {
    if name == ".." {
        return parent_path(base).unwrap_or_else(|| base.to_string());
    }
    let base = base.trim_end_matches(['/', '\\']);
    if base.is_empty() {
        if name.starts_with('/') || (cfg!(windows) && name.contains(':')) {
            name.to_string()
        } else if cfg!(windows) {
            format!("{name}")
        } else {
            format!("/{name}")
        }
    } else if cfg!(windows) && base.contains(':') {
        format!("{base}\\{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Parent directory, or `None` at filesystem root.
pub fn parent_path(path: &str) -> Option<String> {
    let p = Path::new(path);
    p.parent().map(|parent| {
        let s = parent.to_string_lossy();
        if s.is_empty() {
            if cfg!(windows) {
                path.chars()
                    .take_while(|c| *c != '\\')
                    .chain(std::iter::once('\\'))
                    .collect()
            } else {
                "/".to_string()
            }
        } else {
            s.into_owned()
        }
    })
}

/// Initial directory when opening the picker for a session tab.
pub fn initial_picker_dir(cwd: &Option<String>, is_remote: bool) -> String {
    if is_remote {
        if let Some(dir) = cwd.as_ref().filter(|d| d.starts_with('/')) {
            return dir.clone();
        }
        return "/".to_string();
    }
    if let Some(dir) = cwd.as_ref().filter(|d| !d.is_empty()) {
        return dir.clone();
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "C:\\".to_string()
            } else {
                "/".to_string()
            }
        })
}

/// Readonly `ls` for remote listing (allowlisted, no writes).
pub fn remote_ls_command(dir: &str) -> String {
    let quoted = posix_shell_quote(dir);
    format!("ls -1Ap {quoted} 2>/dev/null | head -n {MAX_ENTRIES}")
}

/// Parse `ls -1Ap` lines into entries (directories have trailing `/`).
pub fn parse_ls_output(output: &str) -> Vec<PathEntry> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let line = line.trim_end();
        if line.is_empty() || line == "." {
            continue;
        }
        if line == ".." {
            continue;
        }
        if line.ends_with('/') {
            entries.push(PathEntry {
                name: line.trim_end_matches('/').to_string(),
                is_dir: true,
            });
        } else {
            entries.push(PathEntry {
                name: line.to_string(),
                is_dir: false,
            });
        }
    }
    sort_entries(&mut entries);
    entries
}

/// List a local directory. Returns `(entries, truncated)`.
pub fn list_local_dir(dir: &str) -> Result<(Vec<PathEntry>, bool)> {
    let path = PathBuf::from(dir);
    let read_dir = std::fs::read_dir(&path).map_err(|e| CoreError::Other(e.to_string()))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in read_dir {
        let entry = entry.map_err(|e| CoreError::Other(e.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|e| CoreError::Other(e.to_string()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "." {
            continue;
        }
        entries.push(PathEntry {
            name,
            is_dir: file_type.is_dir(),
        });
        if entries.len() > MAX_ENTRIES {
            truncated = true;
            entries.truncate(MAX_ENTRIES);
            break;
        }
    }
    sort_entries(&mut entries);
    Ok((entries, truncated))
}

/// Build display list with optional `..` parent row at the top.
pub fn entries_with_parent(dir: &str, mut entries: Vec<PathEntry>) -> Vec<PathEntry> {
    if parent_path(dir).is_some() {
        entries.insert(
            0,
            PathEntry {
                name: "..".to_string(),
                is_dir: true,
            },
        );
    }
    entries
}

fn sort_entries(entries: &mut [PathEntry]) {
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
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
        assert_eq!(format_path_for_input("/tmp/a"), "/tmp/a");
        assert_eq!(format_path_for_input("/tmp/my dir"), "'/tmp/my dir'");
    }

    #[test]
    fn parse_ls_marks_directories() {
        let out = "etc/\nhosts\nnginx/\n";
        let entries = parse_ls_output(out);
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().any(|e| e.name == "etc" && e.is_dir));
        assert!(entries.iter().any(|e| e.name == "hosts" && !e.is_dir));
    }

    #[test]
    fn join_and_parent_posix() {
        assert_eq!(join_path("/etc", "nginx"), "/etc/nginx");
        assert_eq!(parent_path("/etc/nginx").as_deref(), Some("/etc"));
        assert_eq!(parent_path("/").as_deref(), None);
    }

    #[test]
    fn entries_with_parent_inserts_dotdot() {
        let entries = entries_with_parent("/etc", vec![]);
        assert_eq!(entries[0].name, "..");
    }
}
