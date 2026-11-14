//! Runbooks for replay, listed from the local export folder (#668).
//!
//! Ctrl+S writes `{stem}.runbook.md` next to the session export: into the
//! export folder (`save_dir`, else the working directory) and its per-target
//! subfolders (#400, #401). The path picker's `Tab` lists them from there,
//! always from the **local** disk through `std::fs` — whatever host the tab
//! is on, nothing is read from or sent to it.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use filar_agent::MAX_REPLAY_RUNBOOK_BYTES;

/// Suffix of a runbook file.
pub const RUNBOOK_SUFFIX: &str = ".runbook.md";

/// How much of a runbook is read for its list preview line.
const PREVIEW_READ_BYTES: u64 = 16 * 1024;

/// One runbook in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunbookEntry {
    /// Path relative to the export folder: `folder/name.runbook.md`, or just
    /// the name for one in the folder itself.
    pub display: String,
    /// Where it is on disk.
    pub path: PathBuf,
    /// Last modification, when the file system reports it.
    pub modified: Option<SystemTime>,
    /// First line of the "Symptom / when to use" section, or the first line
    /// of text when there is no such section; may be empty.
    pub preview: String,
}

/// Runbooks in `base` and its subfolders one level down, newest first.
///
/// At most [`crate::path_picker::MAX_ENTRIES`] are returned; the flag says
/// whether more were found. A missing folder is an empty list (nothing was
/// exported there yet); an unreadable one is an error. Unreadable
/// subfolders and files are skipped.
pub fn list_runbooks(base: &Path) -> Result<(Vec<RunbookEntry>, bool), String> {
    let top = match std::fs::read_dir(base) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
        Err(e) => return Err(format!("cannot read {}: {e}", base.display())),
    };
    let mut found = Vec::new();
    for entry in top.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let Ok(sub) = std::fs::read_dir(&path) else {
                continue;
            };
            for inner in sub.flatten() {
                let file = inner.file_name().to_string_lossy().into_owned();
                if is_runbook_name(&file) && inner.path().is_file() {
                    found.push(entry_for(inner.path(), format!("{name}/{file}")));
                }
            }
        } else if is_runbook_name(&name) && path.is_file() {
            found.push(entry_for(path, name));
        }
    }
    found.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.display.cmp(&b.display))
    });
    let truncated = found.len() > crate::path_picker::MAX_ENTRIES;
    found.truncate(crate::path_picker::MAX_ENTRIES);
    Ok((found, truncated))
}

/// Read a runbook for replay: at most [`MAX_REPLAY_RUNBOOK_BYTES`], UTF-8.
pub fn read_runbook(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read the runbook: {e}"))?;
    if meta.len() > MAX_REPLAY_RUNBOOK_BYTES as u64 {
        return Err(format!(
            "the runbook is too large to replay ({} KiB, at most {} KiB)",
            meta.len().div_ceil(1024),
            MAX_REPLAY_RUNBOOK_BYTES / 1024
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read the runbook: {e}"))?;
    String::from_utf8(bytes).map_err(|_| "the runbook is not UTF-8 text".to_string())
}

/// The list's preview line for a runbook's text.
///
/// The first line under a heading that names the symptom (`Symptom`, `When
/// to use`, `Симптом`, `Когда`), list markers and emphasis stripped. Without
/// such a section, the first line of text that is not a heading.
pub fn symptom_line(text: &str) -> String {
    let mut in_symptom = false;
    let mut first_text = None;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("<!--") {
            continue;
        }
        if t.starts_with('#') {
            let heading = t.trim_start_matches('#').trim().to_lowercase();
            in_symptom = ["symptom", "when to use", "симптом", "когда"]
                .iter()
                .any(|w| heading.contains(w));
            continue;
        }
        let clean = clean_line(t);
        if clean.is_empty() {
            continue;
        }
        if in_symptom {
            return clean;
        }
        if first_text.is_none() {
            first_text = Some(clean);
        }
    }
    first_text.unwrap_or_default()
}

/// `YYYY-MM-DD HH:MM` in local time, or empty when unknown.
pub fn format_modified(modified: Option<SystemTime>) -> String {
    modified
        .map(|t| {
            chrono::DateTime::<chrono::Local>::from(t)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn is_runbook_name(name: &str) -> bool {
    name.ends_with(RUNBOOK_SUFFIX) && name.len() > RUNBOOK_SUFFIX.len()
}

fn entry_for(path: PathBuf, display: String) -> RunbookEntry {
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let preview = read_head(&path)
        .map(|t| symptom_line(&t))
        .unwrap_or_default();
    RunbookEntry {
        display,
        path,
        modified,
        preview,
    }
}

/// The first [`PREVIEW_READ_BYTES`] of a file as text, a cut character
/// dropped.
fn read_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(PREVIEW_READ_BYTES).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A line without list markers, emphasis and control characters.
fn clean_line(line: &str) -> String {
    let mut t = line.trim();
    for marker in ["- ", "* ", "+ ", "> "] {
        if let Some(rest) = t.strip_prefix(marker) {
            t = rest.trim_start();
        }
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && t[digits..].starts_with(". ") {
        t = t[digits + 2..].trim_start();
    }
    t.chars()
        .filter(|c| !c.is_control() && *c != '*' && *c != '`')
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "filar_runbook_picker_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    fn touch_at(path: &Path, secs: u64) {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn runbooks_are_found_in_the_folder_and_one_level_down_only() {
        let dir = tempdir();
        let base = dir.path();
        write(&base.join("top.runbook.md"), "# T\n");
        write(&base.join("web-1/2026-11-13_a.runbook.md"), "# A\n");
        write(&base.join("web-1/2026-11-13_a.md"), "export, not a runbook");
        write(&base.join("local/notes.txt"), "x");
        write(&base.join("deep/nested/x.runbook.md"), "too deep");
        let (list, truncated) = list_runbooks(base).unwrap();
        let names: Vec<&str> = list.iter().map(|e| e.display.as_str()).collect();
        assert!(!truncated);
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"top.runbook.md"));
        assert!(names.contains(&"web-1/2026-11-13_a.runbook.md"));
    }

    #[test]
    fn newest_runbooks_come_first() {
        let dir = tempdir();
        let base = dir.path();
        for (name, secs) in [
            ("a/old.runbook.md", 1_000),
            ("b/new.runbook.md", 3_000),
            ("mid.runbook.md", 2_000),
        ] {
            write(&base.join(name), "# x\n");
            touch_at(&base.join(name), secs);
        }
        let (list, _) = list_runbooks(base).unwrap();
        let names: Vec<&str> = list.iter().map(|e| e.display.as_str()).collect();
        assert_eq!(
            names,
            ["b/new.runbook.md", "mid.runbook.md", "a/old.runbook.md"]
        );
    }

    #[test]
    fn a_missing_folder_is_an_empty_list() {
        let dir = tempdir();
        let (list, truncated) = list_runbooks(&dir.path().join("nope")).unwrap();
        assert!(list.is_empty() && !truncated);
    }

    #[test]
    fn the_preview_is_the_first_symptom_line() {
        let text = "<!-- filar:fleet-runbook -->\n# Disk full\n\nIntro.\n\n## 1. Symptom / when to use\n\n- **Writes fail** with `ENOSPC`.\n- more\n\n## Steps\n1. df\n";
        assert_eq!(symptom_line(text), "Writes fail with ENOSPC.");
        let ru = "# Диск\n## Симптом\n1. Запись падает\n";
        assert_eq!(symptom_line(ru), "Запись падает");
        assert_eq!(
            symptom_line("# Title\n\nJust text here.\n## Steps\n"),
            "Just text here."
        );
        assert_eq!(symptom_line(""), "");
    }

    #[test]
    fn the_list_carries_the_preview_line() {
        let dir = tempdir();
        write(
            &dir.path().join("h/x.runbook.md"),
            "# T\n## Symptom\nNginx returns 502.\n",
        );
        let (list, _) = list_runbooks(dir.path()).unwrap();
        assert_eq!(list[0].preview, "Nginx returns 502.");
    }

    #[test]
    fn reading_refuses_oversized_and_non_utf8_files() {
        let dir = tempdir();
        let big = dir.path().join("big.runbook.md");
        std::fs::write(&big, vec![b'x'; MAX_REPLAY_RUNBOOK_BYTES + 1]).unwrap();
        assert!(read_runbook(&big).unwrap_err().contains("too large"));
        let bin = dir.path().join("bin.runbook.md");
        std::fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();
        assert!(read_runbook(&bin).unwrap_err().contains("UTF-8"));
        let ok = dir.path().join("ok.runbook.md");
        std::fs::write(&ok, "# ok").unwrap();
        assert_eq!(read_runbook(&ok).unwrap(), "# ok");
        assert!(read_runbook(&dir.path().join("gone.runbook.md")).is_err());
    }
}
