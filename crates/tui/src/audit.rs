//! Audit event collection for the TUI (#698).
//!
//! Fills the append-only journal of [`filar_core::audit`] from the events
//! the runner already sees: the confirmation gate (approve/deny, including
//! the auto-deny on Esc/F2/quit), the arbiter verdict, the risk rating, the
//! execution result, cancellations, cluster writes and per-host fleet
//! answers. One writer per process; records are assembled by a pure state
//! machine ([`AuditCollector`]) and written from a dedicated runner task,
//! so the TUI event loop never blocks on file I/O.
//!
//! Secret values are masked before a line is written (fail-closed: a
//! snapshot that cannot be built skips the record with a warning). The
//! hash chain stays off here — the `[audit]` config section (#699) owns
//! enabling it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use filar_agent::fleet_result::HostState;
use filar_agent::fleet_view::FleetView;
use filar_core::audit::{
    secret_snapshot, AuditDecision, AuditDecisionSource, AuditRecord, AuditTarget, AuditWriter,
};
use filar_core::{CommandConfirmMode, SecretProvider};
use tokio::sync::mpsc;

use crate::app::{App, Session};
use crate::event::TuiEvent;

/// One audit-relevant observation, fed to the collector in event order.
///
/// `session_id` is the tab's stable id rendered as a string — the same
/// identifier the session export uses.
pub enum AuditInput {
    /// `AgentEvent::CommandProposed`: a command awaits its fate.
    Proposed {
        session_id: String,
        target: AuditTarget,
        proposal_id: u64,
        command: String,
        explanation: String,
        destructive: bool,
    },
    /// `AgentEvent::CommandAudited`: the arbiter's advisory verdict on the
    /// pending proposal.
    Audited {
        session_id: String,
        verdict: String,
        reason: String,
        model: Option<String>,
        unavailable: bool,
    },
    /// `AgentEvent::CommandScored`: a safety rating, matched by `proposal_id`.
    Scored {
        proposal_id: u64,
        safety_pct: Option<u8>,
    },
    /// The confirmation window opened: its full text (for a cluster write
    /// it carries the dry-run preview) becomes the record's explanation.
    Confirmation {
        session_id: String,
        explanation: String,
    },
    /// The user answered the dialog (`respond_to_confirmation`), including
    /// the auto-deny on Esc/F2/quit.
    Decision {
        session_id: String,
        command: String,
        approved: bool,
    },
    /// `AgentEvent::CommandFinished`: the outcome. Writes the record.
    Finished {
        session_id: String,
        /// The tab's target right now — used when no proposal is pending
        /// (the user's own `!cmd` shell escape).
        target: AuditTarget,
        command: String,
        output: String,
        denied: bool,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
        confirm_mode: CommandConfirmMode,
        /// The session is a fleet: its aggregate finish is journaled per
        /// host by [`AuditInput::Fleet`] instead of one aggregate record.
        fleet: bool,
    },
    /// `AgentEvent::Cancelled`: Ctrl+C/Ctrl+Z fired the token. A proposal
    /// still pending is journaled as `cancelled`.
    Cancelled { session_id: String },
    /// `AgentEvent::ClusterChanged`: one changed object, journaled with the
    /// dry-run window text as its explanation.
    ClusterChanged { session_id: String, trace: String },
    /// `TuiEvent::FleetSummary`: the operation's person-facing view — one
    /// record per host with that host's result.
    Fleet {
        session_id: String,
        view: FleetView,
        /// Per-host target descriptors, by the view's host labels.
        hosts: HashMap<String, AuditTarget>,
        confirm_mode: CommandConfirmMode,
    },
}

/// Cheap cloneable sender into the collector task.
#[derive(Clone)]
pub struct AuditHandle {
    tx: mpsc::UnboundedSender<AuditInput>,
}

impl AuditHandle {
    /// Queue one observation. A closed channel (shutdown) drops it silently.
    pub fn send(&self, input: AuditInput) {
        let _ = self.tx.send(input);
    }
}

/// Start the one collector task of the process: a pure [`AuditCollector`]
/// plus a single [`AuditWriter`] behind a mutex, appending each record via
/// `spawn_blocking` in arrival order. Write failures only warn — auditing
/// must never block or fail a command.
///
/// `provider` is the masking set: the runner passes the union of the
/// in-memory substitution provider, the `[secret_provider]` chain and the
/// env `FILAR_SECRET_*` names, so a value resolved by any of them is masked.
/// Resolution happens inside `spawn_blocking` — a password-manager CLI link
/// must not stall the async reactor.
pub fn spawn(dir: PathBuf, provider: Arc<dyn SecretProvider>) -> AuditHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<AuditInput>();
    let writer = Arc::new(tokio::sync::Mutex::new(AuditWriter::new(dir)));
    tokio::spawn(async move {
        let mut collector = AuditCollector::default();
        while let Some(input) = rx.recv().await {
            for record in collector.handle(input) {
                let writer = Arc::clone(&writer);
                let provider = Arc::clone(&provider);
                let result = tokio::task::spawn_blocking(move || {
                    // A fresh snapshot per record: secrets added mid-session
                    // (Ctrl+P) must mask later lines too. Fail-closed: no
                    // snapshot, no line.
                    let snapshot = secret_snapshot(provider.as_ref())?;
                    let mut guard = writer.blocking_lock();
                    guard.append(&record, Some(&snapshot))
                })
                .await;
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::warn!(error = %e, "audit record not written"),
                    Err(e) => tracing::warn!(error = %e, "audit journal write task failed"),
                }
            }
        }
    });
    AuditHandle { tx }
}

/// The target descriptor of a tab at this moment: fleet, cluster, SSH or
/// local. SSH tags come from the configured target of the same name.
pub fn session_target(session: &Session, ssh_targets: &[filar_core::SshTarget]) -> AuditTarget {
    if let Some(op) = &session.fleet {
        return AuditTarget {
            kind: "fleet".into(),
            name: op.group_name().to_string(),
            host: None,
            tags: Vec::new(),
        };
    }
    if let Some(kube) = &session.kube {
        return AuditTarget {
            kind: "kube".into(),
            name: kube.target.name.clone(),
            host: None,
            tags: kube.target.tags.clone(),
        };
    }
    if let Some(info) = &session.ssh_info {
        let tags = ssh_targets
            .iter()
            .find(|t| t.name == session.target_name)
            .map(|t| t.tags.clone())
            .unwrap_or_default();
        return AuditTarget {
            kind: "ssh".into(),
            name: session.target_name.clone(),
            host: Some(info.clone()),
            tags,
        };
    }
    AuditTarget {
        kind: "local".into(),
        name: session.target_name.clone(),
        host: None,
        tags: Vec::new(),
    }
}

/// Per-host target descriptors of a fleet operation, keyed by the labels
/// [`FleetView`] uses. Kubernetes members carry their configured cluster's
/// tags when it is still in the config.
pub fn fleet_host_targets(
    op: &filar_core::FleetOperation,
    kube_clusters: &[filar_core::KubeCluster],
) -> HashMap<String, AuditTarget> {
    use filar_core::fleet_op::FleetTarget;
    op.members()
        .iter()
        .map(|m| {
            let target = match m.target() {
                FleetTarget::Ssh(t) => AuditTarget {
                    kind: "ssh".into(),
                    name: t.name.clone(),
                    host: Some(format!("{}@{}:{}", t.user, t.host, t.port)),
                    tags: t.tags.clone(),
                },
                FleetTarget::KubePod(p) => AuditTarget {
                    kind: "kube".into(),
                    name: format!("{}/{}", p.cluster, p.namespace),
                    host: None,
                    tags: kube_clusters
                        .iter()
                        .find(|c| c.name == p.cluster)
                        .map(|c| c.tags.clone())
                        .unwrap_or_default(),
                },
                FleetTarget::KubeCluster(c) => AuditTarget {
                    kind: "kube".into(),
                    name: format!("{}/{}", c.cluster, c.namespace),
                    host: None,
                    tags: kube_clusters
                        .iter()
                        .find(|k| k.name == c.cluster)
                        .map(|k| k.tags.clone())
                        .unwrap_or_default(),
                },
            };
            (m.name(), target)
        })
        .collect()
}

/// Translate a TUI event into a collector observation, if it is one.
/// Called from the runner event loop before the event reaches the UI.
pub fn input_from_event(app: &App, event: &TuiEvent) -> Option<AuditInput> {
    use filar_agent::AgentEvent;
    match event {
        TuiEvent::Agent { session_id, event } => {
            let session = app.sessions.iter().find(|s| s.id == *session_id)?;
            let sid = session_id.0.to_string();
            match event {
                AgentEvent::CommandProposed {
                    proposal_id,
                    command,
                    explanation,
                    destructive,
                    ..
                } => Some(AuditInput::Proposed {
                    session_id: sid,
                    target: session_target(session, &app.ssh_targets),
                    proposal_id: *proposal_id,
                    command: command.clone(),
                    explanation: explanation.clone(),
                    destructive: *destructive,
                }),
                AgentEvent::CommandAudited {
                    verdict,
                    reason,
                    arbiter_model,
                    unavailable,
                } => Some(AuditInput::Audited {
                    session_id: sid,
                    verdict: verdict.clone(),
                    reason: reason.clone(),
                    model: arbiter_model.clone(),
                    unavailable: *unavailable,
                }),
                AgentEvent::CommandScored {
                    proposal_id,
                    safety_pct,
                    ..
                } => Some(AuditInput::Scored {
                    proposal_id: *proposal_id,
                    safety_pct: *safety_pct,
                }),
                AgentEvent::CommandFinished {
                    command,
                    output,
                    denied,
                    exit_code,
                    duration_ms,
                } => Some(AuditInput::Finished {
                    session_id: sid,
                    target: session_target(session, &app.ssh_targets),
                    command: command.clone(),
                    output: output.clone(),
                    denied: *denied,
                    exit_code: *exit_code,
                    duration_ms: *duration_ms,
                    confirm_mode: session.confirm_mode,
                    fleet: session.fleet.is_some(),
                }),
                AgentEvent::Cancelled => Some(AuditInput::Cancelled { session_id: sid }),
                AgentEvent::ClusterChanged { trace } => Some(AuditInput::ClusterChanged {
                    session_id: sid,
                    trace: trace.clone(),
                }),
                _ => None,
            }
        }
        TuiEvent::ConfirmationRequest {
            session_id,
            explanation,
            ..
        } => Some(AuditInput::Confirmation {
            session_id: session_id.0.to_string(),
            explanation: explanation.clone(),
        }),
        TuiEvent::FleetSummary { session_id, view } => {
            let session = app.sessions.iter().find(|s| s.id == *session_id)?;
            let hosts = session
                .fleet
                .as_ref()
                .map(|op| fleet_host_targets(op, &app.kube_clusters))
                .unwrap_or_default();
            Some(AuditInput::Fleet {
                session_id: session_id.0.to_string(),
                view: view.clone(),
                hosts,
                confirm_mode: session.confirm_mode,
            })
        }
        _ => None,
    }
}

/// A proposal awaiting its outcome, with everything gathered about it.
struct Pending {
    proposal_id: Option<u64>,
    command: String,
    explanation: String,
    destructive: bool,
    target: AuditTarget,
    decision: Option<(AuditDecision, AuditDecisionSource)>,
    arbiter_verdict: Option<String>,
    arbiter_reason: Option<String>,
    arbiter_model: Option<String>,
    /// The confirmation window's full text — for a cluster write it carries
    /// the dry-run preview the user approved.
    confirm_explanation: Option<String>,
}

/// Pure event-to-records state machine: no I/O, no clock of its own beyond
/// the record timestamps, fully testable with mock events.
///
/// One record per command, written when its outcome is known (finish,
/// cancellation); a fleet operation writes one record per host instead.
/// A risk rating that arrives after its record was written is dropped —
/// the journal is append-only.
#[derive(Default)]
pub struct AuditCollector {
    pending: HashMap<String, Pending>,
    scores: HashMap<u64, u8>,
    /// Last fleet operation journaled per session: the observer fires once
    /// per operation, this guards against a replayed summary.
    fleet_ops: HashMap<String, u64>,
}

impl AuditCollector {
    /// Fold one observation into the state, returning the records it
    /// completes (usually zero or one; a fleet view returns one per host).
    pub fn handle(&mut self, input: AuditInput) -> Vec<AuditRecord> {
        match input {
            AuditInput::Proposed {
                session_id,
                target,
                proposal_id,
                command,
                explanation,
                destructive,
            } => {
                self.pending.insert(
                    session_id,
                    Pending {
                        proposal_id: Some(proposal_id),
                        command,
                        explanation,
                        destructive,
                        target,
                        decision: None,
                        arbiter_verdict: None,
                        arbiter_reason: None,
                        arbiter_model: None,
                        confirm_explanation: None,
                    },
                );
                Vec::new()
            }
            AuditInput::Audited {
                session_id,
                verdict,
                reason,
                model,
                unavailable,
            } => {
                if let Some(p) = self.pending.get_mut(&session_id) {
                    p.arbiter_verdict = Some(if unavailable {
                        "UNAVAILABLE".to_string()
                    } else {
                        verdict
                    });
                    p.arbiter_reason = if reason.is_empty() { None } else { Some(reason) };
                    p.arbiter_model = model;
                }
                Vec::new()
            }
            AuditInput::Scored {
                proposal_id,
                safety_pct,
            } => {
                if let Some(pct) = safety_pct {
                    // Bound the map: ratings that never matched a record
                    // (arrived after the write) must not grow without end.
                    if self.scores.len() >= 1024 {
                        self.scores.clear();
                    }
                    self.scores.insert(proposal_id, pct);
                }
                Vec::new()
            }
            AuditInput::Confirmation {
                session_id,
                explanation,
            } => {
                if let Some(p) = self.pending.get_mut(&session_id) {
                    p.confirm_explanation = Some(explanation);
                }
                Vec::new()
            }
            AuditInput::Decision {
                session_id,
                command,
                approved,
            } => {
                if let Some(p) = self.pending.get_mut(&session_id) {
                    if p.command == command {
                        p.decision = Some(if approved {
                            (AuditDecision::Approved, AuditDecisionSource::User)
                        } else {
                            (AuditDecision::Denied, AuditDecisionSource::User)
                        });
                    }
                }
                Vec::new()
            }
            AuditInput::Finished {
                session_id,
                target,
                command,
                output,
                denied,
                exit_code,
                duration_ms,
                confirm_mode,
                fleet,
            } => {
                let pending = self.pending.remove(&session_id);
                // A fleet's aggregate finish is journaled per host by the
                // Fleet input; only a refusal (nothing ran) writes here.
                if fleet && !denied {
                    return Vec::new();
                }
                let had_proposal = pending.is_some();
                let p = pending.filter(|p| p.command == command);
                let (decision, source) = if denied {
                    if output.is_empty() {
                        // The user said no — the Decision input recorded it.
                        p.as_ref()
                            .and_then(|p| p.decision)
                            .unwrap_or((AuditDecision::Denied, AuditDecisionSource::User))
                    } else if output.contains("Confirmation timed out") {
                        (AuditDecision::Cancelled, AuditDecisionSource::Policy)
                    } else {
                        // Refused before the user could be asked: the fleet
                        // gate, a dry-run refusal, a policy stop.
                        (AuditDecision::Denied, AuditDecisionSource::Policy)
                    }
                } else if output.contains("read-only policy")
                    || output.contains("refused to run a ~")
                {
                    // The read-only allowlist / long-wait policy stopped the
                    // command without running it.
                    (AuditDecision::Denied, AuditDecisionSource::Policy)
                } else if let Some(p) = &p {
                    p.decision.unwrap_or_else(|| auto_decision(confirm_mode))
                } else if !had_proposal {
                    // No proposal at all: the user's own `!cmd` shell escape
                    // — their direct action, not a gate decision.
                    (AuditDecision::Approved, AuditDecisionSource::User)
                } else {
                    auto_decision(confirm_mode)
                };
                let mut rec =
                    self.record_from(&session_id, p.as_ref(), &target, &command, decision, source);
                rec.exit_code = exit_code;
                rec.duration_ms = duration_ms;
                vec![rec]
            }
            AuditInput::Cancelled { session_id } => {
                match self.pending.remove(&session_id) {
                    Some(p) => {
                        let command = p.command.clone();
                        vec![self.record_from(
                            &session_id,
                            Some(&p),
                            &p.target.clone(),
                            &command,
                            AuditDecision::Cancelled,
                            AuditDecisionSource::User,
                        )]
                    }
                    None => Vec::new(),
                }
            }
            AuditInput::ClusterChanged { session_id, trace } => {
                let Some(p) = self.pending.get(&session_id) else {
                    return Vec::new();
                };
                let (decision, source) = p
                    .decision
                    .unwrap_or((AuditDecision::Approved, AuditDecisionSource::User));
                let mut rec =
                    AuditRecord::new(session_id, p.target.clone(), trace, decision, source);
                rec.explanation = p
                    .confirm_explanation
                    .clone()
                    .unwrap_or_else(|| p.explanation.clone());
                rec.destructive = p.destructive;
                rec.proposal_id = p.proposal_id;
                rec.risk_score = p.proposal_id.and_then(|pid| self.scores.get(&pid).copied());
                vec![rec]
            }
            AuditInput::Fleet {
                session_id,
                view,
                hosts,
                confirm_mode,
            } => {
                let op = view.operation.as_u64();
                if self.fleet_ops.get(&session_id) == Some(&op) {
                    return Vec::new();
                }
                self.fleet_ops.insert(session_id.clone(), op);
                let pending = self.pending.get(&session_id);
                let (decision, source) = pending
                    .and_then(|p| p.decision)
                    .unwrap_or_else(|| auto_decision(confirm_mode));
                let fallback = pending.map(|p| p.target.clone()).unwrap_or_default();
                let explanation = pending
                    .map(|p| {
                        p.confirm_explanation
                            .clone()
                            .unwrap_or_else(|| p.explanation.clone())
                    })
                    .unwrap_or_default();
                let destructive = pending.map(|p| p.destructive).unwrap_or(false);
                let proposal_id = pending.and_then(|p| p.proposal_id);
                let risk_score = proposal_id.and_then(|pid| self.scores.get(&pid).copied());
                let mut out = Vec::new();
                let mut record = |host: &str,
                                  dec: AuditDecision,
                                  src: AuditDecisionSource,
                                  exit: Option<i32>| {
                    let target = hosts.get(host).cloned().unwrap_or_else(|| fallback.clone());
                    let mut rec =
                        AuditRecord::new(session_id.clone(), target, view.command.clone(), dec, src);
                    rec.explanation = explanation.clone();
                    rec.destructive = destructive;
                    rec.proposal_id = proposal_id;
                    rec.risk_score = risk_score;
                    rec.exit_code = exit;
                    out.push(rec);
                };
                // Hosts in a group answered and exited 0 (that is what puts
                // them in a group); a divergent answer still exited cleanly.
                for group in &view.groups {
                    for host in &group.hosts {
                        record(host, decision, source, Some(0));
                    }
                }
                for d in &view.dropped {
                    let (dec, src, exit) = match d.state {
                        HostState::ExecutionError => (decision, source, d.exit_code),
                        HostState::AccessDenied => {
                            (AuditDecision::Denied, AuditDecisionSource::Policy, None)
                        }
                        // Timed out, unreachable, cancelled, disappeared,
                        // never asked: the host produced no result.
                        _ => (AuditDecision::Cancelled, AuditDecisionSource::Policy, None),
                    };
                    record(&d.host, dec, src, exit);
                }
                out
            }
        }
    }

    /// Assemble a record from a pending proposal (when there is one) and
    /// consume its risk rating. Without a proposal the caller's target
    /// descriptor is used (the shell escape's tab).
    fn record_from(
        &mut self,
        session_id: &str,
        p: Option<&Pending>,
        fallback_target: &AuditTarget,
        command: &str,
        decision: AuditDecision,
        source: AuditDecisionSource,
    ) -> AuditRecord {
        let target = p
            .map(|p| p.target.clone())
            .unwrap_or_else(|| fallback_target.clone());
        let mut rec = AuditRecord::new(session_id, target, command, decision, source);
        if let Some(p) = p {
            rec.explanation = p
                .confirm_explanation
                .clone()
                .unwrap_or_else(|| p.explanation.clone());
            rec.destructive = p.destructive;
            rec.proposal_id = p.proposal_id;
            rec.arbiter_verdict = p.arbiter_verdict.clone();
            rec.arbiter_reason = p.arbiter_reason.clone();
            rec.arbiter_model = p.arbiter_model.clone();
            rec.risk_score = p.proposal_id.and_then(|pid| self.scores.remove(&pid));
        }
        rec
    }
}

/// The decision of a command that ran without a user answer: the confirm
/// mode says which gate let it through. `Always`/`Explain` confirm every
/// command, so an unconfirmed finish under them can only be a policy path.
fn auto_decision(mode: CommandConfirmMode) -> (AuditDecision, AuditDecisionSource) {
    match mode {
        CommandConfirmMode::Allowlist => (AuditDecision::Auto, AuditDecisionSource::Allowlist),
        CommandConfirmMode::Never => (AuditDecision::Auto, AuditDecisionSource::Never),
        _ => (AuditDecision::Auto, AuditDecisionSource::Policy),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use filar_agent::fleet_view::{GroupRole, ViewDropped, ViewGroup};
    use filar_core::audit::read_records;
    use filar_core::StaticSecretProvider;

    fn target() -> AuditTarget {
        AuditTarget {
            kind: "ssh".into(),
            name: "web-1".into(),
            host: Some("root@10.0.0.5:22".into()),
            tags: vec!["prod".into()],
        }
    }

    fn proposed(sid: &str, pid: u64, command: &str) -> AuditInput {
        AuditInput::Proposed {
            session_id: sid.into(),
            target: target(),
            proposal_id: pid,
            command: command.into(),
            explanation: "check the load".into(),
            destructive: false,
        }
    }

    fn finished(sid: &str, command: &str, denied: bool, output: &str) -> AuditInput {
        AuditInput::Finished {
            session_id: sid.into(),
            target: target(),
            command: command.into(),
            output: output.into(),
            denied,
            exit_code: if denied { None } else { Some(0) },
            duration_ms: if denied { None } else { Some(12) },
            confirm_mode: CommandConfirmMode::Always,
            fleet: false,
        }
    }

    /// The full life of an approved command: proposal, arbiter, rating,
    /// user decision, finish — one record carrying all of it.
    #[test]
    fn a_record_is_assembled_from_the_event_stream() {
        let mut c = AuditCollector::default();
        assert!(c.handle(proposed("1", 7, "uptime")).is_empty());
        assert!(c
            .handle(AuditInput::Audited {
                session_id: "1".into(),
                verdict: "AGREE".into(),
                reason: String::new(),
                model: Some("arbiter-x".into()),
                unavailable: false,
            })
            .is_empty());
        assert!(c
            .handle(AuditInput::Scored {
                proposal_id: 7,
                safety_pct: Some(95),
            })
            .is_empty());
        assert!(c
            .handle(AuditInput::Decision {
                session_id: "1".into(),
                command: "uptime".into(),
                approved: true,
            })
            .is_empty());
        let records = c.handle(finished("1", "uptime", false, "load average: 0.1"));
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r.session_id, "1");
        assert_eq!(r.command, "uptime");
        assert_eq!(r.explanation, "check the load");
        assert_eq!(r.target, target());
        assert_eq!(r.decision, AuditDecision::Approved);
        assert_eq!(r.decision_source, AuditDecisionSource::User);
        assert_eq!(r.arbiter_verdict.as_deref(), Some("AGREE"));
        assert_eq!(r.arbiter_model.as_deref(), Some("arbiter-x"));
        assert_eq!(r.risk_score, Some(95));
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.duration_ms, Some(12));
        assert_eq!(r.proposal_id, Some(7));
        // The rating is consumed with the record.
        assert!(c.scores.is_empty());
    }

    #[test]
    fn a_user_denial_is_journaled_as_denied() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "rm -rf /tmp/x"));
        c.handle(AuditInput::Decision {
            session_id: "1".into(),
            command: "rm -rf /tmp/x".into(),
            approved: false,
        });
        let records = c.handle(finished("1", "rm -rf /tmp/x", true, ""));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision, AuditDecision::Denied);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
        assert_eq!(records[0].exit_code, None);
    }

    #[test]
    fn a_confirmation_timeout_is_cancelled_by_policy() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "uptime"));
        let records = c.handle(finished("1", "uptime", true, "Confirmation timed out"));
        assert_eq!(records[0].decision, AuditDecision::Cancelled);
        assert_eq!(records[0].decision_source, AuditDecisionSource::Policy);
    }

    #[test]
    fn a_gate_refusal_is_denied_by_policy() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "systemctl restart nginx"));
        let records = c.handle(finished(
            "1",
            "systemctl restart nginx",
            true,
            filar_agent::fleet_gate::FLEET_WRITE_DENIED,
        ));
        assert_eq!(records[0].decision, AuditDecision::Denied);
        assert_eq!(records[0].decision_source, AuditDecisionSource::Policy);
    }

    #[test]
    fn a_read_only_refusal_is_denied_by_policy() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "touch /tmp/x"));
        let records = c.handle(finished(
            "1",
            "touch /tmp/x",
            false,
            "Error: read-only policy: \"touch\" is not in the read-only allowlist — command not sent to the host: touch /tmp/x",
        ));
        assert_eq!(records[0].decision, AuditDecision::Denied);
        assert_eq!(records[0].decision_source, AuditDecisionSource::Policy);
    }

    #[test]
    fn ctrl_c_with_a_pending_proposal_is_journaled_as_cancelled() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "sleep 30"));
        let records = c.handle(AuditInput::Cancelled {
            session_id: "1".into(),
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision, AuditDecision::Cancelled);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
        assert_eq!(records[0].command, "sleep 30");
        // Nothing pending left: a second cancellation writes nothing.
        assert!(c
            .handle(AuditInput::Cancelled {
                session_id: "1".into(),
            })
            .is_empty());
    }

    #[test]
    fn an_auto_approved_command_records_the_allowlist() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "ls"));
        let records = c.handle(AuditInput::Finished {
            session_id: "1".into(),
            target: target(),
            command: "ls".into(),
            output: "file1".into(),
            denied: false,
            exit_code: Some(0),
            duration_ms: Some(3),
            confirm_mode: CommandConfirmMode::Allowlist,
            fleet: false,
        });
        assert_eq!(records[0].decision, AuditDecision::Auto);
        assert_eq!(records[0].decision_source, AuditDecisionSource::Allowlist);
    }

    /// A finish without any proposal is the user's own `!cmd` shell escape:
    /// the record carries the tab's target and reads as the user's action.
    #[test]
    fn a_shell_escape_records_the_tab_target_and_the_user() {
        let mut c = AuditCollector::default();
        let records = c.handle(AuditInput::Finished {
            session_id: "1".into(),
            target: AuditTarget {
                kind: "local".into(),
                name: "local".into(),
                host: None,
                tags: Vec::new(),
            },
            command: "echo hello".into(),
            output: "hello".into(),
            denied: false,
            exit_code: Some(0),
            duration_ms: Some(5),
            confirm_mode: CommandConfirmMode::Always,
            fleet: false,
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].target.kind, "local");
        assert_eq!(records[0].decision, AuditDecision::Approved);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
        assert_eq!(records[0].exit_code, Some(0));
    }

    /// A fleet view becomes one record per host: group members exited 0,
    /// a failed host keeps its exit code, an unreachable one is cancelled.
    #[test]
    fn a_fleet_view_writes_one_record_per_host() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "uname -r"));
        c.handle(AuditInput::Decision {
            session_id: "1".into(),
            command: "uname -r".into(),
            approved: true,
        });
        let group = filar_core::HostGroup {
            name: "web".into(),
            match_tags: vec!["web".into()],
            ..Default::default()
        };
        let targets: Vec<filar_core::SshTarget> = ["web-1", "web-2", "web-3", "web-4"]
            .iter()
            .map(|name| filar_core::SshTarget {
                name: (*name).into(),
                host: (*name).into(),
                port: 22,
                user: "root".into(),
                auth: Default::default(),
                host_key_policy: Default::default(),
                tags: vec!["web".into()],
            })
            .collect();
        let op = filar_core::FleetOperation::open(&group, &targets);
        let view = FleetView {
            operation: op.id(),
            command: "uname -r".into(),
            headline: "3 hosts answered".into(),
            groups: vec![ViewGroup {
                hosts: vec!["web-1".into(), "web-2".into()],
                sample: "5.15.0".into(),
                role: GroupRole::Baseline,
            }],
            dropped: vec![
                ViewDropped {
                    host: "web-3".into(),
                    state: HostState::ExecutionError,
                    exit_code: Some(2),
                    reason: Some("boom".into()),
                },
                ViewDropped {
                    host: "web-4".into(),
                    state: HostState::NoContact,
                    exit_code: None,
                    reason: None,
                },
            ],
        };
        let mut hosts = HashMap::new();
        for name in ["web-1", "web-2", "web-3", "web-4"] {
            hosts.insert(
                name.to_string(),
                AuditTarget {
                    kind: "ssh".into(),
                    name: name.into(),
                    host: Some(format!("root@{name}:22")),
                    tags: vec!["web".into()],
                },
            );
        }
        let records = c.handle(AuditInput::Fleet {
            session_id: "1".into(),
            view: view.clone(),
            hosts,
            confirm_mode: CommandConfirmMode::Always,
        });
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].target.name, "web-1");
        assert_eq!(records[0].exit_code, Some(0));
        assert_eq!(records[0].decision, AuditDecision::Approved);
        assert_eq!(records[1].target.name, "web-2");
        assert_eq!(records[2].target.name, "web-3");
        assert_eq!(records[2].exit_code, Some(2));
        assert_eq!(records[2].decision, AuditDecision::Approved);
        assert_eq!(records[3].target.name, "web-4");
        assert_eq!(records[3].decision, AuditDecision::Cancelled);
        assert_eq!(records[3].decision_source, AuditDecisionSource::Policy);
        // The aggregate finish of a fleet writes nothing extra.
        assert!(c
            .handle(AuditInput::Finished {
                session_id: "1".into(),
                target: target(),
                command: "uname -r".into(),
                output: "3 hosts answered".into(),
                denied: false,
                exit_code: Some(0),
                duration_ms: Some(100),
                confirm_mode: CommandConfirmMode::Always,
                fleet: true,
            })
            .is_empty());
        // A replayed summary of the same operation writes nothing.
        assert!(c
            .handle(AuditInput::Fleet {
                session_id: "1".into(),
                view,
                hosts: HashMap::new(),
                confirm_mode: CommandConfirmMode::Always,
            })
            .is_empty());
    }

    /// A cluster write journals one record per change, with the dry-run
    /// window text as the explanation.
    #[test]
    fn a_cluster_change_carries_the_dry_run_window() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "k8s_apply manifest.yaml"));
        let window = "scale the deployment\n\nTool: k8s_scale\nDry run:\ndeployment/web scaled to 3";
        c.handle(AuditInput::Confirmation {
            session_id: "1".into(),
            explanation: window.into(),
        });
        c.handle(AuditInput::Decision {
            session_id: "1".into(),
            command: "k8s_apply manifest.yaml".into(),
            approved: true,
        });
        let records = c.handle(AuditInput::ClusterChanged {
            session_id: "1".into(),
            trace: "Изменение в кластере: scale deployment shop/web".into(),
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].explanation, window);
        assert_eq!(records[0].decision, AuditDecision::Approved);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
        // The command's own finish still writes its record afterwards.
        let records = c.handle(finished("1", "k8s_apply manifest.yaml", false, "scaled"));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].explanation, window);
    }

    /// The journal on disk must not contain a secret value: the writer
    /// masks command, explanation and arbiter reason from the snapshot.
    #[tokio::test]
    async fn the_journal_masks_secret_values() {
        let dir = std::env::temp_dir().join(format!("filar-audit-tui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let provider = Arc::new(StaticSecretProvider::with_secrets(
            [("$FILAR_SECRET_TOKEN".to_string(), "hunter2-tui-secret".to_string())].into(),
        ));
        let handle = spawn(dir.clone(), provider);
        handle.send(proposed("1", 1, "curl -H 'Bearer hunter2-tui-secret' https://x"));
        handle.send(AuditInput::Audited {
            session_id: "1".into(),
            verdict: "MISMATCH".into(),
            reason: "token hunter2-tui-secret is stale".into(),
            model: None,
            unavailable: false,
        });
        handle.send(AuditInput::Decision {
            session_id: "1".into(),
            command: "curl -H 'Bearer hunter2-tui-secret' https://x".into(),
            approved: true,
        });
        handle.send(finished(
            "1",
            "curl -H 'Bearer hunter2-tui-secret' https://x",
            false,
            "ok",
        ));
        // Let the collector task drain the queue and finish its writes.
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if let Ok(entries) = std::fs::read_dir(&dir) {
                if entries.count() > 0 {
                    break;
                }
            }
        }
        let mut found = false;
        for entry in std::fs::read_dir(&dir).expect("journal dir") {
            let path = entry.expect("entry").path();
            let records = read_records(&path).expect("readable journal");
            if records.is_empty() {
                continue;
            }
            found = true;
            let raw = std::fs::read_to_string(&path).expect("raw journal");
            assert!(!raw.contains("hunter2-tui-secret"), "secret leaked: {raw}");
            assert!(records[0].command.contains("$FILAR_SECRET_TOKEN"));
            assert!(records[0]
                .arbiter_reason
                .as_deref()
                .unwrap_or_default()
                .contains("$FILAR_SECRET_TOKEN"));
        }
        assert!(found, "no journal record was written");
        let _ = std::fs::remove_dir_all(&dir);
    }
}