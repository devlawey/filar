//! Fleet check "file against a reference", by hash (#440).
//!
//! The question "on which hosts does `/etc/nginx/nginx.conf` differ from the
//! reference?" is not one-host work, so it does not need `read_file` — which
//! the fleet refuses (#434). It is a fleet check: every host is asked for
//! the **hash** of the file, and the hashes are compared with a reference.
//!
//! # No content, constant size
//!
//! The command prints a marker and a SHA-256 line, nothing of the file. The
//! answer is then parsed down to a [`FileProbe`] — a validated 64-digit hex
//! digest or a named state — so whatever else a host prints is discarded
//! here, before anything is folded, let alone shown to the model. The size
//! of an answer does not depend on the size of the file.
//!
//! # Missing is a state, not an error
//!
//! A host without the file has answered the question: the file is not
//! there. [`FileProbe::Missing`] carries that, apart from a file that
//! exists but could not be hashed ([`FileProbe::Unreadable`] — no
//! permission, a directory) and from output this module does not recognise.
//!
//! # Why the command looks the way it does
//!
//! It has to pass `filar_transport::check_read_only`, which refuses quotes,
//! `$` and glob characters and has no `test`/`[` on its allowlist. So:
//!
//! ```text
//! stat -c present PATH 2>/dev/null && sha256sum PATH
//! ```
//!
//! `stat` prints the marker only when the path exists, so an empty answer
//! with a non-zero exit is "missing"; `sha256sum` then runs only on a path
//! that exists, and its failure is "unreadable". No locale-dependent error
//! text is parsed. The path is restricted to characters that need no
//! quoting (see [`FileBaseline::new`]) — a path the gate would refuse, or
//! the shell would split, is rejected when the check is declared rather
//! than when it is sent.
//!
//! `stat` also fails when a parent directory cannot be searched; such a
//! host reports the file as missing, because from that account it is.

use std::fmt;

/// Marker `stat` prints when the path exists.
pub const PRESENT_MARKER: &str = "present";

/// Longest accepted path, in bytes — `PATH_MAX` on Linux.
const MAX_PATH_LEN: usize = 4096;

/// Length of a SHA-256 digest in hex digits.
const SHA256_HEX_LEN: usize = 64;

/// What the file on every host is compared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileReference {
    /// The file as it is on this host of the fleet — the "golden" machine.
    /// Named by its configured host name.
    Host(String),
    /// A SHA-256 digest given in the check itself, lowercase hex.
    Sha256(String),
}

impl fmt::Display for FileReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host(host) => write!(f, "host {host}"),
            Self::Sha256(digest) => write!(f, "sha256 {}", short_digest(digest)),
        }
    }
}

/// One file to compare across a fleet, and what it is compared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBaseline {
    path: String,
    reference: FileReference,
}

impl FileBaseline {
    /// Validate a path and a reference.
    ///
    /// The path must be absolute and use only characters that need no
    /// quoting in a POSIX shell and pass the read-only gate: ASCII letters,
    /// digits and `/ . _ - + @ : , =`. A host name reference must be
    /// non-empty; a digest must be 64 hex digits (case is normalised).
    pub fn new(path: &str, reference: FileReference) -> Result<Self, String> {
        let path = validate_path(path)?;
        let reference = match reference {
            FileReference::Host(host) => {
                let host = host.trim();
                if host.is_empty() {
                    return Err("reference host must not be empty".into());
                }
                FileReference::Host(host.to_owned())
            }
            FileReference::Sha256(digest) => FileReference::Sha256(
                parse_sha256(digest.trim()).ok_or_else(|| {
                    format!("reference sha256 must be {SHA256_HEX_LEN} hex digits")
                })?,
            ),
        };
        Ok(Self { path, reference })
    }

    /// The file's absolute path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// What the file is compared with.
    pub fn reference(&self) -> &FileReference {
        &self.reference
    }

    /// The command every host runs. Prints a marker and a hash line, never
    /// the file.
    pub fn command(&self) -> String {
        format!(
            "stat -c {PRESENT_MARKER} {path} 2>/dev/null && sha256sum {path}",
            path = self.path
        )
    }
}

/// What one host said about the file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileProbe {
    /// The file is there; its SHA-256, lowercase hex.
    Present(String),
    /// There is no such file (as seen by the account the host runs under).
    Missing,
    /// The path exists but could not be hashed — permissions, a directory.
    Unreadable,
}

impl FileProbe {
    /// Parse a host's answer to [`FileBaseline::command`].
    ///
    /// `None` when the answer is not one the command produces — the caller
    /// treats that as an error of the host, not as a value. Only the digest
    /// survives parsing; any other text a host printed is dropped here.
    pub fn parse(stdout: &str, exit_code: Option<i32>) -> Option<Self> {
        let mut lines = stdout.lines().map(str::trim).filter(|line| !line.is_empty());
        let first = lines.next();
        let second = lines.next();
        let rest = lines.next();
        match (first, second, rest, exit_code) {
            // `stat` failed, so `&&` never ran `sha256sum`.
            (None, None, None, Some(code)) if code != 0 => Some(Self::Missing),
            (Some(PRESENT_MARKER), None, None, Some(code)) if code != 0 => Some(Self::Unreadable),
            (Some(PRESENT_MARKER), Some(line), None, Some(0)) => line
                .split_whitespace()
                .next()
                .and_then(parse_sha256)
                .map(Self::Present),
            _ => None,
        }
    }

    /// Short label for a table cell.
    pub fn label(&self) -> String {
        match self {
            Self::Present(digest) => format!("sha256 {}", short_digest(digest)),
            Self::Missing => "missing".into(),
            Self::Unreadable => "unreadable".into(),
        }
    }
}

/// The first 12 hex digits of a digest — enough to tell hashes apart in a
/// table, short enough to keep the table narrow.
pub fn short_digest(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

/// A 64-digit hex string, lowercased; `None` for anything else.
fn parse_sha256(text: &str) -> Option<String> {
    (text.len() == SHA256_HEX_LEN && text.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase())
}

fn validate_path(path: &str) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("file must not be empty".into());
    }
    if !path.starts_with('/') {
        return Err(format!("file '{path}' must be an absolute path"));
    }
    if path.len() > MAX_PATH_LEN {
        return Err(format!("file path is {} bytes, the limit is {MAX_PATH_LEN}", path.len()));
    }
    if let Some(bad) = path
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '@' | ':' | ',' | '=')))
    {
        return Err(format!(
            "file '{path}' contains '{}': only ASCII letters, digits and / . _ - + @ : , = \
             are allowed, so the path needs no quoting on the host",
            bad.escape_default()
        ));
    }
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "3b5d5c3712955042212316173ccf37be800a0e0fa8a1b5b1e1a5b8c4b0a1f2e3";

    fn baseline(path: &str) -> Result<FileBaseline, String> {
        FileBaseline::new(path, FileReference::Host("web-1".into()))
    }

    #[test]
    fn the_command_hashes_and_never_prints_the_file() {
        let check = baseline("/etc/nginx/nginx.conf").unwrap();
        assert_eq!(
            check.command(),
            "stat -c present /etc/nginx/nginx.conf 2>/dev/null && sha256sum /etc/nginx/nginx.conf"
        );
        assert!(!check.command().contains("cat"));
    }

    #[test]
    fn paths_that_need_quoting_or_are_relative_are_refused() {
        for path in ["", "etc/hosts", "/etc/my file", "/etc/*.conf", "/etc/$HOME", "/etc/a;rm", "/etc/a'b", "/etc/\u{e9}"] {
            assert!(baseline(path).is_err(), "{path:?} must be refused");
        }
        assert!(baseline("/etc/nginx/conf.d/site-1_a+b@c:d,e=f.conf").is_ok());
    }

    #[test]
    fn a_sha256_reference_is_validated_and_lowercased() {
        let upper = HASH.to_ascii_uppercase();
        let check = FileBaseline::new("/etc/hosts", FileReference::Sha256(upper)).unwrap();
        assert_eq!(check.reference(), &FileReference::Sha256(HASH.into()));
        assert!(FileBaseline::new("/etc/hosts", FileReference::Sha256("abc".into())).is_err());
        assert!(FileBaseline::new("/etc/hosts", FileReference::Host("  ".into())).is_err());
    }

    #[test]
    fn a_present_file_parses_to_its_digest_only() {
        let out = format!("present\n{HASH}  /etc/hosts\n");
        assert_eq!(FileProbe::parse(&out, Some(0)), Some(FileProbe::Present(HASH.into())));
    }

    #[test]
    fn a_missing_file_is_a_state_not_an_error() {
        assert_eq!(FileProbe::parse("", Some(1)), Some(FileProbe::Missing));
    }

    #[test]
    fn an_existing_file_that_cannot_be_hashed_is_unreadable() {
        assert_eq!(FileProbe::parse("present\n", Some(1)), Some(FileProbe::Unreadable));
    }

    #[test]
    fn anything_else_is_unrecognised() {
        let cases = [
            ("", Some(0)),
            ("", None),
            ("present\n", Some(0)),
            ("server { listen 80; }\n", Some(0)),
            ("present\nnot-a-hash  /etc/hosts\n", Some(0)),
            (&*format!("present\n{HASH}  /etc/hosts\nextra\n"), Some(0)),
        ];
        for (out, code) in cases {
            assert_eq!(FileProbe::parse(out, code), None, "{out:?} / {code:?}");
        }
    }
}
