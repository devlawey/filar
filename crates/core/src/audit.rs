//! Local append-only audit journal (#696).
//!
//! Every confirmed command can be recorded as one JSONL line: what ran,
//! who decided, the safety rating and the arbiter verdict. The journal
//! lives only on the user's machine — nothing is sent anywhere
//! (invariant 5). Secret values are masked before a line is written.
//!
//! Files are named `audit-<YYYY-MM-DD>.jsonl` (date taken from the record
//! timestamp) under [`default_dir`]; when a file would grow past the size
//! limit the writer rolls over to `audit-<YYYY-MM-DD>.<n>.jsonl`. Existing
//! files are only ever appended to, never rewritten.

use crate::error::{CoreError, Result};
use crate::secrets::{redact_secrets, SecretProvider};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Default size limit for one journal file before rollover (10 MiB).
pub const DEFAULT_AUDIT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// What happened to the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditDecision {
    /// The user approved the command.
    Approved,
    /// The user denied the command.
    Denied,
    /// No confirmation was required (confirm mode / allowlist).
    Auto,
    /// The command was cancelled before a decision (timeout, Ctrl-C).
    Cancelled,
}

/// Where the decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditDecisionSource {
    /// The user answered the confirmation dialog.
    User,
    /// A confirm-mode allowlist matched.
    Allowlist,
    /// A never-confirm policy allowed the command.
    Never,
    /// A group/target policy decided (e.g. read-only refusal).
    Policy,
}

/// Which target the command ran against.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditTarget {
    /// Target kind: `local`, `ssh`, `fleet`, `kube`, …
    pub kind: String,
    /// Display name (target name, group name, cluster name).
    pub name: String,
    /// Host address when applicable.
    #[serde(default)]
    pub host: Option<String>,
    /// Tags of the target at the moment of the decision.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// One audited command decision, serialized as a single JSONL line.
///
/// Optional fields are `None` when the information was not available
/// (no scorer configured, command never ran, …). The hash-chain fields
/// arrive with #697; readers must tolerate their absence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// UTC RFC 3339 timestamp of the decision.
    pub ts: String,
    /// Session identifier (as in the session export).
    pub session_id: String,
    /// Target descriptor.
    pub target: AuditTarget,
    /// The command line, secrets masked.
    pub command: String,
    /// The agent's explanation, secrets masked.
    #[serde(default)]
    pub explanation: String,
    /// Whether the command was flagged destructive.
    #[serde(default)]
    pub destructive: bool,
    /// The decision itself.
    pub decision: AuditDecision,
    /// Who/what decided.
    pub decision_source: AuditDecisionSource,
    /// Safety rating 0–100 (100 = safe), when a scorer answered.
    #[serde(default)]
    pub risk_score: Option<u8>,
    /// Arbiter verdict label (`AGREE`, `MISMATCH`, …), when audited.
    #[serde(default)]
    pub arbiter_verdict: Option<String>,
    /// Arbiter reason, secrets masked.
    #[serde(default)]
    pub arbiter_reason: Option<String>,
    /// Arbiter model slug.
    #[serde(default)]
    pub arbiter_model: Option<String>,
    /// Exit code, when the command actually ran.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Execution duration in milliseconds, when it ran.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// `proposal_id` of the originating `CommandProposed`, when known.
    #[serde(default)]
    pub proposal_id: Option<u64>,
}

impl AuditRecord {
    /// A record stamped with the current UTC time; optional fields empty.
    pub fn new(
        session_id: impl Into<String>,
        target: AuditTarget,
        command: impl Into<String>,
        decision: AuditDecision,
        decision_source: AuditDecisionSource,
    ) -> Self {
        Self {
            ts: now_rfc3339(),
            session_id: session_id.into(),
            target,
            command: command.into(),
            explanation: String::new(),
            destructive: false,
            decision,
            decision_source,
            risk_score: None,
            arbiter_verdict: None,
            arbiter_reason: None,
            arbiter_model: None,
            exit_code: None,
            duration_ms: None,
            proposal_id: None,
        }
    }

    /// Parse one JSONL line back into a record.
    pub fn parse_line(line: &str) -> Result<Self> {
        serde_json::from_str(line)
            .map_err(|e| CoreError::Other(format!("invalid audit record line: {e}")))
    }
}

/// Current UTC time as RFC 3339 (second precision, `Z` suffix).
///
/// Uses the same dependency-free conversion as the session store — core
/// deliberately does not pull in `chrono`.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day, hour, min, sec) = crate::session::unix_to_ymdhms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Default journal directory: `{data_local_dir}/filar/audit`.
pub fn default_dir() -> Result<PathBuf> {
    let base = dirs::data_local_dir().ok_or_else(|| {
        CoreError::Other(
            "could not determine OS local data directory (dirs::data_local_dir returned None)".into(),
        )
    })?;
    Ok(base.join("filar").join("audit"))
}

/// Read every record of one journal file, in file order.
///
/// Empty lines are skipped; a malformed line is an error naming its
/// 1-based number — a journal with a hole must not read as intact.
pub fn read_records(path: &Path) -> Result<Vec<AuditRecord>> {
    let data = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (i, line) in data.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: AuditRecord = serde_json::from_str(line).map_err(|e| {
            CoreError::Other(format!("{} line {}: {e}", path.display(), i + 1))
        })?;
        out.push(rec);
    }
    Ok(out)
}

/// Append-only JSONL writer with date/size rollover.
///
/// The writer never truncates or rewrites an existing file: it opens with
/// `append` and rolls over to a new indexed file when the size limit would
/// be exceeded. A write failure is returned as an error — the caller logs
/// a warning and carries on; auditing must never block a command.
pub struct AuditWriter {
    dir: PathBuf,
    max_bytes: u64,
    /// Currently open file: path, handle, bytes written so far.
    current: Option<(PathBuf, std::fs::File, u64)>,
}

impl AuditWriter {
    /// Writer for an explicit directory with the default size limit.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_max_bytes(dir, DEFAULT_AUDIT_MAX_BYTES)
    }

    /// Writer with an explicit rollover size (bytes).
    pub fn with_max_bytes(dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self { dir: dir.into(), max_bytes: max_bytes.max(1), current: None }
    }

    /// Writer for [`default_dir`].
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(default_dir()?))
    }

    /// The journal directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one record; returns the file it landed in.
    ///
    /// When `secrets` is given, known secret values are masked in the
    /// command, explanation and arbiter reason before serialization —
    /// a value never reaches the disk.
    pub fn append(
        &mut self,
        record: &AuditRecord,
        secrets: Option<&dyn SecretProvider>,
    ) -> Result<PathBuf> {
        let mut clean = record.clone();
        if let Some(provider) = secrets {
            clean.command = redact_secrets(&clean.command, provider);
            clean.explanation = redact_secrets(&clean.explanation, provider);
            if let Some(reason) = &clean.arbiter_reason {
                clean.arbiter_reason = Some(redact_secrets(reason, provider));
            }
        }
        let mut line = serde_json::to_string(&clean)
            .map_err(|e| CoreError::Other(format!("failed to serialize audit record: {e}")))?;
        line.push('\n');

        std::fs::create_dir_all(&self.dir)?;
        let date = date_of_ts(&clean.ts);
        let path = self.pick_file(&date, line.len() as u64)?;
        self.write_line(&path, line.as_bytes())?;
        Ok(path)
    }

    /// Choose the file for `date`: the cached one while it fits, otherwise
    /// the first `audit-<date>[.<n>].jsonl` with room for `extra` bytes.
    fn pick_file(&mut self, date: &str, extra: u64) -> Result<PathBuf> {
        if let Some((path, _, size)) = &self.current {
            if file_date(path) == date && size.saturating_add(extra) <= self.max_bytes {
                return Ok(path.clone());
            }
        }
        let base = self.dir.join(format!("audit-{date}.jsonl"));
        let mut candidate = base.clone();
        let mut index = 0u32;
        loop {
            let size = std::fs::metadata(&candidate).map(|m| m.len()).unwrap_or(0);
            // A brand-new file accepts any single line, even an oversized
            // one — a record must never be dropped for being long.
            if size == 0 || size.saturating_add(extra) <= self.max_bytes {
                return Ok(candidate);
            }
            index += 1;
            candidate = self.dir.join(format!("audit-{date}.{index}.jsonl"));
        }
    }

    /// Open (append-only) and write, keeping the handle for the next line.
    fn write_line(&mut self, path: &Path, bytes: &[u8]) -> Result<()> {
        let reuse = self.current.as_ref().map(|(p, _, _)| p == path).unwrap_or(false);
        if !reuse {
            let file = std::fs::OpenOptions::new().append(true).create(true).open(path)?;
            let size = file.metadata()?.len();
            self.current = Some((path.to_path_buf(), file, size));
        }
        let Some((path_buf, file, size)) = self.current.as_mut() else {
            return Err(CoreError::Other("audit writer lost its open file".into()));
        };
        if let Err(e) = file.write_all(bytes).and_then(|_| file.flush()) {
            // Drop the handle so the next append reopens — the failed
            // write must not poison the cached state.
            let path_disp = path_buf.display().to_string();
            self.current = None;
            return Err(CoreError::Other(format!("failed to append audit record to {path_disp}: {e}")));
        }
        *size += bytes.len() as u64;
        Ok(())
    }
}

/// `YYYY-MM-DD` prefix of an RFC 3339 timestamp; falls back to today for
/// a malformed value so a record is never lost over a bad clock string.
fn date_of_ts(ts: &str) -> String {
    if ts.len() >= 10 && ts.as_bytes()[4] == b'-' && ts.as_bytes()[7] == b'-' {
        ts[..10].to_string()
    } else {
        now_rfc3339()[..10].to_string()
    }
}

/// Date a journal file belongs to, from its `audit-<date>[.<n>].jsonl` name.
fn file_date(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let rest = name.strip_prefix("audit-").unwrap_or(&name);
    rest.get(..10).unwrap_or(rest).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::StaticSecretProvider;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("filar_audit_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn record(ts: &str, command: &str) -> AuditRecord {
        let mut r = AuditRecord::new(
            "sess-1",
            AuditTarget { kind: "ssh".into(), name: "web-1".into(), host: Some("10.0.0.1".into()), tags: vec!["prod".into()] },
            command,
            AuditDecision::Approved,
            AuditDecisionSource::User,
        );
        r.ts = ts.into();
        r
    }

    #[test]
    fn record_jsonl_roundtrip() {
        let mut r = record("2026-01-02T03:04:05Z", "systemctl restart nginx");
        r.explanation = "restart the web server".into();
        r.destructive = true;
        r.risk_score = Some(42);
        r.arbiter_verdict = Some("MISMATCH".into());
        r.arbiter_reason = Some("restart is riskier than stated".into());
        r.arbiter_model = Some("arbiter/model".into());
        r.exit_code = Some(0);
        r.duration_ms = Some(1200);
        r.proposal_id = Some(7);

        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains('\n'), "JSONL must be one line");
        let back = AuditRecord::parse_line(&line).unwrap();
        assert_eq!(back, r);
        // Decision enums serialize as the lowercase labels from the issue.
        assert!(line.contains("\"decision\":\"approved\""));
        assert!(line.contains("\"decision_source\":\"user\""));
    }

    #[test]
    fn append_preserves_existing_lines() {
        let dir = tmp_dir("preserve");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("audit-2026-01-02.jsonl");
        std::fs::write(&file, "{\"sentinel\":true}\n").unwrap();

        let mut w = AuditWriter::new(&dir);
        let path = w.append(&record("2026-01-02T03:04:05Z", "uptime"), None).unwrap();
        assert_eq!(path, file, "same-date record appends to the existing file");

        let data = std::fs::read_to_string(&file).unwrap();
        let mut lines = data.lines();
        assert_eq!(lines.next().unwrap(), "{\"sentinel\":true}", "existing line must survive verbatim");
        let rec = AuditRecord::parse_line(lines.next().unwrap()).unwrap();
        assert_eq!(rec.command, "uptime");
        assert_eq!(lines.next(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_by_size_keeps_all_records() {
        let dir = tmp_dir("rotsize");
        let mut w = AuditWriter::with_max_bytes(&dir, 300);
        let mut paths = Vec::new();
        for i in 0..6 {
            let p = w.append(&record("2026-01-02T03:04:05Z", &format!("echo command-number-{i}")), None).unwrap();
            paths.push(p);
        }
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert!(unique.len() >= 2, "size limit must roll over to a new file");
        let total: usize = unique.iter().map(|p| read_records(p).unwrap().len()).sum();
        assert_eq!(total, 6, "no record may be lost on rollover");
        for p in &unique {
            assert!(std::fs::metadata(p).unwrap().len() <= 300 + 200, "files stay near the limit");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_by_date_uses_record_timestamp() {
        let dir = tmp_dir("rotdate");
        let mut w = AuditWriter::new(&dir);
        let p1 = w.append(&record("2026-01-02T23:59:59Z", "uptime"), None).unwrap();
        let p2 = w.append(&record("2026-01-03T00:00:01Z", "uptime"), None).unwrap();
        assert_ne!(p1, p2, "a new date starts a new file");
        assert!(p1.to_string_lossy().contains("audit-2026-01-02"));
        assert!(p2.to_string_lossy().contains("audit-2026-01-03"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_values_are_masked_before_write() {
        let dir = tmp_dir("mask");
        let provider = StaticSecretProvider::with_secrets(
            [("$FILAR_SECRET_TOKEN".to_string(), "hunter2-super-secret".to_string())].into(),
        );
        let mut r = record("2026-01-02T03:04:05Z", "curl -H 'Authorization: Bearer hunter2-super-secret' https://api.example.com");
        r.explanation = "call the API with hunter2-super-secret".into();
        r.arbiter_reason = Some("token hunter2-super-secret visible".into());

        let mut w = AuditWriter::new(&dir);
        let path = w.append(&r, Some(&provider)).unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(!data.contains("hunter2-super-secret"), "secret value must never reach the journal");
        let rec = AuditRecord::parse_line(data.lines().next().unwrap()).unwrap();
        assert!(rec.command.contains("FILAR_SECRET_TOKEN"), "mask names the secret slot");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_error_is_returned_not_panicked() {
        // A regular file where the directory should be: create_dir_all
        // fails, append must return an error — the caller keeps running.
        let dir = tmp_dir("err");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::write(&dir, "not a directory").unwrap();
        let mut w = AuditWriter::new(&dir);
        let result = w.append(&record("2026-01-02T03:04:05Z", "uptime"), None);
        assert!(result.is_err(), "an unwritable journal is an error, not a panic");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn read_records_names_a_bad_line() {
        let dir = tmp_dir("badline");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("audit-2026-01-02.jsonl");
        let good = serde_json::to_string(&record("2026-01-02T03:04:05Z", "uptime")).unwrap();
        std::fs::write(&file, format!("{good}\nnot json\n")).unwrap();
        let err = read_records(&file).unwrap_err().to_string();
        assert!(err.contains("line 2"), "error names the malformed line: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}