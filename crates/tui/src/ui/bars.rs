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
            HelpItem { key: "F1", desc: "help", action: None },
            HelpItem { key: "!", desc: "shell", action: Some(HelpAction::Shell) },
            HelpItem { key: "^T", desc: "terminal", action: Some(HelpAction::Terminal) },
            HelpItem { key: "^O", desc: "hosts", action: None },
            HelpItem { key: "^S", desc: "save", action: None },
            HelpItem { key: "^P", desc: "password", action: Some(HelpAction::Password) },
            HelpItem { key: "^N", desc: "tab", action: None },
            HelpItem { key: "^W", desc: "close", action: None },
            HelpItem { key: "wheel", desc: "scroll", action: None },
            HelpItem { key: "click", desc: "expand", action: None },
            HelpItem { key: "drag", desc: "copy", action: None },
            HelpItem { key: "^Q", desc: "quit", action: Some(HelpAction::Quit) },
        ],
        AppMode::Thinking => vec![
            HelpItem { key: "F1", desc: "help", action: None },
            HelpItem { key: "^Z", desc: "cancel", action: Some(HelpAction::CancelWork) },
            HelpItem { key: "^Q", desc: "quit", action: Some(HelpAction::Quit) },
            HelpItem { key: "wheel", desc: "scroll", action: None },
        ],
        AppMode::Confirming => vec![
            HelpItem { key: "tab", desc: "switch", action: Some(HelpAction::Switch) },
            HelpItem { key: "enter", desc: "confirm", action: Some(HelpAction::Confirm) },
            HelpItem { key: "a/y", desc: "approve", action: Some(HelpAction::Approve) },
            HelpItem { key: "d/n", desc: "deny", action: Some(HelpAction::Deny) },
            HelpItem { key: "^Z", desc: "deny", action: Some(HelpAction::CancelWork) },
            HelpItem { key: "^Q", desc: "quit", action: Some(HelpAction::Quit) },
        ],
        AppMode::Interactive => vec![
            HelpItem { key: "ctrl+t", desc: "agent mode", action: Some(HelpAction::Terminal) },
            HelpItem { key: "wheel", desc: "scroll", action: None },
        ],
        AppMode::PasswordInput => vec![
            HelpItem { key: "enter", desc: "send password", action: Some(HelpAction::SendPassword) },
            HelpItem { key: "esc", desc: "cancel", action: Some(HelpAction::Cancel) },
            HelpItem { key: "^Q", desc: "quit", action: Some(HelpAction::Quit) },
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

    // Token counter — per-profile breakdown from per_profile, not total.
    // Cost — total session sum. Model slug follows active profile.
    let active = app.llm_profile.clone().unwrap_or_else(|| app.default_profile_name.clone());
    let profile_usage = app.per_profile.get(&active);
    let served = app.model_per_profile.get(&active);
    spans.push(Span::raw("   "));
    if let Some(pu) = profile_usage {
        if pu.tokens_in > 0 || pu.tokens_out > 0 {
            spans.push(Span::styled(
                format!("toks: {}↑ {}↓", pu.tokens_in, pu.tokens_out),
                app.theme.muted(),
            ));
        } else {
            spans.push(Span::styled(
                "toks: —",
                app.theme.muted(),
            ));
        }
    } else {
        spans.push(Span::styled(
            "toks: —",
            app.theme.muted(),
        ));
    }
    if let Some(cost) = app.cost_usd {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("${:.4}", cost),
            app.theme.success_fg(),
        ));
    }
    // Model: per-profile served model if known, else configured model with ~ prefix.
    let model_display = if let Some(sm) = served {
        sm.to_string()
    } else {
        let configured = app.profiles.iter()
            .find(|p| p.name == active)
            .map(|p| format!("~{}", p.model))
            .unwrap_or_else(|| "~?".into());
        configured
    };
    spans.push(Span::raw(" "));
    let truncated: String = if model_display.len() > 24 {
        model_display.chars().take(23).chain("…".chars()).collect()
    } else {
        model_display
    };
    spans.push(Span::styled(truncated, app.theme.dim()));

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

    #[test]
    fn normal_mode_help_includes_close_tab() {
        let items = help_items(AppMode::Normal);
        let has_w = items.iter().any(|i| i.key == "^W" && i.desc == "close");
        assert!(has_w, "Normal mode help must include ^W close");
        let has_n = items.iter().any(|i| i.key == "^N");
        assert!(has_n, "Normal mode help must include ^N (existing check)");
        let has_f1 = items.iter().any(|i| i.key == "F1" && i.desc == "help");
        assert!(has_f1, "Normal mode help must include F1 help");
    }

    #[test]
    fn thinking_mode_help_includes_f1() {
        let items = help_items(AppMode::Thinking);
        let has_f1 = items.iter().any(|i| i.key == "F1" && i.desc == "help");
        assert!(has_f1, "Thinking mode help must include F1 help");
    }

    #[test]
    fn status_bar_shows_configured_model_with_tilde_when_no_response() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.profiles = vec![
            filar_core::LlmProfile {
                name: "glm".into(), model: "z-ai/glm-5.2".into(), api_base_url: "".into(),
                max_tokens: 1024, key_env: "K".into(),
                temperature: None, top_p: None, extra_body: None,
            },
        ];
        app.active_session_mut().llm_profile = Some("glm".into());
        let row = render_status_row(&mut app, 120);
        assert!(row.contains("~z-ai/glm-5.2"), "unconfirmed model must have ~ prefix, got: {row}");
    }

    #[test]
    fn status_bar_shows_served_model_without_tilde_after_response() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.profiles = vec![
            filar_core::LlmProfile {
                name: "glm".into(), model: "z-ai/glm-5.2".into(), api_base_url: "".into(),
                max_tokens: 1024, key_env: "K".into(),
                temperature: None, top_p: None, extra_body: None,
            },
        ];
        app.active_session_mut().llm_profile = Some("glm".into());
        app.active_session_mut().model_per_profile.insert("glm".into(), "openai/gpt-4o-mini".into());
        let row = render_status_row(&mut app, 120);
        assert!(row.contains("openai/gpt-4o-mini"), "served model must appear without ~, got: {row}");
        assert!(!row.contains("~openai"), "served model must NOT have ~, got: {row}");
    }

    #[test]
    fn status_bar_shows_per_profile_tokens_not_total() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.active_session_mut().tokens_in = 999;
        app.active_session_mut().tokens_out = 999;
        app.active_session_mut().per_profile.insert("glm".into(), filar_core::ProfileUsage {
            tokens_in: 50, tokens_out: 30,
        });
        app.active_session_mut().llm_profile = Some("glm".into());
        let row = render_status_row(&mut app, 120);
        assert!(row.contains("50↑ 30↓"), "must show per-profile tokens, got: {row}");
    }

    #[test]
    fn status_bar_shows_dash_when_no_profile_data() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.active_session_mut().per_profile.insert("glm".into(), filar_core::ProfileUsage {
            tokens_in: 0, tokens_out: 0,
        });
        app.active_session_mut().llm_profile = Some("glm".into());
        let row = render_status_row(&mut app, 120);
        assert!(row.contains("toks: —"), "zero tokens must show dash, got: {row}");
    }

    #[test]
    fn profile_switch_restores_correct_status_bar_data() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.profiles = vec![
            filar_core::LlmProfile {
                name: "glm".into(), model: "z-ai/glm-5.2".into(), api_base_url: "".into(),
                max_tokens: 1024, key_env: "K".into(),
                temperature: None, top_p: None, extra_body: None,
            },
            filar_core::LlmProfile {
                name: "ds".into(), model: "deepseek-v3".into(), api_base_url: "".into(),
                max_tokens: 1024, key_env: "K".into(),
                temperature: None, top_p: None, extra_body: None,
            },
        ];
        // Profile A: has served model and tokens
        app.active_session_mut().per_profile.insert("glm".into(), filar_core::ProfileUsage {
            tokens_in: 100, tokens_out: 50,
        });
        app.active_session_mut().model_per_profile.insert("glm".into(), "openai/gpt-4o".into());
        app.active_session_mut().llm_profile = Some("glm".into());
        let row_a = render_status_row(&mut app, 120);
        assert!(row_a.contains("100↑ 50↓") && row_a.contains("openai/gpt-4o"),
            "profile A must show its data, got: {row_a}");
        // Switch to B (no data yet)
        app.llm_profile = Some("ds".into());
        let row_b = render_status_row(&mut app, 120);
        assert!(row_b.contains("~deepseek-v3"), "profile B must show ~configured model, got: {row_b}");
        assert!(!row_b.contains("openai/gpt-4o"), "must not show A's model, got: {row_b}");
        // Switch back to A
        app.llm_profile = Some("glm".into());
        let row_a2 = render_status_row(&mut app, 120);
        assert!(row_a2.contains("openai/gpt-4o"), "back to A must restore A's model, got: {row_a2}");
    }

    #[test]
    fn normal_mode_help_includes_ctrl_o() {
        let items = help_items(AppMode::Normal);
        let has_o = items.iter().any(|i| i.key == "^O" && i.desc == "hosts");
        assert!(has_o, "Normal mode help must include ^O hosts");
    }

    #[test]
    fn normal_mode_help_includes_ctrl_s() {
        let items = help_items(AppMode::Normal);
        let has_s = items.iter().any(|i| i.key == "^S" && i.desc == "save");
        assert!(has_s, "Normal mode help must include ^S save");
    }
}
