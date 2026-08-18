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
    /// The runner uses this to update per-session connection info.
    TransportChanged {
        session_id: SessionId,
        is_local: bool,
        ssh_info: Option<String>,
        /// Optional alias to use for the status bar target_name.
        /// When set, overrides the ssh_info-derived name.
        alias: Option<String>,
    },

    /// Working directory for a tab (from remote `pwd` or OSC 7). Status bar only.
    CwdChanged {
        session_id: SessionId,
        cwd: String,
    },

    /// Ctrl+O encountered a password target with no cached password.
    /// The UI must switch to password entry mode.
    PasswordNeeded {
        session_id: SessionId,
        /// The target the user wants to connect to.
        target: filar_core::SshTarget,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_changed_carries_session_id() {
        let sid = SessionId(42);
        let event = TuiEvent::TransportChanged {
            session_id: sid,
            is_local: false,
            ssh_info: Some("root@10.0.0.5:22".into()),
            alias: None,
        };
        if let TuiEvent::TransportChanged { session_id, is_local, ref ssh_info, .. } = event {
            assert_eq!(session_id, SessionId(42));
            assert!(!is_local);
            assert_eq!(ssh_info.as_deref(), Some("root@10.0.0.5:22"));
        } else {
            panic!("expected TransportChanged");
        }
    }
}
