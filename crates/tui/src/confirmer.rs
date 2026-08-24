//! TUI-based implementation of [`CommandConfirmer`].
//!
//! [`TuiConfirmer`] sends confirmation requests to the TUI via an mpsc channel
//! and waits for the user's response. This allows the agent loop to run in a
//! separate task while the TUI handles the interactive confirmation dialog.

use tokio::sync::{mpsc, oneshot};
use tracing::debug;

use filar_agent::CommandConfirmer;
use filar_core::{CoreError, Result};

use crate::app::SessionId;
use crate::event::TuiEvent;

/// A [`CommandConfirmer`] that delegates to the TUI via channels.
pub struct TuiConfirmer {
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    session_id: SessionId,
}

impl TuiConfirmer {
    /// Create a confirmer bound to one session tab.
    pub fn new(event_tx: mpsc::UnboundedSender<TuiEvent>, session_id: SessionId) -> Self {
        Self {
            event_tx,
            session_id,
        }
    }
}

#[async_trait::async_trait]
impl CommandConfirmer for TuiConfirmer {
    async fn confirm(
        &self,
        command: &str,
        explanation: &str,
        destructive: bool,
    ) -> Result<bool> {
        debug!(command = %command, "TUI confirmer: sending confirmation request");

        let (respond_tx, respond_rx) = oneshot::channel();

        self.event_tx
            .send(TuiEvent::ConfirmationRequest {
                session_id: self.session_id,
                command: command.to_string(),
                explanation: explanation.to_string(),
                destructive,
                respond_to: respond_tx,
            })
            .map_err(|_| {
                CoreError::Other("failed to send confirmation request to TUI".into())
            })?;

        respond_rx.await.map_err(|_| {
            CoreError::Other("confirmation response channel closed".into())
        })
    }
}
