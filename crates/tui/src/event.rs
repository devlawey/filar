//! Event types for communication between the agent and the TUI.
//!
//! The agent runs in a separate tokio task and communicates with the UI
//! via [`TuiEvent`] sent through an mpsc channel. Agent events are forwarded
//! from [`filar_agent::AgentEvent`] (emitted via the agent's `EventSink`),
//! while TUI-specific events (confirmation requests, transport changes) are
//! sent directly by TUI components.

use filar_agent::AgentEvent;
use tokio::sync::oneshot;

use crate::app::SessionId;

/// Events sent to the TUI.
///
/// Agent-originated events arrive as [`TuiEvent::Agent`] wrapping a
/// [`filar_agent::AgentEvent`]. TUI-specific variants handle concerns that
/// don't belong in the engine crate (oneshot channels, spinner state).
#[derive(Debug)]
pub enum TuiEvent {
    /// Forwarded agent event (from the `EventSink`).
    Agent {
        session_id: SessionId,
        event: AgentEvent,
    },

    /// The agent is calling the LLM (thinking). TUI-specific: drives the spinner.
    Thinking,

    /// The agent wants to execute a command and needs user confirmation.
    ///
    /// The UI must respond via the included [`oneshot::Sender`]:
    /// `true` = approve, `false` = deny.
    ConfirmationRequest {
        command: String,
        explanation: String,
        destructive: bool,
        respond_to: oneshot::Sender<bool>,
    },

    /// The transport was switched (e.g. from local to SSH).
    /// The runner uses this to update the system prompt for future agent calls.
    TransportChanged {
        is_local: bool,
        ssh_info: Option<String>,
    },
}
