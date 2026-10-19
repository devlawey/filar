//! Copy engine over two [`FileTransfer`] sides (#534).
//!
//! One code path for local→host, host→local and host→host: bytes are streamed
//! from the source reader into the destination writer through the client's
//! memory, never staged on any disk. The client is the only intermediary
//! (AGENTS.md, invariant 5).
//!
//! Every file is written to `<name>.filar-part` next to its target and renamed
//! over the target only once it is complete, so an existing file is never
//! half-overwritten. On cancel or error the part file is removed; if removing
//! it fails (lost connection), its exact path is reported back
//! (AGENTS.md, invariant 1, #531 exception).

use std::sync::atomic::{AtomicU64, Ordering};

use filar_core::{CoreError, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::file_transfer::{EntryKind, FileTransfer, Metadata, TransferReader, TransferWriter};

/// Suffix of the temporary file a copy writes before renaming it over the target.
pub const PART_SUFFIX: &str = ".filar-part";

/// Read/write chunk size.
const CHUNK: usize = 64 * 1024;

/// One object the user picked for copying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopySource {
    /// Full path on the source side.
    pub path: String,
    /// Kind as the listing reported it (symlinks not followed).
    pub kind: EntryKind,
}

/// What to do with an existing target file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteDecision {
    /// Replace this file.
    Overwrite,
    /// Keep the existing file, skip this one.
    Skip,
    /// Replace this and every later existing file without asking.
    OverwriteAll,
    /// Skip this and every later existing file without asking.
    SkipAll,
    /// Stop the whole copy.
    Cancel,
}

/// Progress of a copy, reported through [`CopyObserver::on_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyEvent {
    /// The sources were walked: totals the progress is measured against.
    Planned {
        /// Number of files to copy.
        files: u64,
        /// Total size of those files.
        bytes: u64,
    },
    /// A file started copying.
    FileStarted {
        /// Source path.
        src: String,
        /// Target path.
        dst: String,
        /// File size.
        size: u64,
    },
    /// Bytes copied so far across all files.
    Progress {
        /// Bytes done.
        done: u64,
        /// Bytes planned.
        total: u64,
    },
    /// A file was copied.
    FileDone {
        /// Target path.
        dst: String,
    },
    /// A file was skipped (existing target kept).
    FileSkipped {
        /// Target path.
        dst: String,
    },
    /// A file or directory could not be copied; the copy goes on.
    Failed {
        /// Path the failure is about.
        path: String,
        /// What went wrong.
        error: String,
    },
    /// Something left out on purpose (e.g. a symlink to a directory).
    Warning(String),
}

/// Receives progress and answers the overwrite question. The UI implements it.
#[async_trait::async_trait]
pub trait CopyObserver: Send + Sync {
    /// A progress event.
    async fn on_event(&self, event: CopyEvent);

    /// The target `dst` exists: what to do. Not called again after an
    /// `…All` answer.
    async fn decide_overwrite(&self, dst: &str) -> OverwriteDecision;
}

/// Outcome of [`copy`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyReport {
    /// Files copied.
    pub copied: u64,
    /// Files skipped because the target existed.
    pub skipped: u64,
    /// Bytes written to targets.
    pub bytes: u64,
    /// Failures as `(path, reason)`.
    pub failed: Vec<(String, String)>,
    /// Warnings (objects left out on purpose).
    pub warnings: Vec<String>,
    /// `true` when the copy stopped on cancel (token or [`OverwriteDecision::Cancel`]).
    pub cancelled: bool,
    /// Part files that could not be removed after a failure — the user must
    /// see these paths.
    pub leftover_parts: Vec<String>,
}

/// One planned step.
#[derive(Debug, Clone)]
enum Step {
    /// Create (or reuse) a directory on the target side.
    Dir { src: String, dst: String },
    /// Copy one file.
    File {
        src: String,
        dst: String,
        meta: Metadata,
    },
}

impl Step {
    fn src(&self) -> &str {
        match self {
            Step::Dir { src, .. } | Step::File { src, .. } => src,
        }
    }
}

/// Copy `sources` from `src` into the directory `dst_dir` on `dst`.
///
/// Directories are copied recursively. A symlink to a file is copied as the
/// file's content; a symlink to a directory is skipped with a warning (no
/// loops). Permission bits travel along, owners do not. An existing target
/// file is replaced only on the observer's decision. A failure of one file
/// does not stop the others. `cancel` stops the copy at the next chunk; the
/// file in flight is then removed.
pub async fn copy(
    src: &dyn FileTransfer,
    sources: &[CopySource],
    dst: &dyn FileTransfer,
    dst_dir: &str,
    observer: &dyn CopyObserver,
    cancel: &CancellationToken,
) -> CopyReport {
    let mut report = CopyReport::default();

    // ── Plan ───────────────────────────────────────────────────────────
    let mut steps = Vec::new();
    for s in sources {
        if cancel.is_cancelled() {
            report.cancelled = true;
            return report;
        }
        let name = src.path_style().file_name(&s.path);
        let target = dst.path_style().join(dst_dir, &name);
        plan(
            src,
            dst,
            &s.path,
            s.kind,
            &target,
            &mut steps,
            &mut report,
            observer,
            cancel,
        )
        .await;
    }
    let total_bytes: u64 = steps
        .iter()
        .map(|s| match s {
            Step::File { meta, .. } => meta.size,
            Step::Dir { .. } => 0,
        })
        .sum();
    let total_files = steps
        .iter()
        .filter(|s| matches!(s, Step::File { .. }))
        .count() as u64;
    observer
        .on_event(CopyEvent::Planned {
            files: total_files,
            bytes: total_bytes,
        })
        .await;

    // ── Execute ────────────────────────────────────────────────────────
    let done = AtomicU64::new(0);
    let mut sticky: Option<OverwriteDecision> = None;
    // Source directories whose target could not be made: their content is
    // skipped (already reported once).
    let mut failed_dirs: Vec<String> = Vec::new();

    for step in &steps {
        if cancel.is_cancelled() {
            report.cancelled = true;
            break;
        }
        if failed_dirs.iter().any(|d| is_inside(src, step.src(), d)) {
            // Cut off with its directory (reported once): its bytes still
            // count, so the bar reaches the end (#541 review).
            if let Step::File { meta, .. } = step {
                advance(&done, meta.size, total_bytes, observer).await;
            }
            continue;
        }
        match step {
            Step::Dir { src: s, dst: d } => {
                if let Err(e) = ensure_dir(dst, d).await {
                    fail(&mut report, observer, d, e.to_string()).await;
                    failed_dirs.push(s.clone());
                }
            }
            Step::File {
                src: s,
                dst: d,
                meta,
            } => {
                // Existing target?
                match dst.stat(d).await {
                    Ok(Some(existing)) if existing.kind == EntryKind::Dir => {
                        fail(
                            &mut report,
                            observer,
                            d,
                            "a directory with this name exists".into(),
                        )
                        .await;
                        advance(&done, meta.size, total_bytes, observer).await;
                        continue;
                    }
                    Ok(Some(_)) => {
                        let decision = match sticky {
                            Some(OverwriteDecision::OverwriteAll) => OverwriteDecision::Overwrite,
                            Some(OverwriteDecision::SkipAll) => OverwriteDecision::Skip,
                            _ => {
                                tokio::select! {
                                    d = observer.decide_overwrite(d) => d,
                                    _ = cancel.cancelled() => OverwriteDecision::Cancel,
                                }
                            }
                        };
                        match decision {
                            OverwriteDecision::Overwrite => {}
                            OverwriteDecision::OverwriteAll => {
                                sticky = Some(OverwriteDecision::OverwriteAll)
                            }
                            OverwriteDecision::Skip | OverwriteDecision::SkipAll => {
                                if decision == OverwriteDecision::SkipAll {
                                    sticky = Some(OverwriteDecision::SkipAll);
                                }
                                report.skipped += 1;
                                advance(&done, meta.size, total_bytes, observer).await;
                                observer
                                    .on_event(CopyEvent::FileSkipped { dst: d.clone() })
                                    .await;
                                continue;
                            }
                            OverwriteDecision::Cancel => {
                                report.cancelled = true;
                                break;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        fail(&mut report, observer, d, e.to_string()).await;
                        advance(&done, meta.size, total_bytes, observer).await;
                        continue;
                    }
                }

                observer
                    .on_event(CopyEvent::FileStarted {
                        src: s.clone(),
                        dst: d.clone(),
                        size: meta.size,
                    })
                    .await;
                let before = done.load(Ordering::Relaxed);
                match copy_file(src, s, dst, d, meta, observer, cancel, &done, total_bytes).await {
                    Ok(()) => {
                        report.copied += 1;
                        report.bytes += done.load(Ordering::Relaxed) - before;
                        observer
                            .on_event(CopyEvent::FileDone { dst: d.clone() })
                            .await;
                    }
                    Err(FileError { error, leftover }) => {
                        if let Some(p) = leftover {
                            report.leftover_parts.push(p);
                        }
                        if cancel.is_cancelled() {
                            report.cancelled = true;
                            break;
                        }
                        // Keep the bar consistent: the failed file's bytes count as done.
                        done.store(before + meta.size, Ordering::Relaxed);
                        observer
                            .on_event(CopyEvent::Progress {
                                done: before + meta.size,
                                total: total_bytes,
                            })
                            .await;
                        fail(&mut report, observer, s, error).await;
                    }
                }
            }
        }
    }
    // Whatever the target side had to leave behind (e.g. a replaced file it
    // could not remove) goes to the user with the part files (#531).
    report.leftover_parts.extend(dst.take_leftovers());
    report
}

/// Walk one source into steps.
#[allow(clippy::too_many_arguments)]
async fn plan(
    src: &dyn FileTransfer,
    dst: &dyn FileTransfer,
    path: &str,
    kind: EntryKind,
    target: &str,
    steps: &mut Vec<Step>,
    report: &mut CopyReport,
    observer: &dyn CopyObserver,
    cancel: &CancellationToken,
) {
    // Iterative walk: (source path, its kind, target path).
    let mut stack = vec![(path.to_string(), kind, target.to_string())];
    while let Some((p, k, t)) = stack.pop() {
        // A large tree is walked before anything is copied: stop walking on
        // cancel, not only when copying starts (#540 review).
        if cancel.is_cancelled() {
            return;
        }
        let resolved = match k {
            EntryKind::Symlink | EntryKind::File | EntryKind::Other => match src.stat(&p).await {
                Ok(Some(md)) => md,
                Ok(None) => {
                    let why = if k == EntryKind::Symlink {
                        "broken symlink"
                    } else {
                        "no longer exists"
                    };
                    warn(report, observer, format!("{p}: {why}, skipped")).await;
                    continue;
                }
                Err(e) => {
                    fail(report, observer, &p, e.to_string()).await;
                    continue;
                }
            },
            EntryKind::Dir => Metadata {
                kind: EntryKind::Dir,
                size: 0,
                modified: None,
                mode: None,
            },
        };
        match (k, resolved.kind) {
            (EntryKind::Symlink, EntryKind::Dir) => {
                warn(
                    report,
                    observer,
                    format!("{p}: symlink to a directory, skipped"),
                )
                .await;
            }
            (_, EntryKind::File) => steps.push(Step::File {
                src: p,
                dst: t,
                meta: resolved,
            }),
            (_, EntryKind::Dir) => {
                let entries = match src.list_dir(&p).await {
                    Ok(e) => e,
                    Err(e) => {
                        fail(report, observer, &p, e.to_string()).await;
                        continue;
                    }
                };
                steps.push(Step::Dir {
                    src: p.clone(),
                    dst: t.clone(),
                });
                // Reverse so the stack yields entries in name order.
                let mut entries = entries;
                entries.sort_by(|a, b| b.name.cmp(&a.name));
                for e in entries {
                    stack.push((
                        src.path_style().join(&p, &e.name),
                        e.metadata.kind,
                        dst.path_style().join(&t, &e.name),
                    ));
                }
            }
            _ => {
                warn(
                    report,
                    observer,
                    format!("{p}: not a regular file, skipped"),
                )
                .await;
            }
        }
    }
}

/// `path` is `dir` itself or lies below it (source side conventions).
fn is_inside(src: &dyn FileTransfer, path: &str, dir: &str) -> bool {
    let mut cur = Some(path.to_string());
    while let Some(c) = cur {
        if c == dir {
            return true;
        }
        cur = src.path_style().parent(&c);
    }
    false
}

async fn ensure_dir(dst: &dyn FileTransfer, path: &str) -> Result<()> {
    match dst.stat(path).await? {
        Some(md) if md.kind == EntryKind::Dir => Ok(()),
        Some(_) => Err(CoreError::Other(format!(
            "{path}: a file with this name exists"
        ))),
        None => dst.create_dir(path).await,
    }
}

/// Count `size` bytes as done without copying them (skipped or failed) and
/// report the new position.
async fn advance(done: &AtomicU64, size: u64, total: u64, observer: &dyn CopyObserver) {
    let now = done.fetch_add(size, Ordering::Relaxed) + size;
    observer
        .on_event(CopyEvent::Progress { done: now, total })
        .await;
}

/// `Some(path)` when `path` still exists (or its existence cannot be ruled
/// out): a part the user must be told about.
async fn still_there(dst: &dyn FileTransfer, path: &str) -> Option<String> {
    match dst.stat(path).await {
        Ok(None) => None,
        Err(e) if is_not_found(&e) => None,
        _ => Some(path.to_string()),
    }
}

fn is_not_found(e: &CoreError) -> bool {
    matches!(e, CoreError::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
}

async fn fail(report: &mut CopyReport, observer: &dyn CopyObserver, path: &str, error: String) {
    observer
        .on_event(CopyEvent::Failed {
            path: path.to_string(),
            error: error.clone(),
        })
        .await;
    report.failed.push((path.to_string(), error));
}

async fn warn(report: &mut CopyReport, observer: &dyn CopyObserver, msg: String) {
    observer.on_event(CopyEvent::Warning(msg.clone())).await;
    report.warnings.push(msg);
}

/// Failure of one file: the reason, and the part file left behind if it could
/// not be removed.
struct FileError {
    error: String,
    leftover: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn copy_file(
    src: &dyn FileTransfer,
    src_path: &str,
    dst: &dyn FileTransfer,
    dst_path: &str,
    meta: &Metadata,
    observer: &dyn CopyObserver,
    cancel: &CancellationToken,
    done: &AtomicU64,
    total: u64,
) -> std::result::Result<(), FileError> {
    let part = format!("{dst_path}{PART_SUFFIX}");
    let plain = |error: String| FileError {
        error,
        leftover: None,
    };

    // Whatever sits under the part's name — a part left by an earlier crash,
    // or a symlink planted there — is removed first (removal never follows
    // a link), and the part is then created exclusively: the bytes can only
    // land in a fresh file of our own (#541 review).
    match dst.remove_file(&part).await {
        Ok(()) => {}
        Err(e) if is_not_found(&e) => {}
        // It stays where it is: the user must see its path (#531).
        Err(e) => {
            let leftover = still_there(dst, &part).await;
            return Err(FileError {
                error: e.to_string(),
                leftover,
            });
        }
    }
    let mut reader = src
        .open_read(src_path)
        .await
        .map_err(|e| plain(e.to_string()))?;
    let writer = dst
        .open_write(&part)
        .await
        .map_err(|e| plain(e.to_string()))?;

    let result: std::result::Result<(), String> = async {
        // The writer is consumed: its handle is closed before the rename,
        // which Windows and some SFTP servers need.
        stream_into(
            &mut reader,
            writer,
            &part,
            src_path,
            observer,
            cancel,
            done,
            total,
        )
        .await?;
        if let Some(mode) = meta.mode {
            // Best effort: a side without POSIX modes accepts and ignores it,
            // a refusal is not worth losing the copied file over.
            if let Err(e) = dst.set_permissions(&part, mode).await {
                tracing::warn!(path = %part, error = %e, "could not set permissions");
            }
        }
        dst.rename(&part, dst_path).await.map_err(|e| e.to_string())
    }
    .await;

    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let leftover = match dst.remove_file(&part).await {
                Ok(()) => None,
                // Whatever the removal said, only a look at the part itself
                // decides whether something is left behind.
                Err(_) => still_there(dst, &part).await,
            };
            Err(FileError { error, leftover })
        }
    }
}

/// Copy `reader` into `writer` chunk by chunk, then flush and close the
/// writer. The writer is dropped on every return, so its handle is closed.
#[allow(clippy::too_many_arguments)]
async fn stream_into(
    reader: &mut TransferReader,
    mut writer: TransferWriter,
    part: &str,
    src_path: &str,
    observer: &dyn CopyObserver,
    cancel: &CancellationToken,
    done: &AtomicU64,
    total: u64,
) -> std::result::Result<(), String> {
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = tokio::select! {
            r = reader.read(&mut buf) => r.map_err(|e| format!("read {src_path}: {e}"))?,
            _ = cancel.cancelled() => return Err("cancelled".to_string()),
        };
        if n == 0 {
            break;
        }
        tokio::select! {
            r = writer.write_all(&buf[..n]) => r.map_err(|e| format!("write {part}: {e}"))?,
            _ = cancel.cancelled() => return Err("cancelled".to_string()),
        }
        let now = done.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        observer
            .on_event(CopyEvent::Progress { done: now, total })
            .await;
    }
    writer
        .shutdown()
        .await
        .map_err(|e| format!("close {part}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transfer::{
        DirEntry, LocalFileTransfer, PathStyle, TransferReader, TransferWriter,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("filar-copy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    /// Records events, answers overwrites from a script.
    struct Recorder {
        events: Mutex<Vec<CopyEvent>>,
        answers: Mutex<Vec<OverwriteDecision>>,
        asked: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn new(answers: Vec<OverwriteDecision>) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                answers: Mutex::new(answers),
                asked: Mutex::new(Vec::new()),
            }
        }
        fn asked(&self) -> usize {
            self.asked.lock().expect("lock").len()
        }
    }

    #[async_trait::async_trait]
    impl CopyObserver for Recorder {
        async fn on_event(&self, event: CopyEvent) {
            self.events.lock().expect("lock").push(event);
        }
        async fn decide_overwrite(&self, dst: &str) -> OverwriteDecision {
            self.asked.lock().expect("lock").push(dst.to_string());
            let mut a = self.answers.lock().expect("lock");
            if a.is_empty() {
                OverwriteDecision::Cancel
            } else {
                a.remove(0)
            }
        }
    }

    fn file_src(p: &Path) -> CopySource {
        CopySource {
            path: s(p),
            kind: EntryKind::File,
        }
    }

    fn no_parts(dir: &Path) -> bool {
        walk(dir).iter().all(|p| !s(p).ends_with(PART_SUFFIX))
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).expect("read_dir") {
            let p = e.expect("entry").path();
            if p.is_dir() {
                out.extend(walk(&p));
            }
            out.push(p);
        }
        out
    }

    #[tokio::test]
    async fn copies_a_file_and_a_nested_directory() {
        let (a, b) = (temp_dir(), temp_dir());
        std::fs::write(a.join("one.txt"), "1").expect("write");
        std::fs::create_dir_all(a.join("tree/sub")).expect("mkdir");
        std::fs::write(a.join("tree/x"), "xx").expect("write");
        std::fs::write(a.join("tree/sub/y"), "yyy").expect("write");

        let ft = LocalFileTransfer::new();
        let obs = Recorder::new(vec![]);
        let sources = vec![
            file_src(&a.join("one.txt")),
            CopySource {
                path: s(&a.join("tree")),
                kind: EntryKind::Dir,
            },
        ];
        let report = copy(&ft, &sources, &ft, &s(&b), &obs, &CancellationToken::new()).await;

        assert_eq!(report.copied, 3, "{report:?}");
        assert_eq!(report.bytes, 6);
        assert!(report.failed.is_empty() && !report.cancelled);
        assert_eq!(
            std::fs::read_to_string(b.join("one.txt")).expect("read"),
            "1"
        );
        assert_eq!(
            std::fs::read_to_string(b.join("tree/sub/y")).expect("read"),
            "yyy"
        );
        assert!(no_parts(&b));
        let events = obs.events.lock().expect("lock");
        assert_eq!(events[0], CopyEvent::Planned { files: 3, bytes: 6 });
        assert!(events.contains(&CopyEvent::Progress { done: 6, total: 6 }));
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[tokio::test]
    async fn overwrite_decisions_are_honoured() {
        let (a, b) = (temp_dir(), temp_dir());
        for n in ["f1", "f2", "f3", "f4"] {
            std::fs::write(a.join(n), "new").expect("write");
            std::fs::write(b.join(n), "old").expect("write");
        }
        let ft = LocalFileTransfer::new();
        let sources: Vec<_> = ["f1", "f2", "f3", "f4"]
            .iter()
            .map(|n| file_src(&a.join(n)))
            .collect();
        let read = |n: &str| std::fs::read_to_string(b.join(n)).expect("read");

        // Overwrite, Skip, then OverwriteAll covers f3 and f4 with one question.
        let obs = Recorder::new(vec![
            OverwriteDecision::Overwrite,
            OverwriteDecision::Skip,
            OverwriteDecision::OverwriteAll,
        ]);
        let r = copy(&ft, &sources, &ft, &s(&b), &obs, &CancellationToken::new()).await;
        assert_eq!((r.copied, r.skipped), (3, 1));
        assert_eq!(obs.asked(), 3);
        assert_eq!(
            (read("f1"), read("f2"), read("f3"), read("f4")),
            ("new".into(), "old".into(), "new".into(), "new".into())
        );

        // Skipped files still move the bar to the end (#541 review).
        let last = obs
            .events
            .lock()
            .expect("lock")
            .iter()
            .rev()
            .find_map(|e| match e {
                CopyEvent::Progress { done, total } => Some((*done, *total)),
                _ => None,
            });
        assert_eq!(last, Some((12, 12)));

        // SkipAll: one question, nothing replaced.
        for n in ["f1", "f2", "f3", "f4"] {
            std::fs::write(b.join(n), "old").expect("write");
        }
        let obs = Recorder::new(vec![OverwriteDecision::SkipAll]);
        let r = copy(&ft, &sources, &ft, &s(&b), &obs, &CancellationToken::new()).await;
        assert_eq!((r.copied, r.skipped, obs.asked()), (0, 4, 1));
        assert_eq!(read("f4"), "old");

        // Cancel stops at the first existing file.
        let obs = Recorder::new(vec![OverwriteDecision::Cancel]);
        let r = copy(&ft, &sources, &ft, &s(&b), &obs, &CancellationToken::new()).await;
        assert!(r.cancelled);
        assert_eq!((r.copied, r.skipped), (0, 0));
        assert!(no_parts(&b));
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[tokio::test]
    async fn a_failed_file_does_not_stop_the_rest() {
        let (a, b) = (temp_dir(), temp_dir());
        std::fs::write(a.join("ok"), "ok").expect("write");
        let ft = LocalFileTransfer::new();
        let sources = vec![file_src(&a.join("missing")), file_src(&a.join("ok"))];
        let obs = Recorder::new(vec![]);
        let r = copy(&ft, &sources, &ft, &s(&b), &obs, &CancellationToken::new()).await;
        // A vanished source is a warning, the other file is still copied.
        assert_eq!(r.copied, 1);
        assert_eq!(r.warnings.len(), 1, "{r:?}");
        assert!(r.warnings[0].contains("no longer exists"), "{r:?}");
        assert!(b.join("ok").exists());
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permissions_travel_and_symlinked_dirs_are_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let (a, b) = (temp_dir(), temp_dir());
        std::fs::create_dir(a.join("d")).expect("mkdir");
        std::fs::write(a.join("d/run.sh"), "#!/bin/sh").expect("write");
        std::fs::set_permissions(a.join("d/run.sh"), std::fs::Permissions::from_mode(0o750))
            .expect("chmod");
        std::os::unix::fs::symlink(a.join("d/run.sh"), a.join("d/file-link")).expect("symlink");
        std::os::unix::fs::symlink(&a, a.join("d/loop")).expect("symlink");

        let ft = LocalFileTransfer::new();
        let obs = Recorder::new(vec![]);
        let sources = vec![CopySource {
            path: s(&a.join("d")),
            kind: EntryKind::Dir,
        }];
        let r = copy(&ft, &sources, &ft, &s(&b), &obs, &CancellationToken::new()).await;
        assert_eq!(r.copied, 2, "{r:?}");
        assert_eq!(r.warnings.len(), 1);
        let mode = std::fs::metadata(b.join("d/run.sh"))
            .expect("md")
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o750);
        // The file symlink became a regular file with the content.
        assert!(!std::fs::symlink_metadata(b.join("d/file-link"))
            .expect("md")
            .file_type()
            .is_symlink());
        assert!(!b.join("d/loop").exists());
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_planted_as_the_part_is_never_written_through() {
        let (a, b, elsewhere) = (temp_dir(), temp_dir(), temp_dir());
        std::fs::write(a.join("f"), "payload").expect("write");
        let victim = elsewhere.join("victim");
        std::fs::write(&victim, "precious").expect("write");
        std::os::unix::fs::symlink(&victim, b.join(format!("f{PART_SUFFIX}"))).expect("symlink");
        let ft = LocalFileTransfer::new();
        let obs = Recorder::new(vec![]);
        let r = copy(
            &ft,
            &[file_src(&a.join("f"))],
            &ft,
            &s(&b),
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(r.copied, 1, "{r:?}");
        assert_eq!(std::fs::read_to_string(&victim).expect("read"), "precious");
        let target = b.join("f");
        assert!(!std::fs::symlink_metadata(&target)
            .expect("md")
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "payload");
        assert!(no_parts(&b));
        let _ = (
            std::fs::remove_dir_all(&a),
            std::fs::remove_dir_all(&b),
            std::fs::remove_dir_all(&elsewhere),
        );
    }

    #[tokio::test]
    async fn a_leftover_part_from_an_earlier_run_is_replaced() {
        let (a, b) = (temp_dir(), temp_dir());
        std::fs::write(a.join("f"), "fresh").expect("write");
        std::fs::write(b.join(format!("f{PART_SUFFIX}")), "stale half").expect("write");
        let ft = LocalFileTransfer::new();
        let obs = Recorder::new(vec![]);
        let r = copy(
            &ft,
            &[file_src(&a.join("f"))],
            &ft,
            &s(&b),
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(r.copied, 1, "{r:?}");
        assert_eq!(std::fs::read_to_string(b.join("f")).expect("read"), "fresh");
        assert!(no_parts(&b));
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[tokio::test]
    async fn a_cut_off_subtree_still_completes_the_progress() {
        let (a, b) = (temp_dir(), temp_dir());
        std::fs::create_dir_all(a.join("d/sub")).expect("mkdir");
        std::fs::write(a.join("d/sub/x"), "xxxx").expect("write");
        std::fs::write(a.join("d/y"), "yy").expect("write");
        // A file where the target directory `d` should go.
        std::fs::write(b.join("d"), "in the way").expect("write");
        let ft = LocalFileTransfer::new();
        let obs = Recorder::new(vec![]);
        let sources = vec![CopySource {
            path: s(&a.join("d")),
            kind: EntryKind::Dir,
        }];
        let r = copy(&ft, &sources, &ft, &s(&b), &obs, &CancellationToken::new()).await;
        assert_eq!((r.copied, r.failed.len()), (0, 1), "{r:?}");
        let events = obs.events.lock().expect("lock");
        assert!(
            events.contains(&CopyEvent::Progress { done: 6, total: 6 }),
            "{events:?}"
        );
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    /// Local side whose reader trips the token after the first chunk, and whose
    /// `remove_file` can be made to fail (a lost connection).
    struct Flaky {
        inner: LocalFileTransfer,
        cancel_on_read: Option<CancellationToken>,
        fail_remove: bool,
        fail_write: bool,
        /// Set once a write started: the link only "drops" after that.
        writing: std::sync::atomic::AtomicBool,
        /// Handed out once by `take_leftovers`.
        leftovers: Mutex<Vec<String>>,
    }

    struct TrippingReader {
        inner: TransferReader,
        token: CancellationToken,
    }

    impl tokio::io::AsyncRead for TrippingReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let before = buf.filled().len();
            let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
            if buf.filled().len() > before {
                self.token.cancel();
            }
            r
        }
    }

    struct FailingWriter;

    impl tokio::io::AsyncWrite for FailingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::other("connection reset")))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[async_trait::async_trait]
    impl FileTransfer for Flaky {
        fn path_style(&self) -> PathStyle {
            self.inner.path_style()
        }
        async fn home_dir(&self) -> Result<String> {
            self.inner.home_dir().await
        }
        async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
            self.inner.list_dir(path).await
        }
        async fn stat(&self, path: &str) -> Result<Option<Metadata>> {
            self.inner.stat(path).await
        }
        async fn open_read(&self, path: &str) -> Result<TransferReader> {
            let r = self.inner.open_read(path).await?;
            Ok(match &self.cancel_on_read {
                Some(t) => Box::new(TrippingReader {
                    inner: r,
                    token: t.clone(),
                }),
                None => r,
            })
        }
        async fn open_write(&self, path: &str) -> Result<TransferWriter> {
            // Create the part file for real, so a failed remove leaves it behind.
            let w = self.inner.open_write(path).await?;
            self.writing.store(true, Ordering::Relaxed);
            if self.fail_write {
                drop(w);
                return Ok(Box::new(FailingWriter));
            }
            Ok(w)
        }
        async fn rename(&self, from: &str, to: &str) -> Result<()> {
            self.inner.rename(from, to).await
        }
        async fn remove_file(&self, path: &str) -> Result<()> {
            if self.fail_remove && self.writing.load(Ordering::Relaxed) {
                return Err(CoreError::Other("connection lost".into()));
            }
            self.inner.remove_file(path).await
        }
        async fn create_dir(&self, path: &str) -> Result<()> {
            self.inner.create_dir(path).await
        }
        async fn set_permissions(&self, path: &str, mode: u32) -> Result<()> {
            self.inner.set_permissions(path, mode).await
        }
        fn take_leftovers(&self) -> Vec<String> {
            std::mem::take(&mut *self.leftovers.lock().expect("lock"))
        }
    }

    #[tokio::test]
    async fn what_the_target_side_left_behind_reaches_the_report() {
        let (a, b) = (temp_dir(), temp_dir());
        std::fs::write(a.join("f"), "data").expect("write");
        let dst = Flaky {
            inner: LocalFileTransfer::new(),
            cancel_on_read: None,
            fail_remove: false,
            fail_write: false,
            writing: Default::default(),
            leftovers: Mutex::new(vec!["/srv/f.filar-old-1a2b3c4d".into()]),
        };
        let obs = Recorder::new(vec![]);
        let r = copy(
            &LocalFileTransfer::new(),
            &[file_src(&a.join("f"))],
            &dst,
            &s(&b),
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(r.copied, 1);
        assert_eq!(
            r.leftover_parts,
            vec!["/srv/f.filar-old-1a2b3c4d".to_string()]
        );
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[tokio::test]
    async fn cancel_mid_file_removes_the_part_and_keeps_the_old_target() {
        let (a, b) = (temp_dir(), temp_dir());
        std::fs::write(a.join("big"), vec![7u8; CHUNK * 4]).expect("write");
        std::fs::write(b.join("big"), "old").expect("write");
        let token = CancellationToken::new();
        let src = Flaky {
            inner: LocalFileTransfer::new(),
            cancel_on_read: Some(token.clone()),
            fail_remove: false,
            fail_write: false,
            writing: Default::default(),
            leftovers: Default::default(),
        };
        let dst = LocalFileTransfer::new();
        let obs = Recorder::new(vec![OverwriteDecision::Overwrite]);
        let r = copy(
            &src,
            &[file_src(&a.join("big"))],
            &dst,
            &s(&b),
            &obs,
            &token,
        )
        .await;
        assert!(r.cancelled);
        assert_eq!(r.copied, 0);
        assert!(r.leftover_parts.is_empty());
        assert!(no_parts(&b));
        assert_eq!(std::fs::read_to_string(b.join("big")).expect("read"), "old");
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[tokio::test]
    async fn a_part_that_cannot_be_removed_is_reported_by_path() {
        let (a, b) = (temp_dir(), temp_dir());
        std::fs::write(a.join("f"), "data").expect("write");
        let dst = Flaky {
            inner: LocalFileTransfer::new(),
            cancel_on_read: None,
            fail_remove: true,
            fail_write: true,
            writing: Default::default(),
            leftovers: Default::default(),
        };
        let obs = Recorder::new(vec![]);
        let r = copy(
            &LocalFileTransfer::new(),
            &[file_src(&a.join("f"))],
            &dst,
            &s(&b),
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(r.copied, 0);
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert!(r.failed[0].1.contains("connection reset"), "{r:?}");
        let expected = format!("{}{PART_SUFFIX}", s(&b.join("f")));
        assert_eq!(r.leftover_parts, vec![expected.clone()]);
        assert!(Path::new(&expected).exists());
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[tokio::test]
    async fn an_old_part_that_cannot_be_removed_is_reported_by_path() {
        let (a, b) = (temp_dir(), temp_dir());
        std::fs::write(a.join("f"), "data").expect("write");
        let old_part = b.join(format!("f{PART_SUFFIX}"));
        std::fs::write(&old_part, "stale").expect("write");
        let dst = Flaky {
            inner: LocalFileTransfer::new(),
            cancel_on_read: None,
            fail_remove: true,
            fail_write: false,
            // The link is already "down" before this copy starts.
            writing: std::sync::atomic::AtomicBool::new(true),
            leftovers: Default::default(),
        };
        let obs = Recorder::new(vec![]);
        let r = copy(
            &LocalFileTransfer::new(),
            &[file_src(&a.join("f"))],
            &dst,
            &s(&b),
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(r.copied, 0);
        assert_eq!(r.leftover_parts, vec![s(&old_part)], "{r:?}");
        assert!(!b.join("f").exists());
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    #[tokio::test]
    async fn a_directory_in_the_way_fails_that_file_only() {
        let (a, b) = (temp_dir(), temp_dir());
        std::fs::write(a.join("name"), "x").expect("write");
        std::fs::write(a.join("other"), "y").expect("write");
        std::fs::create_dir(b.join("name")).expect("mkdir");
        let ft = LocalFileTransfer::new();
        let obs = Recorder::new(vec![]);
        let r = copy(
            &ft,
            &[file_src(&a.join("name")), file_src(&a.join("other"))],
            &ft,
            &s(&b),
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!((r.copied, r.failed.len()), (1, 1));
        assert_eq!(obs.asked(), 0);
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }
}
