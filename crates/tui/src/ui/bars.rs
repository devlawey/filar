//! Status bar (top) and help bar (bottom).
//!
//! Both bars use no background fill — just text on the terminal background,
//! following `docs/DESIGN_PHILOSOPHY.md` §1 (минимум рамок).

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, AppMode, HelpAction};

/// One clickable item in the help bar.
struct HelpItem {
    key: &'static str,
    desc: &'static str,
    action: Option<HelpAction>,
}

/// Return the help-bar items for the current mode.
fn help_items(mode: AppMode) -> Vec<HelpItem> {
    match mode {
        AppMode::Normal => vec![
            HelpItem { key: "enter", desc: "send", action: Some(HelpAction::Send) },
            HelpItem { key: "!", desc: "shell", action: Some(HelpAction::Shell) },
            HelpItem { key: "^T", desc: "terminal", action: Some(HelpAction::Terminal) },
            HelpItem { key: "^P", desc: "password", action: Some(HelpAction::Password) },
            HelpItem { key: "wheel", desc: "scroll", action: None },
            HelpItem { key: "click", desc: "expand", action: None },
            HelpItem { key: "drag", desc: "copy", action: None },
            HelpItem { key: "^C", desc: "quit", action: Some(HelpAction::Quit) },
        ],
        AppMode::Thinking => vec![
            HelpItem { key: "ctrl+c", desc: "quit", action: Some(HelpAction::Quit) },
            HelpItem { key: "wheel", desc: "scroll", action: None },
        ],
        AppMode::Confirming => vec![
            HelpItem { key: "tab", desc: "switch", action: Some(HelpAction::Switch) },
            HelpItem { key: "enter", desc: "confirm", action: Some(HelpAction::Confirm) },
            HelpItem { key: "a/y", desc: "approve", action: Some(HelpAction::Approve) },
            HelpItem { key: "d/n", desc: "deny", action: Some(HelpAction::Deny) },
            HelpItem { key: "ctrl+c", desc: "quit", action: Some(HelpAction::Quit) },
        ],
        AppMode::Interactive => vec![
            HelpItem { key: "ctrl+t", desc: "agent mode", action: Some(HelpAction::Terminal) },
            HelpItem { key: "wheel", desc: "scroll", action: None },
        ],
        AppMode::PasswordInput => vec![
            HelpItem { key: "enter", desc: "send password", action: Some(HelpAction::SendPassword) },
            HelpItem { key: "esc", desc: "cancel", action: Some(HelpAction::Cancel) },
            HelpItem { key: "ctrl+c", desc: "cancel", action: Some(HelpAction::Cancel) },
        ],
    }
}

/// Render the status bar (top line).
///
/// Layout: `filar ▸ {target}` on the left (accent on target name),
/// mode indicator in the center (only for non-Normal modes),
/// `confirm_mode` on the right (muted).
pub(crate) fn render_status_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let glyphs = app.theme.glyphs();

    // Store area for hit-testing.
    app.status_bar_area = area;

    let mut spans = vec![
        Span::raw("filar "),
        Span::styled(glyphs.target_sep, app.theme.muted()),
        Span::raw(" "),
        Span::styled(
            app.target_name.clone(),
            app.theme.user_style(),
        ),
    ];

    // Mode indicator — only shown for non-Normal modes.
    let mode_text = match app.mode {
        AppMode::Normal => None,
        AppMode::Thinking => {
            let spinner = app.spinner_char();
            Some(format!("{spinner} thinking"))
        }
        AppMode::Confirming => Some("confirm".to_string()),
        AppMode::Interactive => Some("interactive".to_string()),
        AppMode::PasswordInput => Some("password".to_string()),
    };

    if let Some(mt) = mode_text {
        let mode_color = app.theme.mode_color(app.mode);
        spans.push(Span::raw("   "));
        spans.push(Span::styled(mt, app.theme.mode_badge_style(mode_color)));
    }

    // Right side: `confirm_mode`, then an optional toast (e.g. "· copied")
    // pinned to the far right. Space for the toast is reserved *before* the
    // padding is computed — otherwise the padding fills the whole line and the
    // toast, pushed afterwards, starts at column == width and gets clipped by
    // ratatui (the original bug: the toast was never visible).
    let confirm_text = format!(" {:?}", app.confirm_mode);
    // left_len already includes mode-badge spans (pushed above), so we
    // must NOT add mode_len again — that would double-count and break
    // the right-alignment in non-Normal modes.
    let left_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let right_len = confirm_text.chars().count();

    // Owned copy drops the borrow on `app` immediately. The rendered toast is
    // a 2-space gap + `· <text>`.
    let toast_span_text = app
        .toast_text()
        .map(|t| format!("  {} {}", glyphs.middle_dot, t));
    let toast_len = toast_span_text
        .as_ref()
        .map(|s| s.chars().count())
        .unwrap_or(0);

    let available = area.width as usize;
    // Toast has priority over padding on a narrow terminal (saturating — no
    // panic, toast may be clipped by ratatui if the line is too short).
    let padding = available.saturating_sub(left_len + right_len + toast_len);
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    spans.push(Span::styled(confirm_text, app.theme.muted()));
    if let Some(text) = toast_span_text {
        spans.push(Span::styled(text, app.theme.success_fg()));
    }

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    f.render_widget(paragraph, area);
}

/// Render a horizontal separator line using the glyph set.
pub(crate) fn render_separator(f: &mut Frame, app: &App, area: Rect) {
    let glyphs = app.theme.glyphs();
    let sep: String = std::iter::repeat_n(glyphs.separator, area.width as usize).collect();
    let paragraph = Paragraph::new(sep).style(app.theme.muted());
    f.render_widget(paragraph, area);
}

/// Render the help bar (bottom line).
///
/// Keys in `fg_dim`, descriptions in `fg_muted`, separated by three spaces.
/// Clickable items store their Rect in `app.helpbar_zones` for hit-testing.
pub(crate) fn render_help_bar(f: &mut Frame, app: &mut App, area: Rect) {
    // Store area for hit-testing.
    app.help_bar_area = area;
    // Clear previous zones.
    app.helpbar_zones.clear();

    let items = help_items(app.mode);
    let mut spans: Vec<Span> = Vec::new();
    let mut col = area.x;

    // Leading whitespace (2 spaces, matching the reference layout).
    spans.push(Span::raw("  "));
    col += 2;

    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            // Three spaces between items.
            spans.push(Span::raw("   "));
            col += 3;
        }

        // Record the zone for clickable items.
        let item_start = col;

        // Key in fg_dim.
        spans.push(Span::styled(item.key, app.theme.dim()));
        col += item.key.chars().count() as u16;

        // Space between key and description.
        spans.push(Span::raw(" "));
        col += 1;

        // Description in fg_muted.
        spans.push(Span::styled(item.desc, app.theme.muted()));
        col += item.desc.chars().count() as u16;

        // Store the zone if this item has an action.
        if let Some(action) = item.action {
            let width = col.saturating_sub(item_start);
            app.helpbar_zones.push((
                Rect::new(item_start, area.y, width, 1),
                action,
            ));
        }
    }

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppMode};
    use filar_core::CommandConfirmMode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::{Duration, Instant};

    /// Render the status bar into a `width`×1 test buffer and return the visible
    /// text of the single row.
    fn render_status_row(app: &mut App, width: u16) -> String {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_status_bar(f, app, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..width).map(|x| buffer[(x, 0)].symbol()).collect()
    }

    #[test]
    fn active_toast_is_visible_in_status_bar() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.toast = Some((
            "copied".to_string(),
            Instant::now() + Duration::from_secs(10),
        ));
        let row = render_status_row(&mut app, 80);
        assert!(
            row.contains("copied"),
            "active toast should be visible, got: {row:?}"
        );
    }

    #[test]
    fn active_toast_visible_alongside_mode_badge() {
        // Guards the double-counting bug flagged near `left_len`: a mode badge
        // (non-Normal mode) is already included in `left_len`, so the toast must
        // still fit and render. Without the reserve-before-padding fix — or if
        // `mode_len` were added twice — the toast would be pushed off-screen.
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Confirming;
        app.toast = Some((
            "copied".to_string(),
            Instant::now() + Duration::from_secs(10),
        ));
        let row = render_status_row(&mut app, 80);
        assert!(
            row.contains("copied"),
            "toast should remain visible alongside a mode badge, got: {row:?}"
        );
    }

    #[test]
    fn expired_toast_is_absent_from_status_bar() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.toast = Some((
            "copied".to_string(),
            Instant::now() - Duration::from_secs(1),
        ));
        let row = render_status_row(&mut app, 80);
        assert!(
            !row.contains("copied"),
            "expired toast must not be rendered, got: {row:?}"
        );
    }

    #[test]
    fn narrow_terminal_does_not_panic_with_toast() {
        // 20 columns: left text + confirm_mode already exceed the width, so the
        // toast is clipped — but rendering must not panic (saturating padding).
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.toast = Some((
            "copied".to_string(),
            Instant::now() + Duration::from_secs(10),
        ));
        let row = render_status_row(&mut app, 20);
        assert_eq!(row.chars().count(), 20, "row must fill exactly 20 columns");
    }
}
