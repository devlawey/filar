//! Session-save progress overlay — modal window shown during Ctrl+S export.

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::app::App;

/// Render the session-save progress overlay.
///
/// Currently a stub — real rendering with progress bar implemented in issue #233.
pub(crate) fn render_save_overlay(_f: &mut Frame, _app: &App, _area: Rect) {}
