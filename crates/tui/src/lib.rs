//! TUI crate: terminal user interface built on `ratatui` + `crossterm`.
//!
//! Provides a chat-like interface for the agent:
//! - Chat history with visually distinct blocks (user, agent, command, error).
//! - Input field for typing messages.
//! - Confirmation dialog for command approval ([a]pprove / [d]eny).
//! - Status bar showing target, confirm mode, and current state.
//! - Scrollable history.

pub mod app;
pub mod confirmer;
pub mod event;
/// Tracing layer that mirrors WARN/ERROR log records into the chat.
pub mod log_layer;
pub mod runner;
pub mod terminal;
pub mod ui;

// Re-export key types.
pub use app::{App, AppMode};
pub use confirmer::TuiConfirmer;
pub use event::TuiEvent;
pub use log_layer::{chat_log_layer, ChatLogLayer};
pub use runner::{run, TuiConfig};
pub use terminal::{key_to_bytes, TerminalModel};
pub use filar_core::ChatBlock;
pub use ui::Theme;
