//! Event types for communication between the agent and the TUI.
//!
//! The agent runs in a separate tokio task and communicates with the UI
//! via [`AgentEvent`] sent through an mpsc channel. Confirmation requests
//! carry a [`oneshot::Sender`] so the UI can send back the user's decision.

use tokio::sync::oneshot;

/// Events sent from the agent task to the TUI.
#[derive(Debug)]
pub enum AgentEvent {
    /// The agent started processing a user prompt.
    Started,

    /// The agent is calling the LLM (thinking).
    Thinking,

    /// A text chunk arrived during streaming.
    /// The UI should append this to the current streaming agent block.
    TextDelta(String),

    /// The agent produced a text response.
    TextResponse(String),

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

    /// A command was executed and produced output.
    CommandExecuted {
        command: String,
        output: String,
        approved: bool,
    },

    /// The agent finished and produced a final answer.
    Finished(String),

    /// The agent encountered an error.
    Error(String),

    /// The transport was switched (e.g. from local to SSH).
    /// The runner uses this to update the system prompt for future agent calls.
    TransportChanged {
        is_local: bool,
        ssh_info: Option<String>,
    },
}
