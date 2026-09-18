//! Status bar (top) and help bar (bottom).
//!
//! Both bars use no background fill — just text on the terminal background,
//! following `docs/DESIGN_PHILOSOPHY.md` §1 (минимум рамок).

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, AppMode, HelpAction};
use crate::ui::theme::Glyphs;

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
            HelpItem { key: "F2", desc: "safe", action: None },
            HelpItem { key: "F3", desc: "sessions", action: None },
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

/// Compact token figure for the status bar: `200000` → `200k`.
///
/// Rounds to the nearest thousand so a near-threshold reading does not
/// understate the fill (`199600` → `200k`).
fn format_tokens_compact(n: u64) -> String {
    if n >= 1000 {
        format!("{}k", n.saturating_add(500) / 1000)
    } else {
        n.to_string()
    }
}

/// Text of the context-fill indicator: `ctx [####----] 78k/200k`.
///
/// `used` is `last_prompt_tokens` — the measured prompt size of the most
/// recent request; `None` renders an empty scale and `—`, never `0` (#399).
/// A `threshold` of `0` (compaction disabled) has no denominator to draw a
/// scale against, so only the absolute figure is shown.
fn context_indicator_text(
    used: Option<u64>,
    threshold: u64,
    glyphs: &Glyphs,
    bar_cells: usize,
) -> String {
    let used_text = used
        .map(format_tokens_compact)
        .unwrap_or_else(|| "—".to_string());
    if threshold == 0 {
        return format!("ctx {used_text}");
    }
    let threshold_text = format_tokens_compact(threshold);
    if bar_cells == 0 {
        return format!("ctx {used_text}/{threshold_text}");
    }
    let filled = match used {
        // Floors, so the bar reads full only at the threshold. `u128` keeps a
        // pathological `threshold` from overflowing the product.
        Some(n) => {
            let n = n.min(threshold) as u128;
            (n * bar_cells as u128 / threshold as u128) as usize
        }
        None => 0,
    };
    let mut bar = String::with_capacity(bar_cells * 3);
    for i in 0..bar_cells {
        bar.push_str(if i < filled { glyphs.bar_full } else { glyphs.bar_empty });
    }
    format!("ctx [{bar}] {used_text}/{threshold_text}")
}

/// The context-fill segment for the status bar, including a one-column
/// trailing gap; with the leading space of `confirm_text` the indicator
/// stands two columns clear of `confirm_mode`, matching the toast gap.
///
/// Yields before `confirm_mode` and the toast do: the bar is tried at 8 and
/// 4 cells, then as an absolute pair without a scale, and finally dropped
/// (`None`) when even that does not fit `max_len`.
fn context_indicator_segment(
    used: Option<u64>,
    threshold: u64,
    glyphs: &Glyphs,
    max_len: usize,
) -> Option<String> {
    let tiers: &[usize] = if threshold == 0 { &[0] } else { &[8, 4, 0] };
    for &bar_cells in tiers {
        let text = context_indicator_text(used, threshold, glyphs, bar_cells);
        let segment = format!("{text} ");
        if segment.chars().count() <= max_len {
            return Some(segment);
        }
    }
    None
}

/// Render the status bar (top line).
///
/// Layout: `filar ▸ {alias host pwd}` on the left for SSH (`name pwd` when
/// local), mode indicator in the center (only for non-Normal modes),
/// context-fill indicator and `confirm_mode` on the right (muted).
pub(crate) fn render_status_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let glyphs = app.theme.glyphs();

    // Store area for hit-testing.
    app.status_bar_area = area;

    let mut spans = vec![
        Span::raw("filar "),
        Span::styled(glyphs.target_sep, app.theme.muted()),
        Span::raw(" "),
        Span::styled(
            app.status_target(),
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
        if cost > 0.0 {
            spans.push(Span::styled(
                format!("${:.4}", cost),
                app.theme.success_fg(),
            ));
        } else {
            spans.push(Span::styled("—", app.theme.muted()));
        }
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

    // Right side: an optional context-fill indicator, `confirm_mode`, then an
    // optional toast (e.g. "· copied") pinned to the far right. Space for the
    // indicator and the toast is reserved *before* the padding is computed —
    // otherwise the padding fills the whole line and the trailing spans,
    // pushed afterwards, start at column == width and get clipped by ratatui
    // (the original bug: the toast was never visible).
    //
    // The leading space below is deliberate: with the one-column trailing
    // gap of the context indicator it makes the two-column separation
    // before `confirm_mode`; when the indicator is dropped, the space stays
    // and keeps `confirm_mode` apart from the preceding text.
    let confirm_text = format!(" {:?}", app.confirm_mode);
    let confirm_style = if app.confirm_mode == filar_core::CommandConfirmMode::Explain {
        app.theme.muted().fg(app.theme.accent)
    } else {
        app.theme.muted()
    };
    // left_len already includes mode-badge spans (pushed above), so we
    // must NOT add mode_len again — that would double-count and break
    // the right-alignment in non-Normal modes.
    // Widths count terminal cells, not Unicode chars: a double-width glyph
    // (CJK) occupies two columns and must be budgeted as such, or the
    // right-aligned tail would be displaced on narrow terminals.
    let left_len: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let available = area.width as usize;

    // Owned copy drops the borrow on `app` immediately. The rendered toast is
    // a 2-space gap + `· <text>`.
    let toast_span_text = app
        .toast_text()
        .map(|t| format!("  {} {}", glyphs.middle_dot, t));
    let toast_len = toast_span_text
        .as_ref()
        .map(|s| UnicodeWidthStr::width(s.as_str()))
        .unwrap_or(0);

    // Context fill — the measured prompt size against the active profile's
    // compaction threshold, the same pair `maybe_request_compaction` compares
    // (#399). Display only: it neither arms nor fires compaction. Space for
    // the indicator is reserved before padding, exactly like the toast; on a
    // narrow terminal it yields first — needing one column of clearance from
    // the left text, it shrinks and then drops rather than crowd
    // `confirm_mode` or the toast.
    let used = app.active_session().last_prompt_tokens;
    let threshold = app.compact_at_tokens_for(&active);
    let confirm_len = UnicodeWidthStr::width(confirm_text.as_str());

    // Tags of the configured SSH target (#413), inserted into the target span
    // (spans[3], built above) between host and path. Their space is reserved
    // before padding, exactly like the indicator and the toast — and the
    // whole `[a,b]` segment yields when the line is too narrow: a truncated
    // list could silently hide a `prod` tag. When tags fit, `left_len` grows
    // so the indicator (the lowest-priority right-side element) shrinks to
    // compensate.
    let tags_budget = available.saturating_sub(left_len + confirm_len + toast_len + 1);
    let tags_segment = app.format_tags_segment(tags_budget);
    let left_len = if let Some(segment) = tags_segment {
        spans[3] = Span::styled(
            app.status_target_with_tags(Some(&segment)),
            app.theme.user_style(),
        );
        spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum()
    } else {
        left_len
    };

    let ctx_max = available.saturating_sub(left_len + confirm_len + toast_len + 1);
    let ctx_segment = context_indicator_segment(used, threshold, glyphs, ctx_max);
    let ctx_style = if used.is_some_and(|n| threshold > 0 && n >= threshold) {
        // The next request will compact — worth the warning colour.
        app.theme.warning_fg()
    } else {
        app.theme.muted()
    };

    let right_len = ctx_segment
        .as_ref()
        .map(|s| UnicodeWidthStr::width(s.as_str()))
        .unwrap_or(0)
        + confirm_len;
    // Toast has priority over padding on a narrow terminal (saturating — no
    // panic, toast may be clipped by ratatui if the line is too short).
    let padding = available.saturating_sub(left_len + right_len + toast_len);
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    if let Some(text) = ctx_segment {
        spans.push(Span::styled(text, ctx_style));
    }
    spans.push(Span::styled(confirm_text, confirm_style));
    if let Some(text) = toast_span_text {
        spans.push(Span::styled(text, app.theme.success_fg()));
    }

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    f.render_widget(Clear, area);
    f.render_widget(paragraph, area);
}

/// Render a horizontal separator line using the glyph set.
pub(crate) fn render_separator(f: &mut Frame, app: &App, area: Rect) {
    let glyphs = app.theme.glyphs();
    let sep: String = std::iter::repeat_n(glyphs.separator, area.width as usize).collect();
    let paragraph = Paragraph::new(sep).style(app.theme.muted());
    f.render_widget(Clear, area);
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

    /// Render the status bar into a `width`×1 test buffer.
    fn render_status_buffer(app: &mut App, width: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_status_bar(f, app, area);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// Render the status bar and return the visible text of the single row.
    fn render_status_row(app: &mut App, width: u16) -> String {
        let buffer = render_status_buffer(app, width);
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
                compact_at_tokens: filar_core::DEFAULT_COMPACT_AT_TOKENS,
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
                compact_at_tokens: filar_core::DEFAULT_COMPACT_AT_TOKENS,
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
    fn status_bar_zero_cost_shows_dash_not_dollar_zero() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.active_session_mut().cost_usd = Some(0.0);
        let row = render_status_row(&mut app, 120);
        assert!(!row.contains("$0"), "zero cost must not show $0.00, got: {row}");
        assert!(row.contains('—'), "zero cost must show dash, got: {row}");
    }

    #[test]
    fn status_bar_positive_cost_shows_dollar_amount() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.active_session_mut().cost_usd = Some(0.0123);
        let row = render_status_row(&mut app, 120);
        assert!(row.contains("$0.0123"), "positive cost, got: {row}");
    }

    #[test]
    fn status_bar_absent_cost_has_no_dollar_or_cost_dash_slot() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.active_session_mut().cost_usd = None;
        let row = render_status_row(&mut app, 120);
        assert!(!row.contains('$'), "absent cost must omit $ amount, got: {row}");
    }

    #[test]
    fn status_bar_ssh_shows_alias_host_and_pwd() {
        let mut app = App::new("prod".into(), CommandConfirmMode::Always);
        app.ssh_info = Some("root@10.0.0.5:22".into());
        app.cwd = Some("/srv".into());
        let row = render_status_row(&mut app, 120);
        assert!(row.contains("prod"), "alias, got: {row}");
        assert!(row.contains("10.0.0.5"), "host, got: {row}");
        assert!(row.contains("/srv"), "pwd, got: {row}");
    }

    /// App with the active SSH session `root@10.0.0.5:22`, an explicit cwd,
    /// and one configured target carrying tags (#413).
    fn app_with_tagged_target(tags: &[&str]) -> App {
        let mut app = App::new("prod".into(), CommandConfirmMode::Always);
        app.ssh_info = Some("root@10.0.0.5:22".into());
        app.cwd = Some("/srv".into());
        app.ssh_targets = vec![filar_core::SshTarget {
            name: "prod".into(),
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            auth: filar_core::SshAuth::Agent,
            host_key_policy: filar_core::HostKeyPolicy::Tofu,
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }];
        app
    }

    #[test]
    fn status_bar_shows_tags_between_host_and_pwd() {
        let mut app = app_with_tagged_target(&["work", "prod"]);
        let row = render_status_row(&mut app, 120);
        assert!(
            row.contains("10.0.0.5 [work,prod] /srv"),
            "tags segment must sit between host and pwd, got: {row}"
        );
        assert!(row.ends_with(" Always"), "right side must stay right-aligned, got: {row}");
    }

    #[test]
    fn status_bar_drops_tags_at_the_width_boundary_not_the_right_side() {
        // 58 = exact fit of the tag segment into the leftover budget;
        // 57 = one cell short — the whole segment must yield, the padded
        // confirm_mode must stay pinned to the right edge.
        let mut app = app_with_tagged_target(&["work", "prod"]);
        let row_fit = render_status_row(&mut app, 58);
        assert!(
            row_fit.contains("10.0.0.5 [work,prod] /srv"),
            "tags must appear when they exactly fit, got: {row_fit}"
        );
        assert!(row_fit.ends_with(" Always"), "got: {row_fit}");

        let mut app = app_with_tagged_target(&["work", "prod"]);
        let row_narrow = render_status_row(&mut app, 57);
        assert!(
            !row_narrow.contains("[work,prod]"),
            "tags must fully yield when they do not fit, got: {row_narrow}"
        );
        assert!(
            row_narrow.ends_with(" Always"),
            "confirm_mode must stay at the right edge after the drop, got: {row_narrow}"
        );
        assert_eq!(row_narrow.chars().count(), 57, "row must fill exactly 57 columns");
    }

    #[test]
    fn status_bar_wide_tag_budget_is_counted_in_cells() {
        // "[中]" is 3 chars but 4 terminal cells: the exact-fit boundary is
        // one column wider than for a 3-cell segment (the 11-cell
        // "[work,prod]" fits at 58, so a 4-cell segment fits at 58 − 11 + 4
        // = 51). At 50 a char-count budget (3 chars ≤ 3 cells) would have
        // admitted the segment and pushed the right side off the edge.
        let mut app = app_with_tagged_target(&["中"]);
        let row_fit = render_status_row(&mut app, 51);
        // One symbol per buffer cell: the second cell of the wide `中`
        // appears as a blank cell, hence `[中 ]` in the collected row.
        assert!(
            row_fit.contains("10.0.0.5 [中 ] /srv"),
            "tags must appear when they fit by cells, got: {row_fit}"
        );
        assert!(row_fit.ends_with(" Always"), "got: {row_fit}");
        assert_eq!(
            row_fit.chars().count(),
            51,
            "row must fill exactly 51 cells, got: {row_fit}"
        );

        let mut app = app_with_tagged_target(&["中"]);
        let row_narrow = render_status_row(&mut app, 50);
        assert!(
            !row_narrow.contains('中'),
            "wide tags must yield when a cell short, got: {row_narrow}"
        );
        assert!(
            row_narrow.ends_with(" Always"),
            "confirm_mode must stay at the right edge after the drop, got: {row_narrow}"
        );
        assert_eq!(
            row_narrow.chars().count(),
            50,
            "row must fill exactly 50 cells, got: {row_narrow}"
        );
    }

    #[test]
    fn status_bar_without_tags_is_unchanged() {
        // A tagged-built app stripped of the target match renders exactly like
        // the pre-#413 bar: no tags segment anywhere.
        let mut app = app_with_tagged_target(&["work"]);
        app.ssh_info = Some("root@10.0.0.9:22".into());
        let row = render_status_row(&mut app, 120);
        assert!(!row.contains("[work]"), "no tags segment for an unmatched host, got: {row}");
    }

    #[test]
    fn status_bar_local_still_shows_name() {
        let mut app = App::new("local".into(), CommandConfirmMode::Always);
        app.cwd = Some("/tmp".into());
        let row = render_status_row(&mut app, 120);
        assert!(row.contains("local"), "local name, got: {row}");
        assert!(row.contains("/tmp"), "local pwd, got: {row}");
    }

    #[test]
    fn profile_switch_restores_correct_status_bar_data() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.profiles = vec![
            filar_core::LlmProfile {
                name: "glm".into(), model: "z-ai/glm-5.2".into(), api_base_url: "".into(),
                max_tokens: 1024, key_env: "K".into(),
                temperature: None, top_p: None, extra_body: None,
                compact_at_tokens: filar_core::DEFAULT_COMPACT_AT_TOKENS,
            },
            filar_core::LlmProfile {
                name: "ds".into(), model: "deepseek-v3".into(), api_base_url: "".into(),
                max_tokens: 1024, key_env: "K".into(),
                temperature: None, top_p: None, extra_body: None,
                compact_at_tokens: filar_core::DEFAULT_COMPACT_AT_TOKENS,
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

    #[test]
    fn normal_mode_help_includes_f2() {
        let items = help_items(AppMode::Normal);
        let has_f2 = items.iter().any(|i| i.key == "F2" && i.desc == "safe");
        assert!(has_f2, "Normal mode help must include F2 safe");
    }

    /// A profile with the given compaction threshold, for status-bar tests.
    fn profile_with_threshold(name: &str, compact_at_tokens: u64) -> filar_core::LlmProfile {
        filar_core::LlmProfile {
            name: name.into(), model: "test-model".into(), api_base_url: "".into(),
            max_tokens: 1024, key_env: "K".into(),
            temperature: None, top_p: None, extra_body: None,
            compact_at_tokens,
        }
    }

    /// Configure `app` with one active profile at `threshold` tokens.
    ///
    /// `cwd` is cleared so the left side of the bar is deterministic (the
    /// process cwd would otherwise leak in via `App::new`).
    fn app_with_threshold(threshold: u64) -> App {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.cwd = None;
        app.profiles = vec![profile_with_threshold("glm", threshold)];
        app.active_session_mut().llm_profile = Some("glm".into());
        app
    }

    #[test]
    fn context_indicator_unknown_usage_shows_dash_and_empty_scale() {
        // `last_prompt_tokens = None` (no usage yet, or a restored session)
        // must render an empty scale and a dash — never 0% (#399).
        let mut app = app_with_threshold(200_000);
        app.active_session_mut().last_prompt_tokens = None;
        let row = render_status_row(&mut app, 120);
        let g = Glyphs::detect();
        let empty: String = std::iter::repeat_n(g.bar_empty, 8).collect();
        assert!(row.contains(&format!("ctx [{empty}] —/200k")), "got: {row}");
        assert!(!row.contains("0/200k"), "unknown usage is not zero, got: {row}");
    }

    #[test]
    fn context_indicator_fills_proportionally() {
        // 50k of 200k on an 8-cell bar → exactly 2 filled cells.
        let mut app = app_with_threshold(200_000);
        app.active_session_mut().last_prompt_tokens = Some(50_000);
        let row = render_status_row(&mut app, 120);
        let g = Glyphs::detect();
        let bar: String = std::iter::repeat_n(g.bar_full, 2)
            .chain(std::iter::repeat_n(g.bar_empty, 6))
            .collect();
        assert!(row.contains(&format!("ctx [{bar}] 50k/200k")), "got: {row}");
    }

    #[test]
    fn context_indicator_near_threshold_is_nearly_full() {
        // 190k of 200k → 7 of 8 cells: visibly close to the fold, but the bar
        // floors, so it reads full only once the threshold is reached.
        let mut app = app_with_threshold(200_000);
        app.active_session_mut().last_prompt_tokens = Some(190_000);
        let row = render_status_row(&mut app, 120);
        let g = Glyphs::detect();
        let bar: String = std::iter::repeat_n(g.bar_full, 7)
            .chain(std::iter::repeat_n(g.bar_empty, 1))
            .collect();
        assert!(row.contains(&format!("ctx [{bar}] 190k/200k")), "got: {row}");
    }

    #[test]
    fn context_indicator_over_threshold_stays_full() {
        let mut app = app_with_threshold(200_000);
        app.active_session_mut().last_prompt_tokens = Some(250_000);
        let row = render_status_row(&mut app, 120);
        let g = Glyphs::detect();
        let full: String = std::iter::repeat_n(g.bar_full, 8).collect();
        assert!(row.contains(&format!("ctx [{full}] 250k/200k")), "got: {row}");
    }

    #[test]
    fn context_indicator_warns_at_threshold_and_stays_muted_below() {
        // The colour is part of the contract, not just the text: the fill
        // flips to the warning tone the moment the measurement reaches the
        // threshold, and back below it.
        let mut app = app_with_threshold(200_000);
        for (used, expected, what) in [
            (200_000, app.theme.warning_fg().fg, "at the threshold"),
            (100_000, app.theme.muted().fg, "below the threshold"),
        ] {
            app.active_session_mut().last_prompt_tokens = Some(used);
            let buffer = render_status_buffer(&mut app, 120);
            let row: String = (0..120).map(|x| buffer[(x, 0)].symbol()).collect();
            let byte = row.find("ctx").unwrap_or_else(|| panic!("indicator shown: {row}"));
            let x = row[..byte].chars().count() as u16;
            assert_eq!(
                buffer[(x, 0)].style().fg,
                expected,
                "{what} the fill must paint the right colour, got: {row}"
            );
        }
    }

    #[test]
    fn context_indicator_without_scale_when_compaction_disabled() {
        // `compact_at_tokens = 0` → no denominator, absolute figure only.
        let mut app = app_with_threshold(0);
        app.active_session_mut().last_prompt_tokens = Some(15_235);
        let row = render_status_row(&mut app, 120);
        assert!(row.contains("ctx 15k"), "got: {row}");
        assert!(!row.contains("ctx ["), "no scale when there is no denominator, got: {row}");
        assert!(!row.contains("15k/"), "got: {row}");
    }

    #[test]
    fn context_indicator_keeps_confirm_mode_and_toast_right_aligned() {
        // With the indicator on, the reserved-width maths must still pin
        // `confirm_mode` and the toast to the right edge (#399).
        let mut app = app_with_threshold(200_000);
        app.active_session_mut().last_prompt_tokens = Some(50_000);
        app.toast = Some((
            "copied".to_string(),
            Instant::now() + Duration::from_secs(10),
        ));
        let row = render_status_row(&mut app, 120);
        let g = Glyphs::detect();
        assert!(row.contains("ctx ["), "indicator shown, got: {row}");
        let toast = format!("  {} copied", g.middle_dot);
        assert!(row.ends_with(&toast), "toast stays pinned to the right edge, got: {row}");
        let confirm = " Always";
        let expected = 120 - toast.chars().count() - confirm.chars().count();
        // `find` answers in bytes and the row holds multi-byte `—`, so
        // compare in characters.
        let byte_pos = row.find(confirm).expect("confirm_mode present");
        let char_pos = row[..byte_pos].chars().count();
        assert_eq!(char_pos, expected, "confirm_mode right-aligned before the toast, got: {row}");
    }

    #[test]
    fn context_indicator_dropped_before_confirm_on_narrow_terminal() {
        // Width 45: not even the shortest tier fits next to the left text and
        // `confirm_mode`, so the indicator yields — while `confirm_mode`
        // keeps its right-alignment.
        let mut app = app_with_threshold(200_000);
        app.active_session_mut().last_prompt_tokens = Some(150_000);
        let row = render_status_row(&mut app, 45);
        assert!(!row.contains("ctx"), "indicator must yield on a narrow terminal, got: {row}");
        assert!(row.ends_with(" Always"), "confirm_mode still right-aligned, got: {row}");
        assert_eq!(row.chars().count(), 45);
    }

    #[test]
    fn context_indicator_shrinks_tiers_to_fit() {
        // Width 66 leaves room for the 4-cell tier but not for the 8-cell
        // one, which is tried first and must give way before the indicator
        // drops entirely.
        let mut app = app_with_threshold(200_000);
        app.active_session_mut().last_prompt_tokens = Some(150_000);
        let row = render_status_row(&mut app, 66);
        let g = Glyphs::detect();
        let bar: String = std::iter::repeat_n(g.bar_full, 3)
            .chain(std::iter::repeat_n(g.bar_empty, 1))
            .collect();
        assert!(row.contains(&format!("ctx [{bar}] 150k/200k")), "4-cell tier expected, got: {row}");
    }

    #[test]
    fn context_indicator_renders_in_both_glyph_sets() {
        // The ASCII fallback and the Unicode set draw the same bar from their
        // own cells (explicitly, independent of terminal detection).
        let ascii = context_indicator_text(Some(50_000), 200_000, &Glyphs::ASCII, 8);
        assert_eq!(ascii, "ctx [##------] 50k/200k");
        let unicode = context_indicator_text(Some(50_000), 200_000, &Glyphs::UNICODE, 8);
        assert_eq!(unicode, "ctx [██░░░░░░] 50k/200k");
    }
}
