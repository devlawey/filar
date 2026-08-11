//! Session-save progress overlay — modal window shown during Ctrl+S export.

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::app::App;

pub(crate) fn render_save_overlay(_f: &mut Frame, _app: &App, _area: Rect) {
    // Stub — real rendering implemented in issue #233.
}
