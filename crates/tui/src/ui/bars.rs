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
            HelpItem { key: "^C", desc: "quit", action: Some(HelpAction::Quit) },
        ],
        AppMode::Thinking => vec![
            HelpItem { key: "ctrl+c", desc: "quit", action: Some(HelpAction::Quit) },
            HelpItem { key: "pgup/pgdn", desc: "scroll", action: None },
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

    // Right-align confirm_mode.
    let confirm_text = format!(" {:?}", app.confirm_mode);
    // left_len already includes mode-badge spans (pushed above), so we
    // must NOT add mode_len again — that would double-count and break
    // the right-alignment in non-Normal modes.
    let left_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let right_len = confirm_text.chars().count();
    let total = left_len + right_len;
    let available = area.width as usize;
    if total < available {
        let padding = available - total;
        spans.push(Span::raw(" ".repeat(padding)));
    }
    spans.push(Span::styled(confirm_text, app.theme.muted()));

    // Toast notification (e.g. "copied") — shown right-aligned after confirm_mode.
    if let Some(toast) = app.toast_text() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{} {}", glyphs.middle_dot, toast),
            app.theme.success_fg(),
        ));
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
