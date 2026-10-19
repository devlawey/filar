//! [`FileTransfer`] over SFTP (#533).
//!
//! SFTP is a standard sshd subsystem: nothing is installed or started on the
//! remote host beyond what sshd already offers (AGENTS.md, invariant 1 and its
//! #531 exception). Writes happen only through the trait, on an explicit user
//! action in the file panels.
//!
//! Two ways in:
//! - [`crate::SshExecutor::open_sftp`] — a new channel on a tab's existing
//!   connection; the shell is untouched;
//! - [`SftpFileTransfer::connect`] — a connection of its own, for a panel that
//!   shows a host other than the tab's. Same host-key check and secret sources
//!   as the shell connection.

use std::io;
use std::time::{Duration, UNIX_EPOCH};

use filar_core::{CoreError, Result, SecretProvider, SshTarget};
use russh::client::{Handle, Msg};
use russh::{Channel, Disconnect};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::file_transfer::{
    DirEntry, EntryKind, FileTransfer, Metadata, PathStyle, TransferReader, TransferWriter,
};
use crate::ssh::{connect_authenticated, request_subsystem, SshHandler, SshTransportConfig};

/// Seconds to wait for one SFTP response before giving up on it.
const SFTP_TIMEOUT_SECS: u64 = 30;

/// Suffix the replaced target steps aside to while a rename takes its name.
pub const OLD_SUFFIX: &str = ".filar-old";

/// An SSH host's filesystem over SFTP.
pub struct SftpFileTransfer {
    sftp: SftpSession,
    /// Target name, for error messages (never a secret).
    host: String,
    /// The connection this side owns ([`SftpFileTransfer::connect`]); `None`
    /// when riding on a tab's connection.
    own: Option<Mutex<Handle<SshHandler>>>,
}

impl SftpFileTransfer {
    /// Open a connection of its own to `target` (host key verified, secrets
    /// from `secrets`, as for the shell) and start SFTP on it.
    pub async fn connect(
        target: &SshTarget,
        cfg: &SshTransportConfig,
        secrets: &dyn SecretProvider,
    ) -> Result<Self> {
        let handle = connect_authenticated(target, cfg, secrets).await?;
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| CoreError::Other(format!("{}: failed to open channel: {e}", target.name)));
        let channel = match channel {
            Ok(c) => c,
            Err(e) => {
                let _ = handle
                    .disconnect(Disconnect::ByApplication, "", "English")
                    .await;
                return Err(e);
            }
        };
        let channel = match request_subsystem(channel, "sftp").await {
            Ok(c) => c,
            Err(e) => {
                let _ = handle
                    .disconnect(Disconnect::ByApplication, "", "English")
                    .await;
                return Err(CoreError::Other(format!("{}: {e}", target.name)));
            }
        };
        Self::from_channel(channel, &target.name, Some(handle)).await
    }

    /// Start SFTP on a channel where the `sftp` subsystem is already running.
    pub(crate) async fn from_channel(
        channel: Channel<Msg>,
        host: &str,
        own: Option<Handle<SshHandler>>,
    ) -> Result<Self> {
        let sftp = match SftpSession::new(channel.into_stream()).await {
            Ok(s) => s,
            Err(e) => {
                if let Some(h) = own {
                    let _ = h.disconnect(Disconnect::ByApplication, "", "English").await;
                }
                return Err(CoreError::Other(format!("{host}: SFTP start failed: {e}")));
            }
        };
        sftp.set_timeout(SFTP_TIMEOUT_SECS);
        info!(host = %host, "SFTP session ready");
        Ok(Self {
            sftp,
            host: host.to_string(),
            own: own.map(Mutex::new),
        })
    }

    /// Close the SFTP channel, and the connection when this side owns one.
    /// A tab's connection is left alone.
    pub async fn close(&self) -> Result<()> {
        // Closing is best effort: a channel already gone (lost connection) has
        // nothing left to close. Logged, not returned (#540 review).
        if let Err(e) = self.sftp.close().await {
            debug!(host = %self.host, error = %e, "SFTP channel close");
        }
        if let Some(h) = &self.own {
            let h = h.lock().await;
            if let Err(e) = h.disconnect(Disconnect::ByApplication, "", "English").await {
                debug!(host = %self.host, error = %e, "SFTP connection disconnect");
            }
        }
        Ok(())
    }

    fn err(&self, op: &str, path: &str, e: SftpError) -> CoreError {
        map_sftp_error(&self.host, op, path, e)
    }
}

/// Map an SFTP error to a filar error that keeps the meaning a UI acts on
/// (`NotFound`, `PermissionDenied`, timeout) and names host, operation and path.
pub(crate) fn map_sftp_error(host: &str, op: &str, path: &str, e: SftpError) -> CoreError {
    let kind = match &e {
        SftpError::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => io::ErrorKind::NotFound,
            StatusCode::PermissionDenied => io::ErrorKind::PermissionDenied,
            StatusCode::NoConnection | StatusCode::ConnectionLost => {
                return CoreError::Other(format!("{host}: {op} {path}: connection lost"));
            }
            StatusCode::OpUnsupported => io::ErrorKind::Unsupported,
            _ => io::ErrorKind::Other,
        },
        SftpError::Timeout => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    CoreError::Io(io::Error::new(kind, format!("{host}: {op} {path}: {e}")))
}

/// SFTP attributes → [`Metadata`].
pub(crate) fn metadata_from(attrs: &FileAttributes) -> Metadata {
    let kind = match attrs.file_type() {
        FileType::Dir => EntryKind::Dir,
        FileType::File => EntryKind::File,
        FileType::Symlink => EntryKind::Symlink,
        FileType::Other => EntryKind::Other,
    };
    Metadata {
        kind,
        size: if kind == EntryKind::Dir {
            0
        } else {
            attrs.size.unwrap_or(0)
        },
        modified: attrs
            .mtime
            .map(|t| UNIX_EPOCH + Duration::from_secs(u64::from(t))),
        mode: attrs.permissions.map(|p| p & 0o7777),
    }
}

fn is_not_found(e: &SftpError) -> bool {
    matches!(e, SftpError::Status(s) if s.status_code == StatusCode::NoSuchFile)
}

#[async_trait::async_trait]
impl FileTransfer for SftpFileTransfer {
    fn path_style(&self) -> PathStyle {
        PathStyle::Posix
    }

    async fn home_dir(&self) -> Result<String> {
        // An SFTP session starts in the login user's home directory.
        self.sftp
            .canonicalize(".")
            .await
            .map_err(|e| self.err("resolve", "~", e))
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let rd = self
            .sftp
            .read_dir(path)
            .await
            .map_err(|e| self.err("list", path, e))?;
        Ok(rd
            .filter(|e| {
                let n = e.file_name();
                n != "." && n != ".."
            })
            .map(|e| DirEntry {
                name: e.file_name(),
                metadata: metadata_from(&e.metadata()),
            })
            .collect())
    }

    async fn stat(&self, path: &str) -> Result<Option<Metadata>> {
        match self.sftp.metadata(path).await {
            Ok(a) => Ok(Some(metadata_from(&a))),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(self.err("stat", path, e)),
        }
    }

    async fn open_read(&self, path: &str) -> Result<TransferReader> {
        let f = self
            .sftp
            .open(path)
            .await
            .map_err(|e| self.err("open", path, e))?;
        Ok(Box::new(f))
    }

    async fn open_write(&self, path: &str) -> Result<TransferWriter> {
        // Exclusive, like the local side: an existing file or a symlink at
        // `path` makes the open fail instead of being written through.
        let f = self
            .sftp
            .open_with_flags(
                path,
                OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
            )
            .await
            .map_err(|e| self.err("create", path, e))?;
        Ok(Box::new(f))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        // SFTPv3 RENAME refuses an existing target (OpenSSH answers a plain
        // FAILURE) and the client offers no `posix-rename@openssh.com`. The
        // contract says "replaces", so — only for that refusal, and only when
        // both ends are files — the old target steps aside as
        // `<to>.filar-old`, the new file takes its name, and the old one is
        // dropped. If taking the name fails, the old file is put back: the
        // target is never lost (#540 review).
        let first = match self.sftp.rename(from, to).await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let refused =
            matches!(&first, SftpError::Status(s) if s.status_code == StatusCode::Failure);
        let is_file = |m: &std::result::Result<FileAttributes, SftpError>| matches!(m, Ok(a) if a.file_type() == FileType::File);
        if !refused
            || !is_file(&self.sftp.metadata(from).await)
            || !is_file(&self.sftp.metadata(to).await)
        {
            return Err(self.err("rename", &format!("{from} -> {to}"), first));
        }
        let backup = format!("{to}{OLD_SUFFIX}");
        self.sftp
            .rename(to, backup.as_str())
            .await
            .map_err(|e| self.err("rename", &format!("{to} -> {backup}"), e))?;
        match self.sftp.rename(from, to).await {
            Ok(()) => {
                if let Err(e) = self.sftp.remove_file(backup.as_str()).await {
                    warn!(host = %self.host, path = %backup, error = %e, "could not remove the replaced file");
                }
                Ok(())
            }
            Err(e) => {
                if let Err(back) = self.sftp.rename(backup.as_str(), to).await {
                    warn!(host = %self.host, path = %backup, error = %back, "could not restore the replaced file");
                    return Err(CoreError::Other(format!(
                        "{}: rename {from} -> {to}: {e}; the previous file is kept as {backup}",
                        self.host
                    )));
                }
                Err(self.err("rename", &format!("{from} -> {to}"), e))
            }
        }
    }

    async fn remove_file(&self, path: &str) -> Result<()> {
        self.sftp
            .remove_file(path)
            .await
            .map_err(|e| self.err("remove", path, e))
    }

    async fn create_dir(&self, path: &str) -> Result<()> {
        self.sftp
            .create_dir(path)
            .await
            .map_err(|e| self.err("mkdir", path, e))
    }

    async fn set_permissions(&self, path: &str, mode: u32) -> Result<()> {
        let mut attrs = FileAttributes::empty();
        attrs.permissions = Some(mode & 0o7777);
        self.sftp
            .set_metadata(path, attrs)
            .await
            .map_err(|e| self.err("chmod", path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::protocol::Status;

    fn status(code: StatusCode) -> SftpError {
        SftpError::Status(Status {
            id: 1,
            status_code: code,
            error_message: "server says".into(),
            language_tag: "en".into(),
        })
    }

    fn io_kind(e: CoreError) -> io::ErrorKind {
        match e {
            CoreError::Io(e) => e.kind(),
            other => panic!("expected io error, got {other:?}"),
        }
    }

    #[test]
    fn statuses_map_to_kinds_and_name_host_and_path() {
        let e = map_sftp_error("web1", "open", "/etc/x", status(StatusCode::NoSuchFile));
        let text = e.to_string();
        assert!(
            text.contains("web1") && text.contains("open /etc/x"),
            "{text}"
        );
        assert_eq!(io_kind(e), io::ErrorKind::NotFound);
        assert_eq!(
            io_kind(map_sftp_error(
                "h",
                "x",
                "/p",
                status(StatusCode::PermissionDenied)
            )),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            io_kind(map_sftp_error("h", "x", "/p", SftpError::Timeout)),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            io_kind(map_sftp_error("h", "x", "/p", status(StatusCode::Failure))),
            io::ErrorKind::Other
        );
    }

    #[test]
    fn a_lost_connection_is_recognised_as_such() {
        let e = map_sftp_error("h", "write", "/p", status(StatusCode::ConnectionLost));
        assert!(crate::is_connection_lost(&e), "{e}");
    }

    #[test]
    fn attributes_convert_to_metadata() {
        let mut a = FileAttributes::empty();
        a.size = Some(42);
        a.permissions = Some(0o100644);
        a.mtime = Some(1_700_000_000);
        let m = metadata_from(&a);
        assert_eq!(m.kind, EntryKind::File);
        assert_eq!((m.size, m.mode), (42, Some(0o644)));
        assert_eq!(
            m.modified,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );

        let mut d = FileAttributes::empty();
        d.size = Some(4096);
        d.permissions = Some(0o040755);
        let m = metadata_from(&d);
        assert_eq!((m.kind, m.size), (EntryKind::Dir, 0));

        let mut l = FileAttributes::empty();
        l.permissions = Some(0o120777);
        assert_eq!(metadata_from(&l).kind, EntryKind::Symlink);
    }

    fn docker_target() -> SshTarget {
        SshTarget {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "testuser".into(),
            auth: filar_core::SshAuth::Password { password: None },
            host_key_policy: filar_core::HostKeyPolicy::Tofu,
            tags: Vec::new(),
        }
    }

    /// Set SSH_PASSWORD; needs `docker/sshd` on port 2222.
    #[tokio::test]
    #[ignore = "requires Docker sshd container on port 2222"]
    async fn sftp_roundtrip_against_sshd() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        std::env::var("SSH_PASSWORD").expect("set SSH_PASSWORD");
        let ft = SftpFileTransfer::connect(
            &docker_target(),
            &SshTransportConfig::default(),
            &filar_core::EnvSecretProvider::new(),
        )
        .await
        .expect("connect");
        let home = ft.home_dir().await.expect("home");
        let dir = ft
            .path_style()
            .join(&home, &format!("filar-sftp-{}", uuid::Uuid::new_v4()));
        ft.create_dir(&dir).await.expect("mkdir");
        let a = ft.path_style().join(&dir, "a");
        let b = ft.path_style().join(&dir, "b");

        let mut w = ft.open_write(&a).await.expect("create");
        w.write_all(b"new").await.expect("write");
        w.shutdown().await.expect("close");
        let mut w = ft.open_write(&b).await.expect("create");
        w.write_all(b"old").await.expect("write");
        w.shutdown().await.expect("close");

        assert!(
            ft.open_write(&b).await.is_err(),
            "open_write must be exclusive"
        );
        ft.set_permissions(&a, 0o600).await.expect("chmod");
        assert_eq!(
            ft.stat(&a).await.expect("stat").expect("exists").mode,
            Some(0o600)
        );
        ft.rename(&a, &b)
            .await
            .expect("rename over an existing file");
        let mut s = String::new();
        ft.open_read(&b)
            .await
            .expect("open")
            .read_to_string(&mut s)
            .await
            .expect("read");
        assert_eq!(s, "new");
        let names: Vec<_> = ft
            .list_dir(&dir)
            .await
            .expect("list")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["b".to_string()]);
        assert!(ft.stat(&a).await.expect("stat").is_none());
        let Err(CoreError::Io(e)) = ft.open_read(&a).await else {
            panic!("expected NotFound");
        };
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        ft.remove_file(&b).await.expect("remove");
        ft.sftp.remove_dir(dir.clone()).await.expect("rmdir");
        ft.close().await.expect("close");
    }

    /// The tab's shell keeps working after an SFTP channel is opened on its
    /// connection.
    #[tokio::test]
    #[ignore = "requires Docker sshd container on port 2222"]
    async fn sftp_on_the_executor_connection_leaves_the_shell_alone() {
        use crate::CommandExecutor;
        std::env::var("SSH_PASSWORD").expect("set SSH_PASSWORD");
        let exec = crate::SshExecutor::connect(&docker_target())
            .await
            .expect("connect");
        exec.run("cd /tmp").await.expect("cd");
        let ft = exec.open_sftp().await.expect("sftp");
        assert!(!ft.list_dir("/").await.expect("list").is_empty());
        ft.close().await.expect("close sftp");
        let r = exec.run("pwd").await.expect("pwd");
        assert_eq!(r.stdout.trim(), "/tmp");
        exec.close().await.expect("close");
    }
}
