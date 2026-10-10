//! What a person sees of a fleet operation (#438).
//!
//! The model gets the fold (#430): who agreed with whom, and — for an
//! ad-hoc command — short answers quoted (#505) and digests for the rest.
//! A digest tells a person nothing, though. They want to read the answer:
//! "eleven hosts say 5.15.0-91, one says 6.1.0". This module builds that
//! view from the same operation — groups of agreeing hosts, each with a
//! sample of what they answered, and the hosts that have no value at all.
//!
//! **It is for the UI only.** It carries host output, so it travels on its
//! own path (the executor's observer) straight to the panel, and never
//! into a tool result, a transcript or the model's context. The fold stays
//! the only thing the agent loop sees.
//!
//! The view is an **aggregate**, not a stream: one sample per group of
//! identical answers, not twelve outputs side by side (развилки 45, 46).

use filar_core::fleet_op::{FleetOperation, OperationId};

use crate::fleet_fold::{ComparedValue, FoldedTable};
use crate::fleet_result::{HostState, OperationSummary};
use crate::fleet_run::{FleetRunReport, HostRun};

/// How many hosts of a fleet are answering (#439) — for the status bar.
///
/// "Answering" means the host answered its last command, well or badly: a
/// host that ran the command and exited non-zero is alive; one that timed
/// out, could not be reached, was skipped or was cancelled is not. While an
/// operation runs the count starts at zero and grows as answers arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetStatus {
    /// The operation the count is about.
    pub operation: OperationId,
    /// Hosts that answered in it so far.
    pub answering: usize,
    /// Every member of the fleet, skipped ones included.
    pub total: usize,
    /// Whether the operation is still running.
    pub running: bool,
}

/// Longest sample kept per group, in characters. The panel shows a line
/// or a few; this only bounds memory for a host that printed megabytes.
pub const MAX_SAMPLE_CHARS: usize = 16 * 1024;

/// How a group stands against the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupRole {
    /// The largest group — what "normal" looks like in this fleet.
    Baseline,
    /// Differs from the baseline.
    Differs,
    /// No strict majority: the fleet is split, and no side is normal.
    Split,
}

/// One set of hosts that answered the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewGroup {
    /// The hosts, in the operation's composition order.
    pub hosts: Vec<String>,
    /// What they answered: the first host's output for a raw comparison,
    /// the compared rows for a typed one.
    pub sample: String,
    /// Against the rest of the fleet.
    pub role: GroupRole,
}

/// A host with no value in the comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDropped {
    /// The host's configured name.
    pub host: String,
    /// Why: no contact, timed out, not applicable, skipped, cancelled, or
    /// an execution error.
    pub state: HostState,
}

/// The person-facing aggregate of one fleet operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetView {
    /// The operation.
    pub operation: OperationId,
    /// The command every host was asked.
    pub command: String,
    /// The summary headline — the same line the model gets first.
    pub headline: String,
    /// Groups of agreeing hosts, largest first.
    pub groups: Vec<ViewGroup>,
    /// Hosts without a value, in composition order.
    pub dropped: Vec<ViewDropped>,
}

impl FleetView {
    /// Build the view of `op` from the fan-out `report`, the `table` folded
    /// from it and its `summary`.
    pub fn build(
        op: &FleetOperation,
        command: &str,
        report: &FleetRunReport,
        table: &FoldedTable,
        summary: &OperationSummary,
    ) -> Self {
        let output_of = |name: &str| -> String {
            op.handles()
                .zip(op.members())
                .find(|(_, m)| m.name() == name)
                .and_then(|(h, _)| report.run_for(h))
                .map(|run| match run {
                    HostRun::Answered(result) => result.stdout.trim_end().to_string(),
                    _ => String::new(),
                })
                .unwrap_or_default()
        };
        let baseline = table.baseline().map(|b| b.hosts().to_vec());
        let groups = table
            .groups()
            .iter()
            .map(|group| {
                let role = match &baseline {
                    Some(hosts) if hosts == group.hosts() => GroupRole::Baseline,
                    Some(_) => GroupRole::Differs,
                    None => GroupRole::Split,
                };
                let sample = match group.value() {
                    ComparedValue::Rows(rows) => {
                        clamp(&rows.iter().map(|r| r.join("  ")).collect::<Vec<_>>().join("\n"))
                    }
                    // A short answer is plain text by construction.
                    ComparedValue::Text(_) => group
                        .hosts()
                        .first()
                        .map(|h| clamp(&output_of(h)))
                        .unwrap_or_default(),
                    ComparedValue::Digest(digest) => group
                        .hosts()
                        .first()
                        .map(|h| clamp(&readable_sample(&output_of(h), *digest)))
                        .unwrap_or_default(),
                };
                ViewGroup {
                    hosts: group.hosts().to_vec(),
                    sample,
                    role,
                }
            })
            .collect();
        let dropped = table
            .dropped()
            .iter()
            .map(|d| ViewDropped {
                host: d.host().to_string(),
                state: d.state(),
            })
            .collect();
        Self {
            operation: op.id(),
            command: command.to_string(),
            headline: summary.headline(),
            groups,
            dropped,
        }
    }
}

/// How much of an answer is looked at to tell text from binary data.
const BINARY_SNIFF_CHARS: usize = 4096;

/// Whether a host's answer is binary data rather than text (#502): `cat` of
/// an archive or an executable, decoded lossily on the way here.
///
/// A NUL settles it. Otherwise it is binary when at least four and more
/// than a tenth of the first [`BINARY_SNIFF_CHARS`] characters are
/// undecodable bytes (U+FFFD) or control characters no text output carries
/// — the floor of four keeps a log with a stray bad byte or two a log. Line breaks, tabs and the
/// escape sequences of coloured output do not count, and any script —
/// Cyrillic, CJK — is text.
pub fn looks_binary(output: &str) -> bool {
    let (mut seen, mut odd) = (0usize, 0usize);
    for c in output.chars().take(BINARY_SNIFF_CHARS) {
        if c == '\0' {
            return true;
        }
        seen += 1;
        let layout = matches!(c, '\n' | '\r' | '\t' | '\u{1b}' | '\u{8}' | '\u{c}' | '\u{7}');
        if c == '\u{fffd}' || (c.is_control() && !layout) {
            odd += 1;
        }
    }
    odd >= 4 && odd * 10 > seen
}

/// The sample a person is shown for a raw answer: the text itself, or — for
/// binary data — one line saying so, with its size and the digest the
/// summary compared (the same number the agent is given), so "is this file
/// the same on every host" still reads off the panel.
fn readable_sample(output: &str, digest: u64) -> String {
    if looks_binary(output) {
        format!(
            "binary data, about {}, digest {digest:#018x} (not shown)",
            human_size(output.len())
        )
    } else {
        output.to_string()
    }
}

/// A byte count as a person reads it. Kilobytes are rounded up: the size is
/// shown as "about", and a 100-byte file is not "0 KB".
fn human_size(bytes: usize) -> String {
    const KB: usize = 1024;
    match bytes {
        b if b < KB => format!("{b} B"),
        b if b < KB * KB => format!("{} KB", b.div_ceil(KB)),
        b => format!("{:.1} MB", b as f64 / (KB * KB) as f64),
    }
}

/// Bound a sample's size, cutting on a character boundary.
fn clamp(s: &str) -> String {
    match s.char_indices().nth(MAX_SAMPLE_CHARS) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `cat file.gz` looks like after lossy decoding.
    fn gzip_like() -> String {
        let bytes: Vec<u8> = (0u32..600).map(|i| (i * 37 % 251) as u8 | 0x80).collect();
        format!("\u{1f}\u{8}{}", String::from_utf8_lossy(&bytes))
    }

    #[test]
    fn binary_data_is_told_from_text() {
        assert!(looks_binary(&gzip_like()));
        assert!(looks_binary("ELF\0\0\0"), "a NUL settles it");
        assert!(looks_binary(&"\u{1}\u{2}\u{3}\u{4}ab".repeat(10)));
    }

    #[test]
    fn text_in_any_script_is_not_binary() {
        for text in [
            "",
            "node1",
            "Filesystem  Inodes IUsed\n/dev/sda1  1000  10\n",
            "Привет, мир\tтаблица\r\n",
            "日本語のテキスト\n한국어\n",
            "\u{1b}[31mred\u{1b}[0m \u{1b}[1mbold\u{1b}[0m\n",
            // A stray undecodable byte in a log is still a log.
            "2026-10-11 error: bad byte \u{fffd} in request from 10.0.0.1\n",
        ] {
            assert!(!looks_binary(text), "{text:?}");
        }
    }

    #[test]
    fn a_binary_answer_is_replaced_by_one_line() {
        let sample = readable_sample(&gzip_like(), 0xe6f5_edc2_20c9_0ffa);
        assert!(!sample.contains('\n') && !sample.contains('\u{fffd}'), "{sample}");
        assert!(sample.starts_with("binary data, about 2 KB, digest 0xe6f5edc220c90ffa"), "{sample}");
        assert_eq!(readable_sample("5.15.0-91\n", 1), "5.15.0-91\n");
    }

    /// Progress bars redraw one line with `\r` and colour it with escape
    /// sequences; a page break or a bell in a text file is still text.
    #[test]
    fn progress_bars_and_page_breaks_are_text() {
        let bar: String = (0..=100)
            .map(|p| format!("\r\u{1b}[K{p:3}% [{}>{}]", "=".repeat(p / 5), " ".repeat(20 - p / 5)))
            .collect();
        assert!(!looks_binary(&bar));
        assert!(!looks_binary("Chapter 1\n\u{c}Chapter 2\n\u{c}Chapter 3\n\u{c}\u{c}\u{7}\n"));
    }

    /// A long text answer is still bounded on its way to the panel.
    #[test]
    fn a_long_text_sample_is_clamped() {
        let long = "line of text\n".repeat(MAX_SAMPLE_CHARS);
        let sample = clamp(&readable_sample(&long, 7));
        assert!(sample.starts_with("line of text\n"));
        assert_eq!(sample.chars().count(), MAX_SAMPLE_CHARS + 1, "clamped plus the ellipsis");
    }

    #[test]
    fn sizes_read_like_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1 KB");
        assert_eq!(human_size(12_000), "12 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
