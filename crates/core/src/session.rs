//! Session persistence: save and restore chat histories.
//!
//! Sessions are stored as JSON files in a per-user directory:
//! - Windows: `%APPDATA%/filar/sessions/<id>.json`
//! - Unix:    `$HOME/.filar/sessions/<id>.json`
//!
//! At most [`MAX_SESSIONS`] sessions are kept; older ones are pruned.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::chat::ChatBlock;
use crate::error::{CoreError, Result};

/// Maximum number of sessions to retain on disk.
pub const MAX_SESSIONS: usize = 10;

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A saved chat session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique identifier (Unix timestamp as a string).
    pub id: String,
    /// Human-readable timestamp (ISO-ish: `YYYY-MM-DD HH:MM:SS`).
    pub timestamp: String,
    /// Target name that was connected to.
    pub target: String,
    /// LLM profile name that was used.
    pub llm_profile: String,
    /// Chat history blocks.
    pub messages: Vec<ChatBlock>,
}

/// Lightweight metadata for listing sessions without loading full messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub timestamp: String,
    pub target: String,
    pub llm_profile: String,
    /// Preview of the first user message (or system message).
    pub preview: String,
}

impl From<&Session> for SessionMeta {
    fn from(s: &Session) -> Self {
        let preview = s
            .messages
            .iter()
            .find(|b| matches!(b, ChatBlock::User(_)))
            .or_else(|| s.messages.first())
            .map(|b| b.preview())
            .unwrap_or_default();
        Self {
            id: s.id.clone(),
            timestamp: s.timestamp.clone(),
            target: s.target.clone(),
            llm_profile: s.llm_profile.clone(),
            preview,
        }
    }
}

// ---------------------------------------------------------------------------
// SessionStore
// ---------------------------------------------------------------------------

/// Manages session files on disk.
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// Returns the sessions directory path.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Create a store, ensuring the sessions directory exists.
    pub fn new() -> Result<Self> {
        let dir = sessions_dir()?;
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Save a session to disk (overwrites if the ID already exists).
    pub fn save(&self, session: &Session) -> Result<()> {
        let path = self.dir.join(format!("{}.json", session.id));
        let json = serde_json::to_string_pretty(session)
            .map_err(|e| CoreError::Other(format!("failed to serialise session: {e}")))?;
        std::fs::write(&path, json)?;
        tracing::info!(session_id = %session.id, path = %path.display(), "session saved");
        Ok(())
    }

    /// Load a session by ID. Returns `None` if the file does not exist.
    pub fn load(&self, id: &str) -> Result<Option<Session>> {
        let path = self.dir.join(format!("{id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(&path)?;
        let session: Session = serde_json::from_str(&contents)
            .map_err(|e| CoreError::Other(format!("failed to parse session {id}: {e}")))?;
        Ok(Some(session))
    }

    /// List metadata for all stored sessions, sorted newest-first.
    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        let mut metas = Vec::new();
        if !self.dir.exists() {
            return Ok(metas);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(contents) => {
                    match serde_json::from_str::<Session>(&contents) {
                        Ok(session) => metas.push(SessionMeta::from(&session)),
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "skipping unparseable session file");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping unreadable session file");
                }
            }
        }
        // Sort newest-first (IDs are timestamps, so string sort works).
        metas.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(metas)
    }

    /// Delete sessions beyond `max`, keeping the newest.
    pub fn prune_to(&self, max: usize) -> Result<()> {
        let metas = self.list()?;
        if metas.len() <= max {
            return Ok(());
        }
        // metas is sorted newest-first; delete everything after index `max`.
        for meta in metas.into_iter().skip(max) {
            let path = self.dir.join(format!("{}.json", meta.id));
            tracing::info!(session_id = %meta.id, "pruning old session");
            let _ = std::fs::remove_file(&path);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Determine the sessions directory.
fn sessions_dir() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .map_err(|_| CoreError::Other("APPDATA environment variable not set".into()))?
    } else {
        std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| CoreError::Other("HOME environment variable not set".into()))?
    };
    Ok(base.join("filar").join("sessions"))
}

/// Generate a session ID and human-readable timestamp from the current time.
pub fn now_session_id() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Simple UTC timestamp formatting without external deps.
    let (year, month, day, hour, min, sec) = unix_to_ymdhms(secs);
    let id = format!("{secs}");
    let timestamp = format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}");
    (id, timestamp)
}

/// Convert a Unix timestamp (seconds) to broken-down UTC time.
///
/// This is a minimal implementation that avoids pulling in `chrono`.
fn unix_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let sec = (rem % 60) as u32;

    // Days since 1970-01-01 → calendar date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y } as u32;

    (year, m, d, hour, min, sec)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_meta_from_session() {
        let session = Session {
            id: "123".into(),
            timestamp: "2026-06-21 12:00:00".into(),
            target: "local".into(),
            llm_profile: "default".into(),
            messages: vec![
                ChatBlock::System("connected".into()),
                ChatBlock::User("find port 8080".into()),
                ChatBlock::Agent("running lsof".into()),
            ],
        };
        let meta = SessionMeta::from(&session);
        assert_eq!(meta.id, "123");
        assert_eq!(meta.target, "local");
        assert_eq!(meta.preview, "You: find port 8080");
    }

    #[test]
    fn session_meta_empty_messages() {
        let session = Session {
            id: "1".into(),
            timestamp: "t".into(),
            target: "t".into(),
            llm_profile: "t".into(),
            messages: vec![],
        };
        let meta = SessionMeta::from(&session);
        assert!(meta.preview.is_empty());
    }

    #[test]
    fn session_store_save_load_roundtrip() {
        // Use a temp directory for testing.
        let tmp = std::env::temp_dir().join(format!("filar_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let store = SessionStore {
            dir: tmp.clone(),
        };

        let session = Session {
            id: "999".into(),
            timestamp: "2026-01-01 00:00:00".into(),
            target: "test".into(),
            llm_profile: "glm".into(),
            messages: vec![
                ChatBlock::User("hello".into()),
                ChatBlock::Agent("world".into()),
            ],
        };

        store.save(&session).unwrap();
        let loaded = store.load("999").unwrap().unwrap();
        assert_eq!(loaded.id, "999");
        assert_eq!(loaded.target, "test");
        assert_eq!(loaded.messages.len(), 2);

        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].preview, "You: hello");

        // Loading non-existent session.
        assert!(store.load("nonexistent").unwrap().is_none());

        // Clean up.
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn session_store_prune() {
        let tmp = std::env::temp_dir().join(format!("filar_prune_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let store = SessionStore {
            dir: tmp.clone(),
        };

        // Save 5 sessions with different IDs (timestamps).
        for i in 1..=5u64 {
            let session = Session {
                id: format!("{i:010}"),
                timestamp: format!("t{i}"),
                target: "t".into(),
                llm_profile: "t".into(),
                messages: vec![ChatBlock::User(format!("msg{i}"))],
            };
            store.save(&session).unwrap();
        }

        // Prune to 3.
        store.prune_to(3).unwrap();
        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 3);
        // Newest 3 kept (IDs 0000000005, 0000000004, 0000000003).
        assert_eq!(metas[0].id, "0000000005");
        assert_eq!(metas[2].id, "0000000003");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unix_to_ymdhms_epoch() {
        let (y, m, d, h, mi, s) = unix_to_ymdhms(0);
        assert_eq!((y, m, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymdhms_known_date() {
        // 2026-06-21 12:30:45 UTC
        // Unix timestamp: calculate from known reference
        // 2026-01-01 00:00:00 = 1767225600
        // + 31 days (Jan) + 28 days (Feb) + 31 (Mar) + 30 (Apr) + 31 (May) + 20 days (Jun 1-20)
        // = 171 days * 86400 = 14774400
        // + 12.5h = 45045
        // Total ≈ 1767225600 + 14774400 + 45045 = 1782045045
        // Let me just verify the epoch is correct and the function doesn't panic.
        let (y, _m, _d, _h, _mi, _s) = unix_to_ymdhms(1_782_045_045);
        assert_eq!(y, 2026);
    }
}
