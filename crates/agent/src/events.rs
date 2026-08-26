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
        /// The command to execute.
        command: String,
        /// Human-readable explanation of what the command does.
        explanation: String,
        /// Whether the command is flagged as destructive.
        destructive: bool,
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

    /// A command was executed (or denied by the user).
    CommandFinished {
        /// The command that was processed.
        command: String,
        /// Command output (empty if denied).
        output: String,
        /// `true` if the user denied the command.
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
