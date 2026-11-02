//! UI-agnostic agent events for external frontends.
//!
//! The [`AgentEvent`] enum is the public event contract between the agent
//! loop and any frontend (TUI, Telegram bot, mobile app).  It is emitted via
//! an [`EventSink`] — a simple callback closure set on [`AgentBuilder`].
//!
//! Frontends MUST handle the `_` catch-all arm because the enum is
//! `#[non_exhaustive]` — new variants may be added in future versions
//! without a breaking change.

use std::sync::Arc;

/// Events emitted by the agent during its processing loop.
///
/// This enum is **public API** — external frontends (Telegram bot, mobile
/// app) depend on it.  The `#[non_exhaustive]` attribute ensures that
/// frontends handle unknown variants gracefully.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AgentEvent {
    /// The agent started processing a user prompt.
    Started,

    /// A text chunk arrived during LLM streaming.
    ///
    /// The frontend should append this to the current streaming text block.
    TextDelta(String),

    /// The agent wants to execute a command and may need user confirmation.
    ///
    /// Emitted **before** the confirmer is called.  The frontend can use
    /// this to show a pending command block or update UI state.
    CommandProposed {
        /// Identifier of this proposal, unique within the process (#549).
        /// A later [`AgentEvent::CommandScored`] carries the same id, so a
        /// frontend can attach an asynchronous safety rating to its command.
        proposal_id: u64,
        /// The command to execute.
        command: String,
        /// Human-readable explanation of what the command does.
        explanation: String,
        /// Whether the command is flagged as destructive.
        destructive: bool,
        /// The tool call behind the command — name and original arguments
        /// (#525). Frontends keep it on the chat block so the history can be
        /// replayed to the model as a real tool call.
        tool: filar_core::ToolInvocation,
    },

    /// Independent arbiter verdict on a command awaiting confirmation.
    ///
    /// Emitted after [`CommandProposed`] and **before** the confirmer runs.
    /// The arbiter never auto-approves or auto-denies — this is advisory only.
    CommandAudited {
        /// Verdict label: `AGREE`, `MISMATCH`, `UNDERSTATED_RISK`,
        /// `CONTRADICTS_EVIDENCE`, or `UNAVAILABLE`.
        verdict: String,
        /// One-sentence reason (empty when `AGREE`).
        reason: String,
        /// Display name / slug of the arbiter model (if known).
        arbiter_model: Option<String>,
        /// `true` when the audit could not be completed (timeout, error, bad JSON).
        unavailable: bool,
    },

    /// Safety rating of a proposed command by the risk scorer (#549).
    ///
    /// Emitted asynchronously after the [`CommandProposed`] with the same
    /// `proposal_id` — possibly after its [`CommandFinished`], since scoring
    /// never delays confirmation or execution. Advisory only: it has no effect
    /// on whether the command runs. Only emitted when a scorer is configured,
    /// only for the agent's own commands, never after [`Cancelled`].
    ///
    /// [`CommandProposed`]: AgentEvent::CommandProposed
    /// [`CommandFinished`]: AgentEvent::CommandFinished
    /// [`Cancelled`]: AgentEvent::Cancelled
    CommandScored {
        /// The `proposal_id` of the rated command.
        proposal_id: u64,
        /// 0–100, where 100 = completely safe. `None` when unavailable.
        safety_pct: Option<u8>,
        /// The model's confidence in the rating, when reported.
        confidence: Option<f32>,
        /// The scorer's model.
        model: String,
        /// Cost of the rating in USD, when reported.
        cost_usd: Option<f64>,
        /// `true` when the rating could not be obtained (timeout, HTTP error,
        /// unexpected response).
        unavailable: bool,
    },

    /// A log query (`query_logs`, `pod_logs_history`) answered (#584) — for
    /// the UI only, never part of the conversation. Sent just before the
    /// matching [`AgentEvent::CommandFinished`], whose `command` it repeats.
    LogQueryFinished {
        /// The command line of the call, as in `CommandFinished`.
        command: String,
        /// Log source name.
        source: String,
        /// Its query language.
        language: String,
        /// The query sent to the store.
        query: String,
        /// Window start, RFC 3339 UTC.
        start: String,
        /// Window end, RFC 3339 UTC.
        end: String,
        /// Lines returned.
        lines: usize,
        /// More data existed than was returned.
        truncated: bool,
        /// The returned lines, redacted, at most [`crate::logs::tool::UI_LINES`],
        /// as `time message` — what the folded view stands for.
        raw: Vec<String>,
    },

    /// A command was executed (or did not run — see `denied`).
    CommandFinished {
        /// The command that was processed.
        command: String,
        /// Command output. When `denied`: empty if the user said no, or a
        /// short reason if the command was stopped without the user's
        /// answer (confirmation timed out; the fleet gate refused a write —
        /// #435).
        output: String,
        /// `true` if the command did not run: denied by the user, or refused
        /// before the user could be asked.
        denied: bool,
    },

    /// The agent finished and produced a final answer.
    Finished(String),

    /// Token usage reported by the LLM provider for the current exchange.
    /// Each call to the model produces one of these events (intermediate
    /// tool-call iterations each get their own).
    TokenUsage {
        tokens_in: u64,
        tokens_out: u64,
        /// Cost in USD (e.g. 0.0412). None when provider doesn't report.
        cost: Option<f64>,
        /// The actually served model slug. None when not reported.
        model: Option<String>,
        /// `true` when this usage is from the command arbiter, not the main agent loop.
        arbiter: bool,
    },

    /// The agent encountered an error (network, LLM, transport).
    Error(String),

    /// The agent was cancelled by the user via a `CancellationToken`.
    ///
    /// This is a terminal event — no further events will be emitted after it.
    /// The chat history remains consistent: any partial response or tool result
    /// produced before cancellation is preserved.
    Cancelled,
}

/// A callback for delivering [`AgentEvent`]s to a frontend.
///
/// Set via [`AgentBuilder::event_sink`](crate::AgentBuilder::event_sink).
/// If not set, events are silently dropped (no-op).
pub type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;
