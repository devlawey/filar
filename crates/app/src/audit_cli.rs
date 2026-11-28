//! `filar audit verify` — read-only integrity check of the audit journal (#697).
//!
//! Reads journal files, recomputes the hash chain and reports the first
//! corrupted/deleted/reordered record plus a totals line. Never writes
//! to the journal. Exit codes: 0 — no errors (including "chain disabled"),
//! 1 — corruption or unreadable journal, 2 — usage/OS error.

use filar_core::audit::{self, ChainOutcome};
use std::path::{Path, PathBuf};

/// Handle `filar audit …` when argv starts with `audit`; returns the
/// process exit code. `None` means "not an audit invocation" — the
/// normal GUI/TUI startup proceeds.
pub fn maybe_run() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("audit") {
        return None;
    }
    Some(run(&args))
}

const USAGE: &str = "Usage: filar audit verify [--path <file>]";

/// Run the `audit` subcommand over already-split args (`args[0] == "audit"`).
fn run(args: &[String]) -> i32 {
    if args.get(1).map(String::as_str) != Some("verify") {
        eprintln!("unknown audit subcommand");
        eprintln!("{USAGE}");
        return 2;
    }
    let mut path: Option<PathBuf> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                let Some(value) = args.get(i + 1) else {
                    eprintln!("--path requires a value");
                    eprintln!("{USAGE}");
                    return 2;
                };
                path = Some(PathBuf::from(value));
                i += 2;
            }
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    match path {
        Some(p) => verify_files(&[p]),
        None => {
            let dir = match audit::default_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    eprintln!("{e}");
                    return 2;
                }
            };
            let files = match journal_files(&dir) {
                Ok(files) => files,
                Err(e) => {
                    eprintln!("cannot read audit directory {}: {e}", dir.display());
                    return 2;
                }
            };
            if files.is_empty() {
                println!("no audit journals found in {}", dir.display());
                return 0;
            }
            verify_files(&files)
        }
    }
}

/// All `audit-*.jsonl` files of a directory, sorted by name. A missing
/// directory is simply "no journals yet".
fn journal_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("audit-") && name.ends_with(".jsonl") {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

/// Verify each file, print per-file verdicts and the totals line;
/// returns the exit code (0 clean / 1 problems).
fn verify_files(files: &[PathBuf]) -> i32 {
    let mut total = 0usize;
    let mut problems = 0usize;
    for file in files {
        let records = match audit::read_records(file) {
            Ok(records) => records,
            Err(e) => {
                // A malformed line names its number — that is corruption too.
                println!("{}: ERROR {e}", file.display());
                problems += 1;
                continue;
            }
        };
        total += records.len();
        match audit::verify_chain(&records) {
            Ok(report) => match report.outcome {
                ChainOutcome::Disabled => println!(
                    "{}: {} records checked, chain disabled",
                    file.display(),
                    report.records_checked
                ),
                ChainOutcome::Intact => println!(
                    "{}: {} records checked, chain OK",
                    file.display(),
                    report.records_checked
                ),
                ChainOutcome::Broken { line, reason } => {
                    println!("{}: BROKEN at line {line}: {reason}", file.display());
                    problems += 1;
                }
            },
            Err(e) => {
                println!("{}: ERROR {e}", file.display());
                problems += 1;
            }
        }
    }
    if problems == 0 {
        println!("{} file(s), {total} records checked, no errors", files.len());
        0
    } else {
        println!("{} file(s), {total} records checked, {problems} problem(s) found", files.len());
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filar_core::{AuditDecision, AuditDecisionSource, AuditRecord, AuditTarget, AuditWriter};

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("filar_audit_cli_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn record(command: &str) -> AuditRecord {
        let mut r = AuditRecord::new(
            "sess-cli",
            AuditTarget { kind: "local".into(), name: "localhost".into(), host: None, tags: vec![] },
            command,
            AuditDecision::Approved,
            AuditDecisionSource::User,
        );
        r.ts = "2026-01-02T03:04:05Z".into();
        r
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn intact_chain_exits_zero() {
        let dir = tmp_dir("ok");
        let mut w = AuditWriter::new(&dir).with_hash_chain(true);
        let path = w.append(&record("uptime"), None).unwrap();
        w.append(&record("df -h"), None).unwrap();
        let code = run(&args(&["audit", "verify", "--path", &path.to_string_lossy()]));
        assert_eq!(code, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_chain_exits_one() {
        let dir = tmp_dir("tamper");
        let mut w = AuditWriter::new(&dir).with_hash_chain(true);
        let path = w.append(&record("uptime"), None).unwrap();
        w.append(&record("df -h"), None).unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = data.lines().map(str::to_string).collect();
        let mut rec = AuditRecord::parse_line(&lines[1]).unwrap();
        rec.command = "rm -rf /".into();
        lines[1] = serde_json::to_string(&rec).unwrap();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let code = run(&args(&["audit", "verify", "--path", &path.to_string_lossy()]));
        assert_eq!(code, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unchained_journal_exits_zero_as_disabled() {
        let dir = tmp_dir("disabled");
        let mut w = AuditWriter::new(&dir);
        let path = w.append(&record("uptime"), None).unwrap();
        let code = run(&args(&["audit", "verify", "--path", &path.to_string_lossy()]));
        assert_eq!(code, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_exits_one() {
        let dir = tmp_dir("missing");
        let path = dir.join("audit-2026-01-02.jsonl");
        let code = run(&args(&["audit", "verify", "--path", &path.to_string_lossy()]));
        assert_eq!(code, 1, "an unreadable journal is a problem, not a pass");
    }

    #[test]
    fn bad_usage_exits_two() {
        assert_eq!(run(&args(&["audit"])), 2);
        assert_eq!(run(&args(&["audit", "frobnicate"])), 2);
        assert_eq!(run(&args(&["audit", "verify", "--path"])), 2);
        assert_eq!(run(&args(&["audit", "verify", "--bogus"])), 2);
    }

    #[test]
    fn journal_files_filters_and_sorts() {
        let dir = tmp_dir("list");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("audit-2026-01-03.jsonl"), "").unwrap();
        std::fs::write(dir.join("audit-2026-01-02.jsonl"), "").unwrap();
        std::fs::write(dir.join("audit-2026-01-02.1.jsonl"), "").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        let files = journal_files(&dir).unwrap();
        let names: Vec<String> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["audit-2026-01-02.1.jsonl", "audit-2026-01-02.jsonl", "audit-2026-01-03.jsonl"]);
        assert!(journal_files(&dir.join("nope")).unwrap().is_empty(), "missing dir = no journals");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_is_read_only() {
        let dir = tmp_dir("readonly");
        let mut w = AuditWriter::new(&dir).with_hash_chain(true);
        let path = w.append(&record("uptime"), None).unwrap();
        let before = std::fs::read(&path).unwrap();
        let code = run(&args(&["audit", "verify", "--path", &path.to_string_lossy()]));
        assert_eq!(code, 0);
        assert_eq!(std::fs::read(&path).unwrap(), before, "verification must not touch the file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}