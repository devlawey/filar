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
        session_id: SessionId,
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

    /// Working directory for a tab (OSC 7 / later sync). Status bar only.
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

    /// The history was compacted: the head up to `boundary` is replaced by
    /// `summary`, or left untouched when `summary` is an `Err` (#377).
    ///
    /// `boundary` is echoed back from the request so the app cannot cut a
    /// history that changed while the summary was being produced.
    HistoryCompacted {
        session_id: SessionId,
        boundary: usize,
        summary: std::result::Result<String, String>,
    },

    /// A reactive compaction is about to be summarised — arm the session for
    /// the result that follows.
    ///
    /// The threshold path arms `pending_compaction` in the app before the run
    /// is spawned; the reactive path decides mid-run, so it has to say so.
    /// Without this the `HistoryCompacted` that follows is rejected as stale,
    /// the retry runs on a history the session never adopts, and every
    /// subsequent turn overflows again (#378).
    ///
    /// The channel is ordered, so this always lands before the result it
    /// arms, and the stale guard in `apply_compaction` still does its job:
    /// anything that replaces the history in between clears the flag again.
    CompactionStarted {
        session_id: SessionId,
        boundary: usize,
        /// The session's `history_epoch` when the run was spawned. The app
        /// refuses to arm if it has moved since.
        epoch: u64,
    },

    /// A note for the feed that does not end the run.
    ///
    /// Deliberately separate from [`AgentEvent::Error`], which the app treats
    /// as final: it clears the cancellation token and marks the agent idle.
    /// Reporting mid-run progress through that variant would leave a request
    /// in flight that the user can no longer cancel (#378).
    Notice {
        session_id: SessionId,
        text: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_request_carries_session_id() {
        let sid = SessionId(99);
        let (_tx, _rx) = oneshot::channel();
        let event = TuiEvent::ConfirmationRequest {
            session_id: sid,
            command: "ls".into(),
            explanation: "list".into(),
            destructive: false,
            respond_to: _tx,
        };
        if let TuiEvent::ConfirmationRequest { session_id, .. } = event {
            assert_eq!(session_id, SessionId(99));
        } else {
            panic!("expected ConfirmationRequest");
        }
    }

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
