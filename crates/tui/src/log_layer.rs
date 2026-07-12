//! Custom `tracing` layer that forwards WARN/ERROR events into the chat.
//!
//! While the TUI is active, log records must never be written to the terminal
//! (they would be painted over the ratatui interface). All records go to a
//! file instead (configured in the binary). This layer is the *second* sink:
//! it mirrors WARN and ERROR events into the chat as `System` blocks so the
//! user sees important messages (e.g. an SSH disconnect) without leaving the
//! interface.
//!
//! The layer only holds an [`UnboundedSender<String>`]; the TUI runner owns
//! the matching receiver and polls it in its event loop, pushing each line as
//! a `System` block. Formatting is `target: message [fields]`, on a single
//! line and without a timestamp — the file log keeps the full record.

use std::fmt::Write as _;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// A [`Layer`] that forwards WARN/ERROR events to the chat via a channel.
pub struct ChatLogLayer {
    tx: UnboundedSender<String>,
}

/// Create a [`ChatLogLayer`] and its paired receiver.
///
/// Register the layer on the tracing subscriber and hand the receiver to the
/// TUI runner (via `TuiConfig`) so it can render forwarded log lines.
pub fn chat_log_layer() -> (ChatLogLayer, UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (ChatLogLayer { tx }, rx)
}

/// Collects the `message` field and any remaining fields from an event.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn push_field(&mut self, name: &str, value: &dyn std::fmt::Debug) {
        if name == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{name}={value:?}");
        }
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push_field(field.name(), value);
    }
}

impl<S: Subscriber> Layer<S> for ChatLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = *event.metadata().level();
        if level != Level::WARN && level != Level::ERROR {
            return;
        }

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let msg = visitor.message.trim();
        let mut line = format!("{}: ", event.metadata().target());
        line.push_str(msg);
        if !visitor.fields.is_empty() {
            if !msg.is_empty() {
                line.push(' ');
            }
            line.push_str(&visitor.fields);
        }

        // A chat System block is a single line — collapse any newlines.
        if line.contains(['\n', '\r']) {
            line = line.replace(['\n', '\r'], " ");
        }

        // Best-effort: if the receiver is gone (TUI already torn down), drop it.
        let _ = self.tx.send(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn forwards_only_warn_and_error() {
        let (layer, mut rx) = chat_log_layer();
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "ignored", "info message");
            tracing::debug!(target: "ignored", "debug message");
            tracing::warn!(target: "filar_transport::ssh", "reader: channel closed");
            tracing::error!(target: "agent", "boom {}", 42);
        });

        assert_eq!(
            rx.try_recv().unwrap(),
            "filar_transport::ssh: reader: channel closed"
        );
        assert_eq!(rx.try_recv().unwrap(), "agent: boom 42");
        assert!(rx.try_recv().is_err(), "info/debug must not be forwarded");
    }

    #[test]
    fn appends_extra_fields_on_one_line() {
        let (layer, mut rx) = chat_log_layer();
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "t", code = 7, "failed");
        });

        let line = rx.try_recv().unwrap();
        assert!(line.starts_with("t: failed"), "got: {line}");
        assert!(line.contains("code=7"), "got: {line}");
        assert!(!line.contains('\n'));
    }
}
