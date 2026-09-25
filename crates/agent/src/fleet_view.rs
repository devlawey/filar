//! What a person sees of a fleet operation (#438).
//!
//! The model gets the fold (#430): who agreed with whom, and — for an
//! ad-hoc command — only digests, because host output must never reach it.
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
                    HostRun::Answered(result) => clamp(result.stdout.trim_end()),
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
                    ComparedValue::Digest(_) => group
                        .hosts()
                        .first()
                        .map(|h| output_of(h))
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

/// Bound a sample's size, cutting on a character boundary.
fn clamp(s: &str) -> String {
    match s.char_indices().nth(MAX_SAMPLE_CHARS) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}
