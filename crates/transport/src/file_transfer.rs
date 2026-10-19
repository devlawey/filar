//! File transfer abstraction (#532).
//!
//! [`FileTransfer`] is the filesystem "on the other side" as the file panels
//! (Ctrl+G) and the copy engine see it: the local disk or an SSH host. Both
//! implementations expose the same operations, so local→host, host→local and
//! host→host copies are one code path.
//!
//! The trait is deliberately separate from [`crate::CommandExecutor`]:
//! transferring files is not running commands, and the executor contract stays
//! untouched. Remote writes are only ever made on an explicit user action
//! (AGENTS.md, invariant 1 and its file-transfer exception, #531).

use std::io;
use std::time::SystemTime;

use filar_core::{CoreError, Result};
use tokio::io::{AsyncRead, AsyncWrite};

/// What a directory entry or a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link, not followed. Use [`FileTransfer::stat`] to see what it
    /// points to.
    Symlink,
    /// Anything else (device, socket, FIFO, …).
    Other,
}

/// Metadata of a file system object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// Object kind.
    pub kind: EntryKind,
    /// Size in bytes (0 when unknown, e.g. for directories on some servers).
    pub size: u64,
    /// Last modification time, when the side reports it.
    pub modified: Option<SystemTime>,
    /// POSIX permission bits (`0o7777` mask), when the side has them.
    pub mode: Option<u32>,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// File name without the directory part.
    pub name: String,
    /// Metadata of the entry itself; symlinks are reported as
    /// [`EntryKind::Symlink`], not followed.
    pub metadata: Metadata,
}

/// Path conventions of a [`FileTransfer`] side.
///
/// Paths cross the trait as strings: a remote host is always POSIX no matter
/// which OS the client runs on, so `std::path::Path` cannot be used for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStyle {
    /// `/`-separated, single root `/`. Every SSH host.
    Posix,
    /// Drive-letter paths (`C:\…`). The empty string is the virtual root above
    /// all drives; listing it yields the drives.
    Windows,
}

impl PathStyle {
    /// Join `name` onto the directory `base`. `..` goes to the parent; an
    /// absolute `name` replaces `base`.
    pub fn join(self, base: &str, name: &str) -> String {
        if name == ".." {
            return self.parent(base).unwrap_or_else(|| base.to_string());
        }
        match self {
            PathStyle::Posix => {
                if name.starts_with('/') {
                    return name.to_string();
                }
                let base = base.trim_end_matches('/');
                if base.is_empty() {
                    format!("/{name}")
                } else {
                    format!("{base}/{name}")
                }
            }
            PathStyle::Windows => {
                if base.is_empty() || is_windows_absolute(name) {
                    return name.to_string();
                }
                if base.ends_with('\\') || base.ends_with('/') {
                    format!("{base}{name}")
                } else {
                    format!("{base}\\{name}")
                }
            }
        }
    }

    /// Parent directory of `path`, or `None` at the top.
    ///
    /// POSIX: the top is `/`. Windows: a drive root (`C:\`) has the virtual
    /// drive list `""` as its parent, and `""` is the top.
    pub fn parent(self, path: &str) -> Option<String> {
        match self {
            PathStyle::Posix => {
                let trimmed = path.trim_end_matches('/');
                if trimmed.is_empty() {
                    return None;
                }
                match trimmed.rfind('/') {
                    None | Some(0) => Some("/".to_string()),
                    Some(i) => Some(trimmed[..i].to_string()),
                }
            }
            PathStyle::Windows => {
                if path.is_empty() {
                    return None;
                }
                let trimmed = path.trim_end_matches(['\\', '/']);
                if is_drive(trimmed) {
                    return Some(String::new());
                }
                match trimmed.rfind(['\\', '/']) {
                    None => Some(String::new()),
                    Some(i) => {
                        let head = &trimmed[..i];
                        if is_drive(head) {
                            Some(format!("{head}\\"))
                        } else {
                            Some(head.to_string())
                        }
                    }
                }
            }
        }
    }

    /// Last component of `path` (the file name), or the path itself when it
    /// has no separator.
    pub fn file_name(self, path: &str) -> String {
        let seps: &[char] = match self {
            PathStyle::Posix => &['/'],
            PathStyle::Windows => &['\\', '/'],
        };
        let trimmed = path.trim_end_matches(seps);
        match trimmed.rfind(seps) {
            Some(i) => trimmed[i + 1..].to_string(),
            None => trimmed.to_string(),
        }
    }
}

/// `C:` — a bare drive designator.
fn is_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// `C:\…`, `C:/…` or a UNC `\\server\…` path.
fn is_windows_absolute(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || s.starts_with("\\\\")
}

/// Boxed async reader returned by [`FileTransfer::open_read`].
pub type TransferReader = Box<dyn AsyncRead + Send + Unpin>;
/// Boxed async writer returned by [`FileTransfer::open_write`].
pub type TransferWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// A filesystem the file panels and the copy engine work with: the local disk
/// ([`LocalFileTransfer`]) or an SSH host (SFTP, #533).
///
/// Errors keep the [`io::ErrorKind`] where one exists (`NotFound`,
/// `PermissionDenied`, …) wrapped in [`CoreError::Io`], with the operation
/// and path in the message. They never contain secrets.
///
/// Required of every implementation: a path that does not exist is reported
/// as [`CoreError::Io`] with [`io::ErrorKind::NotFound`] — the copy engine
/// tells "nothing to clean up" from a real failure by it (#541 review).
#[async_trait::async_trait]
pub trait FileTransfer: Send + Sync {
    /// Path conventions of this side.
    fn path_style(&self) -> PathStyle;

    /// Home directory of the user on this side.
    async fn home_dir(&self) -> Result<String>;

    /// List `path`. Order is unspecified; `.` and `..` are not included.
    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>>;

    /// Metadata of `path`, following symlinks. A missing path is `Ok(None)`,
    /// not an error.
    async fn stat(&self, path: &str) -> Result<Option<Metadata>>;

    /// Open `path` for reading from the start.
    async fn open_read(&self, path: &str) -> Result<TransferReader>;

    /// Create `path`, which must not exist yet, and open it for writing.
    /// Exclusive: an existing file — or a symlink, which is never followed —
    /// is an `AlreadyExists` error, so a write can only land in a fresh file.
    /// The caller must `shutdown` the writer to flush it, and drop it to close.
    async fn open_write(&self, path: &str) -> Result<TransferWriter>;

    /// Rename `from` to `to`, replacing an existing file at `to`.
    async fn rename(&self, from: &str, to: &str) -> Result<()>;

    /// Remove the file `path`.
    async fn remove_file(&self, path: &str) -> Result<()>;

    /// Create the directory `path` (its parent must exist).
    async fn create_dir(&self, path: &str) -> Result<()>;

    /// Set POSIX permission bits on `path`. Sides without POSIX permissions
    /// (Windows) accept the call and do nothing.
    async fn set_permissions(&self, path: &str, mode: u32) -> Result<()>;
}

/// Wrap an I/O error with the operation and path, keeping its kind.
pub(crate) fn io_context(op: &str, path: &str, err: io::Error) -> CoreError {
    CoreError::Io(io::Error::new(err.kind(), format!("{op} {path}: {err}")))
}

/// [`FileTransfer`] over the client's own disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalFileTransfer;

impl LocalFileTransfer {
    /// Create a local file transfer side.
    pub fn new() -> Self {
        Self
    }
}

fn local_metadata(md: &std::fs::Metadata) -> Metadata {
    let ft = md.file_type();
    let kind = if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        Some(md.permissions().mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let mode = None;
    Metadata {
        kind,
        size: if kind == EntryKind::Dir { 0 } else { md.len() },
        modified: md.modified().ok(),
        mode,
    }
}

/// Existing drive roots (`C:\`, `D:\`, …), as the Windows virtual root lists.
#[cfg(windows)]
async fn list_drives() -> Vec<DirEntry> {
    let mut out = Vec::new();
    for letter in b'A'..=b'Z' {
        let root = format!("{}:\\", letter as char);
        if tokio::fs::metadata(&root).await.is_ok() {
            out.push(DirEntry {
                name: root,
                metadata: Metadata {
                    kind: EntryKind::Dir,
                    size: 0,
                    modified: None,
                    mode: None,
                },
            });
        }
    }
    out
}

#[async_trait::async_trait]
impl FileTransfer for LocalFileTransfer {
    fn path_style(&self) -> PathStyle {
        if cfg!(windows) {
            PathStyle::Windows
        } else {
            PathStyle::Posix
        }
    }

    async fn home_dir(&self) -> Result<String> {
        dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| CoreError::Other("cannot determine the local home directory".into()))
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        #[cfg(windows)]
        if path.is_empty() {
            return Ok(list_drives().await);
        }
        let mut rd = tokio::fs::read_dir(path)
            .await
            .map_err(|e| io_context("list", path, e))?;
        let mut out = Vec::new();
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| io_context("list", path, e))?
        {
            // An entry that vanished or cannot be inspected is skipped rather
            // than failing the whole listing.
            let Ok(md) = tokio::fs::symlink_metadata(entry.path()).await else {
                continue;
            };
            out.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                metadata: local_metadata(&md),
            });
        }
        Ok(out)
    }

    async fn stat(&self, path: &str) -> Result<Option<Metadata>> {
        match tokio::fs::metadata(path).await {
            Ok(md) => Ok(Some(local_metadata(&md))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_context("stat", path, e)),
        }
    }

    async fn open_read(&self, path: &str) -> Result<TransferReader> {
        let f = tokio::fs::File::open(path)
            .await
            .map_err(|e| io_context("open", path, e))?;
        Ok(Box::new(f))
    }

    async fn open_write(&self, path: &str) -> Result<TransferWriter> {
        let f = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
            .map_err(|e| io_context("create", path, e))?;
        Ok(Box::new(f))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        tokio::fs::rename(from, to)
            .await
            .map_err(|e| io_context("rename", &format!("{from} -> {to}"), e))
    }

    async fn remove_file(&self, path: &str) -> Result<()> {
        tokio::fs::remove_file(path)
            .await
            .map_err(|e| io_context("remove", path, e))
    }

    async fn create_dir(&self, path: &str) -> Result<()> {
        tokio::fs::create_dir(path)
            .await
            .map_err(|e| io_context("mkdir", path, e))
    }

    async fn set_permissions(&self, path: &str, mode: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
                .await
                .map_err(|e| io_context("chmod", path, e))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("filar-ft-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn s(p: &std::path::Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn posix_join_and_parent() {
        let p = PathStyle::Posix;
        assert_eq!(p.join("/", "etc"), "/etc");
        assert_eq!(p.join("/etc/", "nginx"), "/etc/nginx");
        assert_eq!(p.join("/etc", "/var"), "/var");
        assert_eq!(p.join("/etc/nginx", ".."), "/etc");
        assert_eq!(p.parent("/etc/nginx"), Some("/etc".into()));
        assert_eq!(p.parent("/etc"), Some("/".into()));
        assert_eq!(p.parent("/"), None);
        assert_eq!(p.parent("/var/log/"), Some("/var".into()));
        assert_eq!(p.file_name("/var/log/syslog"), "syslog");
        assert_eq!(p.file_name("/var/log/"), "log");
    }

    #[test]
    fn windows_join_and_parent_up_to_drive_list() {
        let w = PathStyle::Windows;
        assert_eq!(w.join("C:\\", "Users"), "C:\\Users");
        assert_eq!(w.join("C:\\Users", "me"), "C:\\Users\\me");
        assert_eq!(w.join("", "D:\\"), "D:\\");
        assert_eq!(w.join("C:\\Users", "D:\\x"), "D:\\x");
        assert_eq!(w.parent("C:\\Users\\me"), Some("C:\\Users".into()));
        assert_eq!(w.parent("C:\\Users"), Some("C:\\".into()));
        assert_eq!(w.parent("C:\\"), Some(String::new()));
        assert_eq!(w.parent(""), None);
        assert_eq!(w.join("C:\\", ".."), "");
        assert_eq!(w.file_name("C:\\Users\\me\\a.txt"), "a.txt");
    }

    #[tokio::test]
    async fn local_write_read_list_stat() {
        let ft = LocalFileTransfer::new();
        let dir = temp_dir();
        let file = s(&dir.join("a.txt"));

        let mut w = ft.open_write(&file).await.expect("open_write");
        w.write_all(b"hello").await.expect("write");
        w.shutdown().await.expect("shutdown");

        let mut r = ft.open_read(&file).await.expect("open_read");
        let mut buf = String::new();
        r.read_to_string(&mut buf).await.expect("read");
        assert_eq!(buf, "hello");

        let md = ft.stat(&file).await.expect("stat").expect("exists");
        assert_eq!(md.kind, EntryKind::File);
        assert_eq!(md.size, 5);
        assert!(md.modified.is_some());

        ft.create_dir(&s(&dir.join("sub"))).await.expect("mkdir");
        let mut names: Vec<_> = ft
            .list_dir(&s(&dir))
            .await
            .expect("list")
            .into_iter()
            .map(|e| (e.name, e.metadata.kind))
            .collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            names,
            vec![
                ("a.txt".into(), EntryKind::File),
                ("sub".into(), EntryKind::Dir)
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_stat_missing_is_none_and_errors_keep_kind() {
        let ft = LocalFileTransfer::new();
        let dir = temp_dir();
        let missing = s(&dir.join("nope"));
        assert!(ft.stat(&missing).await.expect("stat").is_none());
        let Err(CoreError::Io(e)) = ft.open_read(&missing).await else {
            panic!("expected an io error");
        };
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("nope"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_open_write_is_exclusive() {
        let ft = LocalFileTransfer::new();
        let dir = temp_dir();
        let f = s(&dir.join("exists"));
        std::fs::write(&f, "keep").expect("write");
        let Err(CoreError::Io(e)) = ft.open_write(&f).await else {
            panic!("expected AlreadyExists");
        };
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&f).expect("read"), "keep");
        #[cfg(unix)]
        {
            let victim = dir.join("victim");
            std::fs::write(&victim, "keep").expect("write");
            let link = dir.join("link");
            std::os::unix::fs::symlink(&victim, &link).expect("symlink");
            assert!(ft.open_write(&s(&link)).await.is_err());
            assert_eq!(std::fs::read_to_string(&victim).expect("read"), "keep");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_rename_replaces_and_remove() {
        let ft = LocalFileTransfer::new();
        let dir = temp_dir();
        let a = s(&dir.join("a"));
        let b = s(&dir.join("b"));
        std::fs::write(&a, "new").expect("write a");
        std::fs::write(&b, "old").expect("write b");
        ft.rename(&a, &b).await.expect("rename");
        assert_eq!(std::fs::read_to_string(&b).expect("read b"), "new");
        assert!(ft.stat(&a).await.expect("stat").is_none());
        ft.remove_file(&b).await.expect("remove");
        assert!(ft.stat(&b).await.expect("stat").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_permissions_and_symlink_listing() {
        let ft = LocalFileTransfer::new();
        let dir = temp_dir();
        let f = s(&dir.join("f"));
        std::fs::write(&f, "x").expect("write");
        ft.set_permissions(&f, 0o640).await.expect("chmod");
        assert_eq!(
            ft.stat(&f).await.expect("stat").expect("exists").mode,
            Some(0o640)
        );

        std::os::unix::fs::symlink(&f, dir.join("link")).expect("symlink");
        let list = ft.list_dir(&s(&dir)).await.expect("list");
        let link = list.iter().find(|e| e.name == "link").expect("link listed");
        assert_eq!(link.metadata.kind, EntryKind::Symlink);
        // stat follows the link.
        let followed = ft
            .stat(&s(&dir.join("link")))
            .await
            .expect("stat")
            .expect("exists");
        assert_eq!(followed.kind, EntryKind::File);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_home_dir_exists() {
        let home = LocalFileTransfer::new().home_dir().await.expect("home");
        assert!(!home.is_empty());
    }
}
