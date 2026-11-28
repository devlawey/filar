//! Audit event collection for the TUI (#698).
//!
//! Fills the append-only journal of [`filar_core::audit`] from the events
//! the runner already sees: the confirmation gate (approve/deny, including
//! the auto-deny on Ctrl+Z/quit), the arbiter verdict, the risk rating, the
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
        /// The agent's effective confirm mode for this proposal (#698
        /// review): the mode can change mid-turn (F2) and a tag policy can
        /// tighten it beyond the tab's setting — the gate that judged this
        /// command is the one the agent reports, at proposal time.
        confirm_mode: CommandConfirmMode,
        /// The tool behind the proposal: executor-level policy refusals are
        /// only inferred from output text for shell-execution tools, never
        /// for a log query whose answer may quote the same words (#698).
        tool_name: String,
        /// The fleet operation this proposal belongs to, when the tab is a
        /// fleet: a late summary matches its own operation, not whatever
        /// proposal is pending now (#698 review).
        fleet_op: Option<u64>,
        /// Per-host target descriptors of that operation, by the view's
        /// host labels — kept so a summary that lands after the tab closed
        /// still journals real targets.
        hosts: HashMap<String, AuditTarget>,
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
    /// the auto-deny on Ctrl+Z/quit.
    Decision {
        session_id: String,
        command: String,
        approved: bool,
        /// The answer was a mode switch (F2), not a deliberate yes/no
        /// (#698 review): the command is cancelled, not denied.
        mode_switch: bool,
    },
    /// `AgentEvent::CommandFinished`: the outcome. Writes the record.
    Finished {
        session_id: String,
        /// The originating proposal's id (#698 review): the outcome consumes
        /// only its own proposal, never a newer one. `None` for the user's
        /// own `!cmd` shell escape, which has no proposal.
        proposal_id: Option<u64>,
        /// The tab's target — captured at command start for a shell escape,
        /// read live otherwise; a default when the tab is already closed
        /// (the pending proposal's own target wins then).
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
    /// `AgentEvent::CommandRefused`: the gate refused the proposal before
    /// anyone was asked — an explicit policy denial, no finish follows.
    Refused {
        session_id: String,
        proposal_id: u64,
        reason: String,
    },
    /// `AgentEvent::Cancelled`: Ctrl+Z fired the token. The proposal
    /// it names (when any) is journaled as `cancelled`.
    Cancelled {
        session_id: String,
        /// The proposal the turn was working on (#698 review), so a late
        /// cancellation never consumes a newer proposal.
        proposal_id: Option<u64>,
    },
    /// The turn ended (`AgentEvent::Finished`/`Error`): ratings still in
    /// flight have settled, so held records are released and a proposal
    /// left without an outcome is journaled with its recorded decision or
    /// as cancelled — its fate is unknown (#698 review).
    TurnEnded { session_id: String },
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
    tx: mpsc::Sender<AuditInput>,
}

impl AuditHandle {
    /// Queue one observation without ever blocking the event loop: a full
    /// queue drops the observation with a warning (auditing must not stall
    /// the TUI); a closed channel (shutdown) drops it silently.
    pub fn send(&self, input: AuditInput) {
        match self.tx.try_send(input) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("audit queue full: observation dropped");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

/// Start the one collector task of the process: a pure [`AuditCollector`]
/// plus a single [`AuditWriter`] behind a mutex, appending each record via
/// `spawn_blocking` in arrival order. Write failures only warn — auditing
/// must never block or fail a command.
///
/// `provider` is the masking set: the runner passes a [`MaskingUnion`] of
/// the in-memory substitution provider, the `[secret_provider]` chain and
/// the env `FILAR_SECRET_*` names, so a value resolved by any of them is
/// masked. Resolution happens inside `spawn_blocking` — a password-manager
/// CLI link must not stall the async reactor.
///
/// `scoring` tells the collector a risk scorer is configured, so finished
/// records are held for a rating that may still be in flight (#698 review).
/// The returned [`tokio::task::JoinHandle`] lets the runner drain the queue
/// on shutdown: drop every sender, then await it (with a timeout) so the
/// last records of a session are not lost.
pub fn spawn(
    dir: PathBuf,
    provider: Arc<dyn SecretProvider>,
    scoring: bool,
) -> (AuditHandle, tokio::task::JoinHandle<()>) {
    // Bounded (#698 review): an unbounded queue could grow without end if
    // the writer stalls; 1024 observations is far more than a turn produces.
    let (tx, mut rx) = mpsc::channel::<AuditInput>(1024);
    let writer = Arc::new(tokio::sync::Mutex::new(AuditWriter::new(dir)));
    let task = tokio::spawn(async move {
        let mut collector = AuditCollector::with_scoring(scoring);
        while let Some(input) = rx.recv().await {
            for record in collector.handle(input) {
                append_record(&writer, &provider, record).await;
            }
        }
        // Every sender is gone (shutdown): flush what is still in flight so
        // a quit never loses the last records (#698 review).
        for record in collector.finish() {
            append_record(&writer, &provider, record).await;
        }
    });
    (AuditHandle { tx }, task)
}

/// Append one record with a fresh masking snapshot, off the async reactor.
async fn append_record(
    writer: &Arc<tokio::sync::Mutex<AuditWriter>>,
    provider: &Arc<dyn SecretProvider>,
    record: AuditRecord,
) {
    let writer = Arc::clone(writer);
    let provider = Arc::clone(provider);
    let result = tokio::task::spawn_blocking(move || {
        // A fresh snapshot per record: secrets added mid-session (Ctrl+P)
        // must mask later lines too. Fail-closed: no snapshot, no line.
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
        // A pod tab journals the pod's own address, not just the cluster
        // (#698 review): the shell ran in a container of that pod.
        if let Some(pod) = &kube.pod {
            let container = pod
                .container
                .as_deref()
                .or(pod.pod_ref.container.as_deref());
            let host = match container {
                Some(c) => format!("pod/{}/{}", pod.display_pod(), c),
                None => format!("pod/{}", pod.display_pod()),
            };
            return AuditTarget {
                kind: "kube".into(),
                name: format!("{}/{}", pod.pod_ref.cluster, pod.pod_ref.namespace),
                host: Some(host),
                tags: kube.target.tags.clone(),
            };
        }
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
                    // A pod member journals the pod's own address (#698
                    // review): the shell ran in a container of that pod.
                    host: Some(if p.container.is_empty() {
                        format!("pod/{}", p.pod)
                    } else {
                        format!("pod/{}/{}", p.pod, p.container)
                    }),
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
            let sid = session_id.0.to_string();
            // Terminal events route without a live session (#698 review):
            // the tab may already be closed when the agent's last event
            // lands, and the collector still holds its pending proposal.
            match event {
                AgentEvent::Cancelled { proposal_id } => {
                    return Some(AuditInput::Cancelled {
                        session_id: sid,
                        proposal_id: *proposal_id,
                    });
                }
                AgentEvent::Finished(_) | AgentEvent::Error(_) => {
                    return Some(AuditInput::TurnEnded { session_id: sid });
                }
                AgentEvent::CommandRefused {
                    proposal_id,
                    reason,
                } => {
                    return Some(AuditInput::Refused {
                        session_id: sid,
                        proposal_id: *proposal_id,
                        reason: reason.clone(),
                    });
                }
                _ => {}
            }
            let session = app.sessions.iter().find(|s| s.id == *session_id)?;
            match event {
                AgentEvent::CommandProposed {
                    proposal_id,
                    command,
                    explanation,
                    destructive,
                    tool,
                    confirm_mode,
                } => {
                    let fleet_op = session.fleet.as_ref().map(|op| op.id().as_u64());
                    let hosts = session
                        .fleet
                        .as_ref()
                        .map(|op| fleet_host_targets(op, &app.kube_clusters))
                        .unwrap_or_default();
                    Some(AuditInput::Proposed {
                        session_id: sid,
                        target: session_target(session, &app.ssh_targets),
                        proposal_id: *proposal_id,
                        command: command.clone(),
                        explanation: bound_output(explanation),
                        destructive: *destructive,
                        confirm_mode: *confirm_mode,
                        tool_name: tool.name.clone(),
                        fleet_op,
                        hosts,
                    })
                }
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
                    proposal_id,
                    command,
                    output,
                    denied,
                    exit_code,
                    duration_ms,
                } => Some(AuditInput::Finished {
                    session_id: sid,
                    proposal_id: *proposal_id,
                    target: session_target(session, &app.ssh_targets),
                    command: command.clone(),
                    output: bound_output(output),
                    denied: *denied,
                    exit_code: *exit_code,
                    duration_ms: *duration_ms,
                    confirm_mode: session.confirm_mode,
                    fleet: session.fleet.is_some(),
                }),
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
            explanation: bound_output(explanation),
        }),
        TuiEvent::FleetSummary { session_id, view } => {
            // No live session required (#698 review): a summary that lands
            // after the tab closed still journals — the collector keeps the
            // operation's own proposal context and host targets.
            let session = app.sessions.iter().find(|s| s.id == *session_id);
            let op = view.operation.as_u64();
            let hosts = session
                .and_then(|s| s.fleet.as_ref())
                .filter(|fleet| fleet.id().as_u64() == op)
                .map(|fleet| fleet_host_targets(fleet, &app.kube_clusters))
                .unwrap_or_default();
            // The collector never reads the samples; drop them so a fleet
            // of megabyte answers does not sit in the queue (#698 review).
            let mut view = view.clone();
            for group in &mut view.groups {
                group.sample = String::new();
            }
            Some(AuditInput::Fleet {
                session_id: session_id.0.to_string(),
                view,
                hosts,
                confirm_mode: session
                    .map(|s| s.confirm_mode)
                    .unwrap_or(CommandConfirmMode::Always),
            })
        }
        TuiEvent::Shell {
            session_id,
            event,
            command,
            target,
            confirm_mode,
            fleet,
        } => {
            // The user's own `!cmd` escape journals from the context captured
            // at command start (#698 review): the tab may have reconnected or
            // closed since. The app renders the same event for the feed; no
            // agent turn-end follows a shell escape.
            use filar_agent::AgentEvent;
            let (output, denied, exit_code, duration_ms) = match event {
                AgentEvent::CommandFinished {
                    output,
                    denied,
                    exit_code,
                    duration_ms,
                    ..
                } => (
                    bound_output(output),
                    *denied,
                    *exit_code,
                    *duration_ms,
                ),
                AgentEvent::Error(text) => (bound_output(text), false, None, None),
                _ => return None,
            };
            Some(AuditInput::Finished {
                session_id: session_id.0.to_string(),
                proposal_id: None,
                target: target.clone(),
                command: command.clone(),
                output,
                denied,
                exit_code,
                duration_ms,
                confirm_mode: *confirm_mode,
                fleet: *fleet,
            })
        }
        _ => None,
    }
}

/// Longest command output kept on an audit observation, in characters
/// (#698 review): the collector only sniffs the leading policy-refusal
/// text and journals a shell escape's error, never the whole output. This
/// bounds the queue payload for a command that printed megabytes.
const MAX_AUDIT_OUTPUT_CHARS: usize = 2048;

/// Cut an output to [`MAX_AUDIT_OUTPUT_CHARS`] on a character boundary.
pub(crate) fn bound_output(output: &str) -> String {
    match output.char_indices().nth(MAX_AUDIT_OUTPUT_CHARS) {
        Some((cut, _)) => format!("{}…", &output[..cut]),
        None => output.to_string(),
    }
}

/// Whether a tool's command actually runs through a shell executor, so an
/// executor-level policy refusal ("Error: read-only policy…", "Error:
/// refused to run a ~…") can be told from output that merely quotes those
/// words. Log queries and Kubernetes reads never run a shell — their
/// answers may legitimately contain the same text (#698 review).
fn is_shell_execution_tool(name: &str) -> bool {
    matches!(
        name,
        "run_command" | "read_file" | "list_dir" | "start_background_job" | "cancel_background_job"
    )
}

/// A proposal awaiting its outcome, with everything gathered about it.
#[derive(Clone)]
struct Pending {
    proposal_id: Option<u64>,
    command: String,
    explanation: String,
    destructive: bool,
    target: AuditTarget,
    /// The confirm mode in force when the proposal was made (#698 review).
    confirm_mode: CommandConfirmMode,
    /// The tool behind the proposal — gates output-text refusal sniffing.
    tool_name: String,
    /// The fleet operation this proposal belongs to, when the tab is a
    /// fleet (#698 review): a summary matches its own operation.
    fleet_op: Option<u64>,
    /// Per-host targets of that operation, by the view's host labels.
    hosts: HashMap<String, AuditTarget>,
    decision: Option<(AuditDecision, AuditDecisionSource)>,
    arbiter_verdict: Option<String>,
    arbiter_reason: Option<String>,
    arbiter_model: Option<String>,
    /// The confirmation window's full text — for a cluster write it carries
    /// the dry-run preview the user approved.
    confirm_explanation: Option<String>,
}

/// What a fleet summary needs from its proposal, kept after the proposal
/// is consumed or replaced (#698 review): a summary that lands late must
/// journal its own operation's context, not a newer proposal's.
struct FleetCtx {
    explanation: String,
    destructive: bool,
    target: AuditTarget,
    proposal_id: Option<u64>,
    decision: Option<(AuditDecision, AuditDecisionSource)>,
    arbiter_verdict: Option<String>,
    arbiter_reason: Option<String>,
    arbiter_model: Option<String>,
    hosts: HashMap<String, AuditTarget>,
}

impl FleetCtx {
    fn from_pending(p: &Pending) -> Self {
        Self {
            explanation: p
                .confirm_explanation
                .clone()
                .unwrap_or_else(|| p.explanation.clone()),
            destructive: p.destructive,
            target: p.target.clone(),
            proposal_id: p.proposal_id,
            decision: p.decision,
            arbiter_verdict: p.arbiter_verdict.clone(),
            arbiter_reason: p.arbiter_reason.clone(),
            arbiter_model: p.arbiter_model.clone(),
            hosts: p.hosts.clone(),
        }
    }
}

/// Pure event-to-records state machine: no I/O, no clock of its own beyond
/// the record timestamps, fully testable with mock events.
///
/// One record per command, written when its outcome is known (finish,
/// cancellation); a fleet operation writes one record per host instead.
/// When a risk scorer is configured, a finished record is held until its
/// rating lands (or the turn ends) so a late score is not lost (#698
/// review); both agents settle ratings before the turn's terminal event.
#[derive(Default)]
pub struct AuditCollector {
    pending: HashMap<String, Pending>,
    scores: HashMap<u64, u8>,
    /// Last fleet operation journaled per session: the observer fires once
    /// per operation, this guards against a replayed summary.
    fleet_ops: HashMap<String, u64>,
    /// Proposal context per fleet operation, kept until its summary lands
    /// (#698 review) — bounded like the hold.
    fleet_ctx: HashMap<(String, u64), FleetCtx>,
    /// The last proposal whose outcome was journaled, per session: a
    /// cancellation naming an already-finished command still journals the
    /// cancelled turn (#698 review).
    last_consumed: HashMap<String, Pending>,
    /// Finished records waiting for their risk rating (#698 review).
    held: Vec<AuditRecord>,
    /// Whether a risk scorer is configured: only then are finished records
    /// held for a rating that may still be in flight.
    scoring: bool,
}

impl AuditCollector {
    /// A collector that holds finished records for in-flight ratings when
    /// `scoring` is set (the runner passes whether a scorer is configured).
    pub fn with_scoring(scoring: bool) -> Self {
        Self {
            scoring,
            ..Default::default()
        }
    }

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
                confirm_mode,
                tool_name,
                fleet_op,
                hosts,
            } => {
                let mut out = Vec::new();
                // A proposal replaced without an outcome event: its fate is
                // unknown — an explicit gate refusal arrives as `Refused`,
                // so nothing here may infer a denial (#698 review). Keep a
                // recorded user decision; otherwise journal it cancelled
                // with the uncertainty spelled out.
                if let Some(old) = self.pending.remove(&session_id) {
                    self.remember_fleet_ctx(&session_id, &old);
                    let (decision, source) = old
                        .decision
                        .unwrap_or((AuditDecision::Cancelled, AuditDecisionSource::Policy));
                    let mut rec = self.orphan_record(&session_id, old, decision, source);
                    if rec.decision == AuditDecision::Cancelled {
                        rec.explanation = with_note(
                            rec.explanation,
                            "outcome unknown: replaced by a newer proposal without a finish event",
                        );
                    }
                    out.push(rec);
                }
                let p = Pending {
                    proposal_id: Some(proposal_id),
                    command,
                    explanation,
                    destructive,
                    target,
                    confirm_mode,
                    tool_name,
                    fleet_op,
                    hosts,
                    decision: None,
                    arbiter_verdict: None,
                    arbiter_reason: None,
                    arbiter_model: None,
                    confirm_explanation: None,
                };
                self.remember_fleet_ctx(&session_id, &p);
                self.pending.insert(session_id, p);
                out
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
                    let update = p
                        .fleet_op
                        .map(|op| ((session_id.clone(), op), FleetCtx::from_pending(p)));
                    if let Some((key, ctx)) = update {
                        self.insert_fleet_ctx(key, ctx);
                    }
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
                // Release a held record whose rating just arrived (or proved
                // unavailable): the journal is append-only, so this is the
                // last chance to attach it (#698 review).
                let mut out = Vec::new();
                let mut released = false;
                let mut i = 0;
                while i < self.held.len() {
                    if self.held[i].proposal_id == Some(proposal_id) {
                        let mut rec = self.held.remove(i);
                        if rec.risk_score.is_none() {
                            rec.risk_score = safety_pct;
                        }
                        out.push(rec);
                        released = true;
                    } else {
                        i += 1;
                    }
                }
                if released {
                    self.scores.remove(&proposal_id);
                }
                out
            }
            AuditInput::Confirmation {
                session_id,
                explanation,
            } => {
                if let Some(p) = self.pending.get_mut(&session_id) {
                    p.confirm_explanation = Some(explanation);
                    let update = p
                        .fleet_op
                        .map(|op| ((session_id.clone(), op), FleetCtx::from_pending(p)));
                    if let Some((key, ctx)) = update {
                        self.insert_fleet_ctx(key, ctx);
                    }
                }
                Vec::new()
            }
            AuditInput::Decision {
                session_id,
                command,
                approved,
                mode_switch,
            } => {
                if let Some(p) = self.pending.get_mut(&session_id) {
                    if p.command == command {
                        p.decision = Some(if mode_switch {
                            // F2 switched the mode instead of answering: the
                            // command is cancelled, not deliberately denied.
                            (AuditDecision::Cancelled, AuditDecisionSource::User)
                        } else if approved {
                            (AuditDecision::Approved, AuditDecisionSource::User)
                        } else {
                            (AuditDecision::Denied, AuditDecisionSource::User)
                        });
                        let update = p
                            .fleet_op
                            .map(|op| ((session_id.clone(), op), FleetCtx::from_pending(p)));
                        if let Some((key, ctx)) = update {
                            self.insert_fleet_ctx(key, ctx);
                        }
                    }
                }
                Vec::new()
            }
            AuditInput::Finished {
                session_id,
                proposal_id,
                target,
                command,
                output,
                denied,
                exit_code,
                duration_ms,
                confirm_mode,
                fleet,
            } => {
                // Identity matching (#698 review): an outcome consumes only
                // its own proposal. A late outcome of an older proposal (a
                // newer one is pending) is dropped — its fate is already
                // journaled; a shell escape (`None`) never touches a proposal.
                let p = match proposal_id {
                    Some(pid) => match self.pending.get(&session_id) {
                        Some(p) if p.proposal_id == Some(pid) => self.pending.remove(&session_id),
                        Some(_) => return Vec::new(),
                        None => None,
                    },
                    None => None,
                };
                // A fleet's aggregate finish is journaled per host by the
                // Fleet input; only a refusal (nothing ran) writes here.
                // The pending proposal's own operation decides when the tab
                // is already closed (#698 review).
                let is_fleet = fleet || p.as_ref().is_some_and(|p| p.fleet_op.is_some());
                if let Some(p) = &p {
                    self.remember_fleet_ctx(&session_id, p);
                    self.last_consumed.insert(session_id.clone(), clone_pending(p));
                }
                if is_fleet && !denied {
                    return Vec::new();
                }
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
                } else if exit_code.is_none()
                    && duration_ms.is_none()
                    && p.as_ref()
                        .is_some_and(|p| is_shell_execution_tool(&p.tool_name))
                    && (output.starts_with("Error: read-only policy")
                        || output.starts_with("Error: refused to run a ~"))
                {
                    // The read-only allowlist / long-wait policy stopped the
                    // command without running it. Gated on the absence of
                    // execution metadata, the leading refusal text and a
                    // shell-execution tool, so neither a command that merely
                    // prints those words nor a log answer quoting them is
                    // misread (#698 review).
                    (AuditDecision::Denied, AuditDecisionSource::Policy)
                } else if let Some(p) = &p {
                    p.decision.unwrap_or_else(|| auto_decision(p.confirm_mode))
                } else if proposal_id.is_none() {
                    // No proposal at all: the user's own `!cmd` shell escape
                    // — their direct action, not a gate decision.
                    if denied {
                        (AuditDecision::Denied, AuditDecisionSource::User)
                    } else {
                        (AuditDecision::Approved, AuditDecisionSource::User)
                    }
                } else {
                    auto_decision(confirm_mode)
                };
                let mut rec =
                    self.record_from(&session_id, p.as_ref(), &target, &command, decision, source);
                rec.exit_code = exit_code;
                rec.duration_ms = duration_ms;
                // A shell escape that failed before producing a result has
                // no exit code or duration: keep the error text as its
                // explanation so the journal says what went wrong (#698).
                if proposal_id.is_none()
                    && exit_code.is_none()
                    && duration_ms.is_none()
                    && !output.is_empty()
                {
                    rec.explanation = output.clone();
                }
                self.hold_or_emit(rec)
            }
            AuditInput::Refused {
                session_id,
                proposal_id,
                reason,
            } => {
                // An explicit gate refusal (#698 review): the proposal never
                // reached the user and no finish follows. Only the named
                // proposal is consumed — a late refusal of an older one is
                // already journaled.
                let p = match self.pending.get(&session_id) {
                    Some(p) if p.proposal_id == Some(proposal_id) => {
                        self.pending.remove(&session_id)
                    }
                    _ => return Vec::new(),
                };
                let Some(p) = p else { return Vec::new() };
                self.remember_fleet_ctx(&session_id, &p);
                self.last_consumed.insert(session_id.clone(), clone_pending(&p));
                let mut rec = self.orphan_record(
                    &session_id,
                    p,
                    AuditDecision::Denied,
                    AuditDecisionSource::Policy,
                );
                rec.explanation = with_note(rec.explanation, &format!("refused by the gate: {reason}"));
                self.hold_or_emit(rec)
            }
            AuditInput::Cancelled {
                session_id,
                proposal_id,
            } => {
                let mut out = self.release_held(&session_id);
                match proposal_id {
                    Some(pid) => {
                        // Journal the pending proposal only when it is the
                        // one cancelled (#698 review).
                        let matches_pending = self
                            .pending
                            .get(&session_id)
                            .is_some_and(|p| p.proposal_id == Some(pid));
                        if matches_pending {
                            if let Some(p) = self.pending.remove(&session_id) {
                                self.remember_fleet_ctx(&session_id, &p);
                                out.push(self.orphan_record(
                                    &session_id,
                                    p,
                                    AuditDecision::Cancelled,
                                    AuditDecisionSource::User,
                                ));
                            }
                        } else if self
                            .last_consumed
                            .get(&session_id)
                            .is_some_and(|p| p.proposal_id == Some(pid))
                        {
                            // The command already finished, then the turn
                            // was cancelled (#698 review): journal the
                            // cancelled turn against that same command —
                            // the finish record alone would hide it.
                            if let Some(p) = self.last_consumed.get(&session_id) {
                                let mut rec = self.orphan_record(
                                    &session_id,
                                    clone_pending(p),
                                    AuditDecision::Cancelled,
                                    AuditDecisionSource::User,
                                );
                                rec.explanation = with_note(
                                    rec.explanation,
                                    "turn cancelled after this command finished",
                                );
                                out.push(rec);
                            }
                        }
                        // Otherwise the proposal's fate is already journaled
                        // (replaced or refused): nothing to add.
                    }
                    None => {
                        // The turn had not proposed anything: a pending
                        // proposal of a newer turn must not be consumed by
                        // this cancellation (#698 review).
                    }
                }
                out
            }
            AuditInput::TurnEnded { session_id } => {
                let mut out = self.release_held(&session_id);
                // A proposal still pending at turn end never got an outcome
                // event. An explicit refusal arrived as `Refused`; whatever
                // remains is unknown — keep a recorded decision, otherwise
                // journal it cancelled, never an inferred denial (#698).
                if let Some(p) = self.pending.remove(&session_id) {
                    self.remember_fleet_ctx(&session_id, &p);
                    let (decision, source) = p
                        .decision
                        .unwrap_or((AuditDecision::Cancelled, AuditDecisionSource::Policy));
                    let mut rec = self.orphan_record(&session_id, p, decision, source);
                    if rec.decision == AuditDecision::Cancelled {
                        rec.explanation = with_note(
                            rec.explanation,
                            "outcome unknown: the turn ended without a finish event",
                        );
                    }
                    out.push(rec);
                }
                out
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
                // Hold for a late rating like a finish (#698 review): the
                // write's own score may still be in flight.
                self.hold_or_emit(rec)
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
                // The operation's own proposal context (#698 review): the
                // kept ctx survives the finish consuming the pending, and a
                // newer proposal of the same session is never borrowed. The
                // live pending is used only when it belongs to this very
                // operation (the summary landed before the aggregate finish).
                let ctx = self.fleet_ctx.remove(&(session_id.clone(), op));
                let live = self
                    .pending
                    .get(&session_id)
                    .filter(|p| p.fleet_op == Some(op));
                let (decision, source) = ctx
                    .as_ref()
                    .and_then(|c| c.decision)
                    .or_else(|| live.and_then(|p| p.decision))
                    .unwrap_or_else(|| auto_decision(confirm_mode));
                let fallback = ctx
                    .as_ref()
                    .map(|c| c.target.clone())
                    .or_else(|| live.map(|p| p.target.clone()))
                    .unwrap_or_default();
                let explanation = ctx
                    .as_ref()
                    .map(|c| c.explanation.clone())
                    .or_else(|| {
                        live.map(|p| {
                            p.confirm_explanation
                                .clone()
                                .unwrap_or_else(|| p.explanation.clone())
                        })
                    })
                    .unwrap_or_default();
                let destructive = ctx
                    .as_ref()
                    .map(|c| c.destructive)
                    .or_else(|| live.map(|p| p.destructive))
                    .unwrap_or(false);
                let proposal_id = ctx
                    .as_ref()
                    .and_then(|c| c.proposal_id)
                    .or_else(|| live.and_then(|p| p.proposal_id));
                let risk_score = proposal_id.and_then(|pid| self.scores.get(&pid).copied());
                let arbiter_verdict = ctx
                    .as_ref()
                    .and_then(|c| c.arbiter_verdict.clone())
                    .or_else(|| live.and_then(|p| p.arbiter_verdict.clone()));
                let arbiter_reason = ctx
                    .as_ref()
                    .and_then(|c| c.arbiter_reason.clone())
                    .or_else(|| live.and_then(|p| p.arbiter_reason.clone()));
                let arbiter_model = ctx
                    .as_ref()
                    .and_then(|c| c.arbiter_model.clone())
                    .or_else(|| live.and_then(|p| p.arbiter_model.clone()));
                // Host targets: the live operation's when it still matches,
                // else the ones kept with the proposal — a summary after the
                // tab closed still journals real targets (#698 review).
                let hosts = if hosts.is_empty() {
                    ctx.as_ref()
                        .map(|c| c.hosts.clone())
                        .or_else(|| live.map(|p| p.hosts.clone()))
                        .unwrap_or_default()
                } else {
                    hosts
                };
                let mut out = Vec::new();
                let mut record = |host: &str,
                                  dec: AuditDecision,
                                  src: AuditDecisionSource,
                                  exit: Option<i32>,
                                  note: Option<String>| {
                    let target = hosts.get(host).cloned().unwrap_or_else(|| fallback.clone());
                    let mut rec =
                        AuditRecord::new(session_id.clone(), target, view.command.clone(), dec, src);
                    rec.explanation = match note {
                        Some(note) if !explanation.is_empty() => format!("{explanation}\n{note}"),
                        Some(note) => note,
                        None => explanation.clone(),
                    };
                    rec.destructive = destructive;
                    rec.proposal_id = proposal_id;
                    rec.risk_score = risk_score;
                    rec.arbiter_verdict = arbiter_verdict.clone();
                    rec.arbiter_reason = arbiter_reason.clone();
                    rec.arbiter_model = arbiter_model.clone();
                    rec.exit_code = exit;
                    out.push(rec);
                };
                // Group members answered; each keeps its own process exit
                // code (#698 review) — a typed answer (e.g. a Kubernetes
                // read) has no process behind it and so no exit code.
                for group in &view.groups {
                    for (i, host) in group.hosts.iter().enumerate() {
                        let exit = group.exits.get(i).copied().flatten();
                        record(host, decision, source, exit, None);
                    }
                }
                for d in &view.dropped {
                    // The host's state and reason go into the explanation:
                    // the decision enum has no error variant, but a dropped
                    // host's fate must still be readable (#698 review).
                    let mut note = format!("host result: {}", d.state.label());
                    if let Some(code) = d.exit_code {
                        note.push_str(&format!(" (exit {code})"));
                    }
                    if let Some(reason) = &d.reason {
                        note.push_str(&format!(": {reason}"));
                    }
                    let (dec, src, exit) = match d.state {
                        HostState::ExecutionError => (decision, source, d.exit_code),
                        HostState::AccessDenied => {
                            (AuditDecision::Denied, AuditDecisionSource::Policy, None)
                        }
                        // Timed out, unreachable, cancelled, disappeared,
                        // never asked: the host produced no result.
                        _ => (AuditDecision::Cancelled, AuditDecisionSource::Policy, None),
                    };
                    record(&d.host, dec, src, exit, Some(note));
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

    /// Journal a proposal that left the pending slot without an outcome
    /// event of its own (a gate refusal, a cancellation, shutdown).
    fn orphan_record(
        &mut self,
        session_id: &str,
        p: Pending,
        decision: AuditDecision,
        source: AuditDecisionSource,
    ) -> AuditRecord {
        let command = p.command.clone();
        let target = p.target.clone();
        self.record_from(session_id, Some(&p), &target, &command, decision, source)
    }

    /// Keep a fleet proposal's context for its summary (#698 review).
    fn remember_fleet_ctx(&mut self, session_id: &str, p: &Pending) {
        if let Some(op) = p.fleet_op {
            self.insert_fleet_ctx((session_id.to_string(), op), FleetCtx::from_pending(p));
        }
    }

    /// Insert a fleet context, bounded: a summary that never lands must
    /// not grow the map without end.
    fn insert_fleet_ctx(&mut self, key: (String, u64), ctx: FleetCtx) {
        if self.fleet_ctx.len() >= 256 && !self.fleet_ctx.contains_key(&key) {
            if let Some(oldest) = self.fleet_ctx.keys().next().cloned() {
                self.fleet_ctx.remove(&oldest);
            }
        }
        self.fleet_ctx.insert(key, ctx);
    }

    /// Hold a finished record for its in-flight rating (#698), or emit it
    /// when no rating is expected. Shared by command finishes, gate
    /// refusals and cluster-change traces.
    fn hold_or_emit(&mut self, rec: AuditRecord) -> Vec<AuditRecord> {
        if self.scoring && rec.risk_score.is_none() && rec.proposal_id.is_some() {
            // Bound the hold: a scorer that never answers must not grow it
            // without end (it never fills in practice).
            if self.held.len() >= 1024 {
                let oldest = self.held.remove(0);
                self.held.push(rec);
                return vec![oldest];
            }
            self.held.push(rec);
            return Vec::new();
        }
        vec![rec]
    }

    /// Release every held record of a session (its turn ended or was
    /// cancelled): their ratings have settled or been aborted (#698 review).
    fn release_held(&mut self, session_id: &str) -> Vec<AuditRecord> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.held.len() {
            if self.held[i].session_id == session_id {
                out.push(self.held.remove(i));
            } else {
                i += 1;
            }
        }
        out
    }

    /// Flush everything still in flight when the channel closes (shutdown):
    /// held records keep their fate; a proposal without an outcome honors a
    /// recorded user decision (the denial on quit, #698 review) and otherwise
    /// is journaled as cancelled — its fate is unknown.
    pub fn finish(&mut self) -> Vec<AuditRecord> {
        let mut out = std::mem::take(&mut self.held);
        let sessions: Vec<String> = self.pending.keys().cloned().collect();
        for sid in sessions {
            if let Some(p) = self.pending.remove(&sid) {
                let (decision, source) = p
                    .decision
                    .unwrap_or((AuditDecision::Cancelled, AuditDecisionSource::User));
                out.push(self.orphan_record(&sid, p, decision, source));
            }
        }
        out
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

/// Append a note to an explanation on its own line.
fn with_note(explanation: String, note: &str) -> String {
    if explanation.is_empty() {
        note.to_string()
    } else {
        format!("{explanation}\n{note}")
    }
}

/// A copy of a pending proposal for the consumed-outcome bookkeeping.
fn clone_pending(p: &Pending) -> Pending {
    p.clone()
}

/// The masking set of the journal (#698 review): every secret link, not
/// just the first that resolves a name. [`filar_core::ChainSecretProvider`]
/// keeps only the first value per name, so a later link's value for a
/// colliding name would leak into the journal. This provider enumerates
/// each link's names; a name listed by more than one link gets a synthetic
/// key (the name plus a `\u{2}` separator and the link index) for **every**
/// link that lists it, so each value is masked — including a nested chain's
/// inner links, which the runner flattens into the union.
///
/// A plain name resolves first-non-empty across the links, exactly as the
/// substitution chain resolves it (a link that does not know the name
/// errors, and the next one is tried); a total failure stays fail-closed
/// (the record is skipped). A synthetic duplicate key addresses one
/// specific link, and a link that cannot resolve it **fails the snapshot**:
/// silently masking nothing would let that link's value reach the journal.
pub struct MaskingUnion {
    links: Vec<Arc<dyn SecretProvider>>,
}

impl MaskingUnion {
    /// Build the union of `links`, in resolution order.
    pub fn new(links: Vec<Arc<dyn SecretProvider>>) -> Self {
        Self { links }
    }
}

impl SecretProvider for MaskingUnion {
    fn get(&self, name: &str) -> filar_core::Result<String> {
        // A synthetic duplicate key addresses one specific link; its
        // failure is fail-closed (#698 review) — no empty-value fallback.
        if let Some((base, idx)) = name.rsplit_once('\u{2}') {
            let link = idx
                .parse::<usize>()
                .ok()
                .and_then(|i| self.links.get(i))
                .ok_or_else(|| {
                    filar_core::CoreError::Secret(format!(
                        "masking link {idx} of {name} does not exist"
                    ))
                })?;
            return link.get(base);
        }
        // Plain name: first non-empty value wins, as the chain resolves it.
        let mut failures: Vec<String> = Vec::new();
        for link in &self.links {
            match link.get(name) {
                Ok(value) if !value.is_empty() => return Ok(value),
                Ok(_) => {}
                Err(e) => failures.push(e.to_string()),
            }
        }
        let detail = if failures.is_empty() {
            String::new()
        } else {
            format!(": {}", failures.join("; "))
        };
        Err(filar_core::CoreError::Secret(format!(
            "{name} not resolved by any of the {} masking links{detail}",
            self.links.len()
        )))
    }

    fn secret_names(&self) -> Vec<String> {
        // Count first: a name listed by several links needs a synthetic key
        // for every one of them — the first link's value must not ride on
        // the plain name while a later link's identical name hides it
        // (#698 review).
        let mut counts: HashMap<String, usize> = HashMap::new();
        let per_link: Vec<Vec<String>> = self
            .links
            .iter()
            .map(|link| {
                let names = link.secret_names();
                for name in &names {
                    *counts.entry(name.clone()).or_insert(0) += 1;
                }
                names
            })
            .collect();
        let mut out = Vec::new();
        for (idx, names) in per_link.into_iter().enumerate() {
            for name in names {
                if counts.get(&name).copied().unwrap_or(0) > 1 {
                    out.push(format!("{name}\u{2}{idx}"));
                } else {
                    out.push(name);
                }
            }
        }
        out
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
        proposed_in_mode(sid, pid, command, CommandConfirmMode::Always)
    }

    fn proposed_in_mode(
        sid: &str,
        pid: u64,
        command: &str,
        confirm_mode: CommandConfirmMode,
    ) -> AuditInput {
        AuditInput::Proposed {
            session_id: sid.into(),
            target: target(),
            proposal_id: pid,
            command: command.into(),
            explanation: "check the load".into(),
            destructive: false,
            confirm_mode,
            tool_name: "run_command".into(),
            fleet_op: None,
            hosts: HashMap::new(),
        }
    }

    fn finished(sid: &str, pid: u64, command: &str, denied: bool, output: &str) -> AuditInput {
        AuditInput::Finished {
            session_id: sid.into(),
            proposal_id: Some(pid),
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

    /// A proposal made on a fleet tab, carrying its operation id.
    fn proposed_fleet(sid: &str, pid: u64, command: &str, op: u64) -> AuditInput {
        let mut input = proposed(sid, pid, command);
        if let AuditInput::Proposed { fleet_op, .. } = &mut input {
            *fleet_op = Some(op);
        }
        input
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
                mode_switch: false,
            })
            .is_empty());
        let records = c.handle(finished("1", 7, "uptime", false, "load average: 0.1"));
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
            mode_switch: false,
        });
        let records = c.handle(finished("1", 1, "rm -rf /tmp/x", true, ""));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision, AuditDecision::Denied);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
        assert_eq!(records[0].exit_code, None);
    }

    #[test]
    fn a_confirmation_timeout_is_cancelled_by_policy() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "uptime"));
        let records = c.handle(finished("1", 1, "uptime", true, "Confirmation timed out"));
        assert_eq!(records[0].decision, AuditDecision::Cancelled);
        assert_eq!(records[0].decision_source, AuditDecisionSource::Policy);
    }

    #[test]
    fn a_gate_refusal_is_denied_by_policy() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "systemctl restart nginx"));
        let records = c.handle(finished(
            "1",
            1,
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
        // A gate refusal never reached the host: no exit code, no duration.
        let records = c.handle(AuditInput::Finished {
            session_id: "1".into(),
            proposal_id: Some(1),
            target: target(),
            command: "touch /tmp/x".into(),
            output: "Error: read-only policy: \"touch\" is not in the read-only allowlist — command not sent to the host: touch /tmp/x".into(),
            denied: false,
            exit_code: None,
            duration_ms: None,
            confirm_mode: CommandConfirmMode::Always,
            fleet: false,
        });
        assert_eq!(records[0].decision, AuditDecision::Denied);
        assert_eq!(records[0].decision_source, AuditDecisionSource::Policy);
    }

    #[test]
    fn ctrl_c_with_a_pending_proposal_is_journaled_as_cancelled() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "sleep 30"));
        let records = c.handle(AuditInput::Cancelled {
            session_id: "1".into(),
            proposal_id: Some(1),
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision, AuditDecision::Cancelled);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
        assert_eq!(records[0].command, "sleep 30");
        // Nothing pending left: a second cancellation writes nothing.
        assert!(c
            .handle(AuditInput::Cancelled {
                session_id: "1".into(),
                proposal_id: Some(1),
            })
            .is_empty());
    }

    #[test]
    fn an_auto_approved_command_records_the_allowlist() {
        let mut c = AuditCollector::default();
        c.handle(proposed_in_mode("1", 1, "ls", CommandConfirmMode::Allowlist));
        let records = c.handle(AuditInput::Finished {
            session_id: "1".into(),
            proposal_id: Some(1),
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
            proposal_id: None,
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
        c.handle(proposed_fleet("1", 1, "uname -r", op.id().as_u64()));
        c.handle(AuditInput::Decision {
            session_id: "1".into(),
            command: "uname -r".into(),
            approved: true,
            mode_switch: false,
        });
        let view = FleetView {
            operation: op.id(),
            command: "uname -r".into(),
            headline: "3 hosts answered".into(),
            groups: vec![ViewGroup {
                hosts: vec!["web-1".into(), "web-2".into()],
                sample: "5.15.0".into(),
                role: GroupRole::Baseline,
                exits: vec![Some(0), Some(0)],
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
                proposal_id: Some(1),
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
            mode_switch: false,
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
        let records = c.handle(finished("1", 1, "k8s_apply manifest.yaml", false, "scaled"));
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
        let (handle, task) = spawn(dir.clone(), provider, false);
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
            mode_switch: false,
        });
        handle.send(finished(
            "1",
            1,
            "curl -H 'Bearer hunter2-tui-secret' https://x",
            false,
            "ok",
        ));
        // Drop the sender and let the collector drain the queue and finish
        // its writes (the shutdown path, #698 review).
        drop(handle);
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("collector drained in time")
            .expect("collector task");
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

    /// A late finish of an older proposal must not consume the newer one
    /// pending in the same slot (#698 review): identity is the proposal id.
    #[test]
    fn a_late_finish_never_consumes_a_newer_proposal() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "uptime"));
        assert_eq!(c.handle(finished("1", 1, "uptime", false, "ok")).len(), 1);
        c.handle(proposed("1", 2, "date"));
        assert!(
            c.handle(finished("1", 1, "uptime", false, "ok")).is_empty(),
            "the late outcome of proposal 1 is dropped"
        );
        let records = c.handle(finished("1", 2, "date", false, "today"));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "date");
    }

    /// With a scorer configured, a finished record is held until its rating
    /// lands, then written carrying it (#698 review).
    #[test]
    fn a_late_rating_is_held_then_attached() {
        let mut c = AuditCollector::with_scoring(true);
        c.handle(proposed("1", 7, "uptime"));
        assert!(
            c.handle(finished("1", 7, "uptime", false, "ok")).is_empty(),
            "held for the in-flight rating"
        );
        let records = c.handle(AuditInput::Scored {
            proposal_id: 7,
            safety_pct: Some(90),
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].risk_score, Some(90));
        assert!(c.scores.is_empty(), "the rating is consumed with the record");
    }

    /// A turn end releases a held record even when no rating ever came.
    #[test]
    fn a_turn_end_releases_a_held_record_without_a_rating() {
        let mut c = AuditCollector::with_scoring(true);
        c.handle(proposed("1", 7, "uptime"));
        assert!(c.handle(finished("1", 7, "uptime", false, "ok")).is_empty());
        let records = c.handle(AuditInput::TurnEnded {
            session_id: "1".into(),
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].risk_score, None);
    }

    /// F2 switches the mode instead of answering: the command is cancelled,
    /// not deliberately denied (#698 review).
    #[test]
    fn a_mode_switch_cancels_rather_than_denies() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "uptime"));
        c.handle(AuditInput::Decision {
            session_id: "1".into(),
            command: "uptime".into(),
            approved: false,
            mode_switch: true,
        });
        let records = c.handle(finished("1", 1, "uptime", true, ""));
        assert_eq!(records[0].decision, AuditDecision::Cancelled);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
    }

    /// A shell escape that failed before a result keeps the error text as
    /// its explanation (#698 review).
    #[test]
    fn a_shell_escape_failure_keeps_the_error_text() {
        let mut c = AuditCollector::default();
        let records = c.handle(AuditInput::Finished {
            session_id: "1".into(),
            proposal_id: None,
            target: AuditTarget {
                kind: "local".into(),
                name: "local".into(),
                host: None,
                tags: Vec::new(),
            },
            command: "badcmd".into(),
            output: "Shell command failed: boom".into(),
            denied: false,
            exit_code: None,
            duration_ms: None,
            confirm_mode: CommandConfirmMode::Always,
            fleet: false,
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].explanation, "Shell command failed: boom");
        assert_eq!(records[0].decision, AuditDecision::Approved);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
    }

    /// A command that ran and merely printed the refusal words is not a
    /// policy denial: the sniff is gated on execution metadata (#698 review).
    #[test]
    fn a_printed_refusal_string_is_not_a_policy_denial() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "echo"));
        let records = c.handle(finished(
            "1",
            1,
            "echo",
            false,
            "Error: read-only policy is just text I printed",
        ));
        assert_eq!(records[0].decision, AuditDecision::Auto);
        assert_eq!(records[0].exit_code, Some(0));
    }

    /// A proposal left without an outcome event has an unknown fate: it is
    /// journaled as cancelled-by-policy with a note, never guessed as a
    /// denial (#698 review). An explicit gate refusal arrives as `Refused`.
    #[test]
    fn a_proposal_without_an_outcome_is_journaled_at_turn_end() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "k8s_scale deployment/web"));
        let records = c.handle(AuditInput::TurnEnded {
            session_id: "1".into(),
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision, AuditDecision::Cancelled);
        assert_eq!(records[0].decision_source, AuditDecisionSource::Policy);
        assert!(
            records[0].explanation.contains("outcome unknown"),
            "{}",
            records[0].explanation
        );
    }

    /// An explicit gate refusal (`CommandRefused`) journals a policy denial
    /// with the gate's reason, without waiting for the turn to end (#698).
    #[test]
    fn a_gate_refusal_journals_a_policy_denial_with_its_reason() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "rm -rf /"));
        let records = c.handle(AuditInput::Refused {
            session_id: "1".into(),
            proposal_id: 1,
            reason: "read-only policy".into(),
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision, AuditDecision::Denied);
        assert_eq!(records[0].decision_source, AuditDecisionSource::Policy);
        assert!(
            records[0].explanation.contains("refused by the gate: read-only policy"),
            "{}",
            records[0].explanation
        );
        // The turn end after the refusal writes nothing extra.
        assert!(c
            .handle(AuditInput::TurnEnded {
                session_id: "1".into(),
            })
            .is_empty());
    }

    /// Fleet records carry the arbiter verdict, each host's own exit code,
    /// and a dropped host's state and reason in the explanation (#698).
    #[test]
    fn a_fleet_record_carries_the_arbiter_and_per_host_exits() {
        let mut c = AuditCollector::default();
        let group = filar_core::HostGroup {
            name: "web".into(),
            match_tags: vec!["web".into()],
            ..Default::default()
        };
        let targets: Vec<filar_core::SshTarget> = ["web-1", "web-2", "web-3"]
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
        c.handle(proposed_fleet("1", 1, "uname -r", op.id().as_u64()));
        c.handle(AuditInput::Audited {
            session_id: "1".into(),
            verdict: "AGREE".into(),
            reason: "looks safe".into(),
            model: Some("arbiter-x".into()),
            unavailable: false,
        });
        c.handle(AuditInput::Decision {
            session_id: "1".into(),
            command: "uname -r".into(),
            approved: true,
            mode_switch: false,
        });
        let view = FleetView {
            operation: op.id(),
            command: "uname -r".into(),
            headline: "2 hosts answered".into(),
            groups: vec![ViewGroup {
                hosts: vec!["web-1".into(), "web-2".into()],
                sample: "5.15.0".into(),
                role: GroupRole::Baseline,
                exits: vec![Some(0), None],
            }],
            dropped: vec![ViewDropped {
                host: "web-3".into(),
                state: HostState::ExecutionError,
                exit_code: Some(2),
                reason: Some("boom".into()),
            }],
        };
        let records = c.handle(AuditInput::Fleet {
            session_id: "1".into(),
            view,
            hosts: HashMap::new(),
            confirm_mode: CommandConfirmMode::Always,
        });
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].exit_code, Some(0));
        assert_eq!(records[1].exit_code, None, "a typed answer has no process exit code");
        assert_eq!(records[0].arbiter_verdict.as_deref(), Some("AGREE"));
        assert_eq!(records[0].arbiter_model.as_deref(), Some("arbiter-x"));
        assert_eq!(records[2].exit_code, Some(2));
        assert!(
            records[2].explanation.contains("host result: error (exit 2): boom"),
            "{}",
            records[2].explanation
        );
    }

    /// The masking union masks every link's value for a colliding name, not
    /// just the first link's (#698 review).
    #[test]
    fn masking_union_masks_every_links_value_for_a_colliding_name() {
        let a = Arc::new(StaticSecretProvider::with_secrets(
            [("TOKEN".to_string(), "first-value".to_string())].into(),
        ));
        let b = Arc::new(StaticSecretProvider::with_secrets(
            [("TOKEN".to_string(), "second-value".to_string())].into(),
        ));
        let union = MaskingUnion::new(vec![a, b]);
        let names = union.secret_names();
        assert_eq!(names.len(), 2, "each link gets a synthetic key: {names:?}");
        assert!(
            !names.contains(&"TOKEN".to_string()),
            "the plain name would mask only the first link's value: {names:?}"
        );
        let snapshot = secret_snapshot(&union).expect("snapshot");
        let red = filar_core::secrets::redact_secrets("first-value and second-value", &snapshot);
        assert!(!red.contains("first-value"), "{red}");
        assert!(!red.contains("second-value"), "{red}");
    }

    /// Shutdown flushes held records and pending proposals alike (#698).
    #[test]
    fn finish_drains_held_records_and_pending_proposals() {
        let mut c = AuditCollector::with_scoring(true);
        c.handle(proposed("1", 7, "uptime"));
        assert!(c.handle(finished("1", 7, "uptime", false, "ok")).is_empty());
        c.handle(proposed("2", 8, "date"));
        let records = c.finish();
        assert_eq!(records.len(), 2);
        let uptime = records.iter().find(|r| r.command == "uptime").expect("held");
        assert_eq!(uptime.decision, AuditDecision::Auto);
        let date = records.iter().find(|r| r.command == "date").expect("pending");
        assert_eq!(date.decision, AuditDecision::Cancelled);
        assert_eq!(date.decision_source, AuditDecisionSource::User);
        assert!(c.finish().is_empty(), "a second drain writes nothing");
    }

    /// The denial recorded on quit is honored when the queue drains, even
    /// though the agent's own denied finish never lands (#698 review).
    #[test]
    fn finish_honors_a_recorded_denial_on_quit() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "rm -rf /tmp/x"));
        c.handle(AuditInput::Decision {
            session_id: "1".into(),
            command: "rm -rf /tmp/x".into(),
            approved: false,
            mode_switch: false,
        });
        let records = c.finish();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision, AuditDecision::Denied);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
    }

    /// A fleet summary that lands after its aggregate finish (and after a
    /// newer proposal started) still journals its own operation's context,
    /// not the newer proposal's (#698 review).
    #[test]
    fn a_late_fleet_summary_keeps_its_own_proposals_context() {
        let mut c = AuditCollector::default();
        let group = filar_core::HostGroup {
            name: "web".into(),
            match_tags: vec!["web".into()],
            ..Default::default()
        };
        let targets: Vec<filar_core::SshTarget> = ["web-1"]
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
        c.handle(proposed_fleet("1", 1, "uname -r", op.id().as_u64()));
        c.handle(AuditInput::Audited {
            session_id: "1".into(),
            verdict: "AGREE".into(),
            reason: String::new(),
            model: Some("arbiter-x".into()),
            unavailable: false,
        });
        c.handle(AuditInput::Decision {
            session_id: "1".into(),
            command: "uname -r".into(),
            approved: true,
            mode_switch: false,
        });
        // The aggregate finish consumes the pending proposal…
        assert!(c
            .handle(AuditInput::Finished {
                session_id: "1".into(),
                proposal_id: Some(1),
                target: target(),
                command: "uname -r".into(),
                output: "1 host answered".into(),
                denied: false,
                exit_code: Some(0),
                duration_ms: Some(100),
                confirm_mode: CommandConfirmMode::Always,
                fleet: true,
            })
            .is_empty());
        // …and a newer, denied proposal starts in the same session.
        c.handle(proposed("1", 2, "date"));
        c.handle(AuditInput::Decision {
            session_id: "1".into(),
            command: "date".into(),
            approved: false,
            mode_switch: false,
        });
        let view = FleetView {
            operation: op.id(),
            command: "uname -r".into(),
            headline: "1 host answered".into(),
            groups: vec![ViewGroup {
                hosts: vec!["web-1".into()],
                sample: "5.15.0".into(),
                role: GroupRole::Baseline,
                exits: vec![Some(0)],
            }],
            dropped: Vec::new(),
        };
        let records = c.handle(AuditInput::Fleet {
            session_id: "1".into(),
            view,
            hosts: HashMap::new(),
            confirm_mode: CommandConfirmMode::Always,
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].proposal_id, Some(1));
        assert_eq!(records[0].decision, AuditDecision::Approved);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
        assert_eq!(records[0].arbiter_verdict.as_deref(), Some("AGREE"));
        assert_eq!(records[0].command, "uname -r");
    }

    /// A cancellation naming an already-finished command still journals the
    /// cancelled turn against that command (#698 review).
    #[test]
    fn a_cancel_after_a_finished_command_journals_the_cancelled_turn() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 1, "uptime"));
        assert_eq!(c.handle(finished("1", 1, "uptime", false, "ok")).len(), 1);
        let records = c.handle(AuditInput::Cancelled {
            session_id: "1".into(),
            proposal_id: Some(1),
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "uptime");
        assert_eq!(records[0].decision, AuditDecision::Cancelled);
        assert_eq!(records[0].decision_source, AuditDecisionSource::User);
        assert!(
            records[0]
                .explanation
                .contains("turn cancelled after this command finished"),
            "{}",
            records[0].explanation
        );
    }

    /// A cancellation without a proposal id (the turn had not proposed
    /// anything) never consumes a newer turn's pending proposal (#698).
    #[test]
    fn a_cancel_without_a_proposal_id_spares_a_newer_pending() {
        let mut c = AuditCollector::default();
        c.handle(proposed("1", 2, "date"));
        assert!(c
            .handle(AuditInput::Cancelled {
                session_id: "1".into(),
                proposal_id: None,
            })
            .is_empty());
        let records = c.handle(finished("1", 2, "date", false, "today"));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "date");
    }

    /// A log answer quoting the refusal text is not a policy denial: the
    /// sniff only applies to shell-execution tools (#698 review).
    #[test]
    fn a_log_answer_quoting_the_refusal_text_is_not_a_denial() {
        let mut c = AuditCollector::default();
        let mut input = proposed("1", 1, "query_logs nginx");
        if let AuditInput::Proposed { tool_name, .. } = &mut input {
            *tool_name = "query_logs".into();
        }
        c.handle(input);
        let records = c.handle(AuditInput::Finished {
            session_id: "1".into(),
            proposal_id: Some(1),
            target: target(),
            command: "query_logs nginx".into(),
            output: "Error: read-only policy appears in this log line".into(),
            denied: false,
            exit_code: None,
            duration_ms: None,
            confirm_mode: CommandConfirmMode::Always,
            fleet: false,
        });
        assert_eq!(records.len(), 1);
        assert_ne!(records[0].decision, AuditDecision::Denied);
    }

    /// A pod fleet member journals the pod's own address, with the
    /// configured cluster's tags (#698 review).
    #[test]
    fn a_pod_fleet_member_journals_the_pods_address() {
        use filar_core::fleet_op::{FleetTarget, KubePodRef};
        let group = filar_core::HostGroup {
            name: "pods".into(),
            ..Default::default()
        };
        let op = filar_core::FleetOperation::from_targets(
            &group,
            &[FleetTarget::KubePod(KubePodRef {
                cluster: "prod-eu".into(),
                namespace: "shop".into(),
                pod: "web-7f9c".into(),
                container: "main".into(),
            })],
        );
        let clusters = vec![filar_core::KubeCluster {
            name: "prod-eu".into(),
            tags: vec!["prod".into()],
            ..Default::default()
        }];
        let hosts = fleet_host_targets(&op, &clusters);
        let target = hosts.values().next().expect("one member");
        assert_eq!(target.kind, "kube");
        assert_eq!(target.host.as_deref(), Some("pod/web-7f9c/main"));
        assert_eq!(target.tags, vec!["prod".to_string()]);
    }

    /// A synthetic duplicate key whose link cannot resolve it fails the
    /// whole snapshot — masking nothing would leak that link's value (#698).
    #[test]
    fn a_synthetic_masking_key_fails_closed() {
        struct BrokenLink;
        impl SecretProvider for BrokenLink {
            fn get(&self, _name: &str) -> filar_core::Result<String> {
                Err(filar_core::CoreError::Secret("unavailable".into()))
            }
            fn secret_names(&self) -> Vec<String> {
                vec!["TOKEN".into()]
            }
        }
        let good = Arc::new(StaticSecretProvider::with_secrets(
            [("TOKEN".to_string(), "good-value".to_string())].into(),
        ));
        let union = MaskingUnion::new(vec![good, Arc::new(BrokenLink)]);
        assert!(
            secret_snapshot(&union).is_err(),
            "a listed secret that cannot be resolved must abort the write"
        );
    }
}