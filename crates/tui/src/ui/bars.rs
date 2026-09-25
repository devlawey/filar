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

/// Help-bar items in the fleet layer (#434): what works there, with `^O`
/// leading — it is the way to every single-host feature.
fn fleet_help_items() -> Vec<HelpItem> {
    vec![
        HelpItem { key: "^O", desc: "open host", action: None },
        HelpItem { key: "^N", desc: "tab", action: None },
        HelpItem { key: "^W", desc: "close fleet", action: None },
        HelpItem { key: "^Tab", desc: "tabs", action: None },
        HelpItem { key: "F1", desc: "help", action: None },
        HelpItem { key: "^Q", desc: "quit", action: Some(HelpAction::Quit) },
    ]
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

/// Status-bar counter of operations (#431): `ops ▸2 ✓1 ✗1`, zero states
/// omitted; `None` when there are no operations at all.
fn operations_counter(c: crate::ops::OpCounts, glyphs: &Glyphs) -> Option<String> {
    if c.total() == 0 {
        return None;
    }
    let mut out = String::from("ops");
    for (n, g) in [
        (c.running, glyphs.op_running),
        (c.done, glyphs.op_done),
        (c.failed, glyphs.op_failed),
        (c.cancelled, glyphs.op_cancelled),
    ] {
        if n > 0 {
            out.push_str(&format!(" {g}{n}"));
        }
    }
    Some(out)
}

/// Status-bar segments that give way when the line is too narrow (#439),
/// **first to go first**.
///
/// The bar used to decide this implicitly — each segment subtracted the
/// others from its own budget — which held for two flexible segments and
/// not for five. Now there is one order:
///
/// 1. the context-fill indicator: shrinks (8-cell bar, 4-cell, figures
///    only) and then goes — it is a gauge, not a fact to act on;
/// 2. the model slug;
/// 3. the token counter;
/// 4. the operations counter;
/// 5. the SSH target's tags — last to go, since a hidden `prod` tag is the
///    one omission that can mislead.
///
/// Never evicted: the target (in the fleet: the group and how many hosts
/// are answering), the mode badge, the session cost, `confirm_mode` and the
/// toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Evictable {
    Context,
    Model,
    Tokens,
    Ops,
    Tags,
}

/// See [`Evictable`].
const EVICTION_ORDER: [Evictable; 5] = [
    Evictable::Context,
    Evictable::Model,
    Evictable::Tokens,
    Evictable::Ops,
    Evictable::Tags,
];

/// The fleet's target in the status bar (#439): `fleet <group>`, and how
/// many of its hosts answered — ` · 3/12 live`, or `?/12` before the first
/// operation reports. Only glyphs of `glyphs`, so ASCII mode stays ASCII.
fn fleet_segment(
    group: &str,
    total: usize,
    status: Option<filar_agent::fleet_view::FleetStatus>,
    glyphs: &Glyphs,
) -> (String, String) {
    let count = match status {
        Some(st) => format!("{}/{}", st.answering, st.total),
        None => format!("?/{total}"),
    };
    (format!("fleet {group}"), format!(" {} {count} live", glyphs.middle_dot))
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

    let width = |s: &str| UnicodeWidthStr::width(s);
    let available = area.width as usize;

    // ── Every segment first, placed later ───────────────────────────
    // Which segment gives way on a narrow terminal is decided in one place
    // below (`EVICTION_ORDER`, #439), not by each segment's own formula.

    // Target: the host, or in the fleet the group and how many of its hosts
    // are answering (#439) — both kept to the last.
    let fleet_target = app.fleet().map(|f| (f.group_name().to_string(), f.len()));
    let (target, live) = match &fleet_target {
        Some((group, total)) => {
            let status = app.active_session().fleet_status;
            let (target, live) = fleet_segment(group, *total, status, glyphs);
            let style = match status {
                Some(st) if !st.running && st.answering < st.total => app.theme.warning_fg(),
                Some(st) if !st.running => app.theme.success_fg(),
                _ => app.theme.dim(),
            };
            (target, Some((live, style)))
        }
        None => (app.status_target(), None),
    };

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

    // Operations counter (#431): visible with the side panel closed and on
    // every tab, so a background job is not lost by switching away from it.
    let ops = operations_counter(app.operation_counts(), glyphs);

    // Token counter — per-profile breakdown from per_profile, not total.
    // Cost — total session sum. Model slug follows active profile.
    let active = app.llm_profile.clone().unwrap_or_else(|| app.default_profile_name.clone());
    let tokens = match app.per_profile.get(&active) {
        Some(pu) if pu.tokens_in > 0 || pu.tokens_out > 0 => {
            format!("toks: {}↑ {}↓", pu.tokens_in, pu.tokens_out)
        }
        _ => "toks: —".to_string(),
    };
    // The session's cost: in the fleet always shown (#439), `$?` until the
    // provider reports one; elsewhere only once the provider reported one.
    let cost = match app.cost_usd {
        Some(c) if c > 0.0 => Some((format!("${c:.4}"), app.theme.success_fg())),
        Some(_) => Some(("—".to_string(), app.theme.muted())),
        None if fleet_target.is_some() => Some(("$?".to_string(), app.theme.muted())),
        None => None,
    };
    // Model: per-profile served model if known, else configured model with ~ prefix.
    let model_display = match app.model_per_profile.get(&active) {
        Some(sm) => sm.to_string(),
        None => app
            .profiles
            .iter()
            .find(|p| p.name == active)
            .map(|p| format!("~{}", p.model))
            .unwrap_or_else(|| "~?".into()),
    };
    let model: String = if model_display.len() > 24 {
        model_display.chars().take(23).chain("…".chars()).collect()
    } else {
        model_display
    };

    // Right side: an optional context-fill indicator, `confirm_mode`, then an
    // optional toast (e.g. "· copied") pinned to the far right.
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
    // Owned copy drops the borrow on `app` immediately. The rendered toast is
    // a 2-space gap + `· <text>`.
    let toast_span_text = app
        .toast_text()
        .map(|t| format!("  {} {}", glyphs.middle_dot, t));

    // ── What stays and what goes ────────────────────────────────────
    // Widths count terminal cells, not Unicode chars: a double-width glyph
    // (CJK) occupies two columns and must be budgeted as such, or the
    // right-aligned tail would be displaced on narrow terminals.
    //
    // Never evicted: the brand and target (in the fleet: the group and its
    // answering count), the mode badge, the session cost, `confirm_mode`
    // and the toast. Everything else gives way in `EVICTION_ORDER`.
    let required = width("filar ")
        + width(glyphs.target_sep)
        + 1
        + width(&target)
        + live.as_ref().map_or(0, |(t, _)| width(t))
        + mode_text.as_ref().map_or(0, |m| 3 + width(m))
        + cost.as_ref().map_or(0, |(c, _)| width(c))
        + 3 // the gap that opens the usage group (tokens, cost, model)
        + width(&confirm_text)
        + toast_span_text.as_ref().map_or(0, |t| width(t));
    let mut budget = available.saturating_sub(required);

    // Kept in the reverse of `EVICTION_ORDER`: the most important optional
    // segment claims its room first. Each one is all-or-nothing, except the
    // context indicator, which shrinks before it goes. A segment that does
    // not fit gives way; a smaller, less important one may still take the
    // room that is left.
    let mut kept = std::collections::BTreeSet::new();
    let mut tags_segment = None;
    for segment in EVICTION_ORDER.iter().rev() {
        match segment {
            // Tags of the configured SSH target (#413), between host and
            // path. All or nothing: a truncated list could hide a `prod` tag.
            Evictable::Tags => {
                if let Some(t) = app.format_tags_segment(budget.saturating_sub(1)) {
                    budget -= width(&t) + 1;
                    tags_segment = Some(t);
                }
            }
            Evictable::Ops => {
                if let Some(o) = &ops {
                    let need = width(o) + 3;
                    if need <= budget {
                        budget -= need;
                        kept.insert(Evictable::Ops);
                    }
                }
            }
            Evictable::Tokens => {
                // Opens the usage group, whose gap is part of `required`;
                // a cost after it needs one more column to stand apart.
                let need = width(&tokens) + usize::from(cost.is_some());
                if need <= budget {
                    budget -= need;
                    kept.insert(Evictable::Tokens);
                }
            }
            Evictable::Model => {
                let need = width(&model) + 1;
                if need <= budget {
                    budget -= need;
                    kept.insert(Evictable::Model);
                }
            }
            // Context fill — the measured prompt size against the active
            // profile's compaction threshold (#399). Display only. It takes
            // whatever room is left, shrinking before it is dropped.
            Evictable::Context => {}
        }
    }
    let used = app.active_session().last_prompt_tokens;
    let threshold = app.compact_at_tokens_for(&active);
    let ctx_segment = context_indicator_segment(used, threshold, glyphs, budget.saturating_sub(1));
    let ctx_style = if used.is_some_and(|n| threshold > 0 && n >= threshold) {
        // The next request will compact — worth the warning colour.
        app.theme.warning_fg()
    } else {
        app.theme.muted()
    };

    // ── Assemble, left to right ─────────────────────────────────────
    let target = match (&fleet_target, &tags_segment) {
        (None, Some(t)) => app.status_target_with_tags(Some(t)),
        _ => target,
    };
    let mut spans = vec![
        Span::raw("filar "),
        Span::styled(glyphs.target_sep, app.theme.muted()),
        Span::raw(" "),
        Span::styled(target, app.theme.user_style()),
    ];
    if let Some((text, style)) = live {
        spans.push(Span::styled(text, style));
    }
    if let Some(mt) = mode_text {
        let mode_color = app.theme.mode_color(app.mode);
        spans.push(Span::raw("   "));
        spans.push(Span::styled(mt, app.theme.mode_badge_style(mode_color)));
    }
    if let (Some(counter), true) = (ops, kept.contains(&Evictable::Ops)) {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(counter, app.theme.dim()));
    }
    spans.push(Span::raw("   "));
    let mut usage_started = false;
    if kept.contains(&Evictable::Tokens) {
        spans.push(Span::styled(tokens, app.theme.muted()));
        usage_started = true;
    }
    if let Some((text, style)) = cost {
        if usage_started {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(text, style));
        usage_started = true;
    }
    if kept.contains(&Evictable::Model) {
        if usage_started {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(model, app.theme.dim()));
    }

    let left_len: usize = spans.iter().map(|s| width(s.content.as_ref())).sum();
    let right_len = ctx_segment.as_ref().map_or(0, |s| width(s)) + width(&confirm_text);
    let toast_len = toast_span_text.as_ref().map_or(0, |t| width(t));
    // Space for the right side is reserved before the padding is computed —
    // otherwise the padding fills the whole line and the trailing spans,
    // pushed afterwards, start at column == width and get clipped by
    // ratatui (the original bug: the toast was never visible).
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

    // The fleet layer does not advertise single-host keys it refuses (#434).
    let items = if app.in_fleet() && app.mode == AppMode::Normal {
        fleet_help_items()
    } else {
        help_items(app.mode)
    };
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

    #[test]
    fn operations_counter_omits_zero_states_and_hides_when_empty() {
        use crate::ops::OpCounts;
        assert_eq!(operations_counter(OpCounts::default(), &Glyphs::UNICODE), None);
        let c = OpCounts { running: 2, done: 1, failed: 0, cancelled: 3 };
        assert_eq!(operations_counter(c, &Glyphs::UNICODE).as_deref(), Some("ops ▸2 ✓1 ■3"));
        let c = OpCounts { running: 1, done: 1, failed: 1, cancelled: 1 };
        assert_eq!(operations_counter(c, &Glyphs::ASCII).as_deref(), Some("ops >1 +1 !1 ~1"));
    }

    #[test]
    fn operations_counter_shows_on_the_status_bar_with_the_panel_closed() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.refresh_operations_with(|_| {
            vec![filar_agent::background::JobSnapshot {
                job_id: "job-1".into(),
                command: "sleep 5".into(),
                state: filar_agent::background::JobState::Running,
                output_tail: String::new(),
                remote: false,
                output_settled: true,
            }]
        });
        assert!(!app.side_panel.open);
        let text = render_status_row(&mut app, 120);
        assert!(text.contains("ops"), "{text}");
    }

    #[test]
    fn the_fleet_help_bar_leads_with_ctrl_o_and_hides_refused_keys() {
        let items = fleet_help_items();
        assert_eq!(items[0].key, "^O");
        for refused in ["^T", "!", "^P"] {
            assert!(items.iter().all(|i| i.key != refused), "{refused} is refused in the fleet");
        }
    }

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

    // ── Fleet (#439) ────────────────────────────────────────────────

    fn fleet_status(answering: usize, running: bool) -> filar_agent::fleet_view::FleetStatus {
        filar_agent::fleet_view::FleetStatus {
            operation: crate::app::test_fleet_view().operation,
            answering,
            total: 3,
            running,
        }
    }

    #[test]
    fn fleet_status_bar_shows_group_live_count_and_cost() {
        let mut app = crate::app::test_fleet_app(crate::app::test_fleet_view());
        let row = render_status_row(&mut app, 120);
        assert!(row.contains("fleet web"), "group, got: {row}");
        assert!(row.contains("?/3 live"), "no operation yet, got: {row}");
        assert!(row.contains("$?"), "cost is shown before it is known, got: {row}");
        assert!(row.ends_with(" Always"), "right side stays right-aligned, got: {row}");
        assert_eq!(row.chars().count(), 120);

        app.cost_usd = Some(0.0123);
        let sid = app.sessions[app.active].id;
        for (answering, running) in [(0, true), (2, true), (2, false)] {
            app.handle_agent_event(crate::event::TuiEvent::FleetStatus {
                session_id: sid,
                status: fleet_status(answering, running),
            });
            let row = render_status_row(&mut app, 120);
            assert!(row.contains(&format!("{answering}/3 live")), "live count, got: {row}");
            assert!(row.contains("$0.0123"), "cost, got: {row}");
            assert!(row.ends_with(" Always"), "got: {row}");
        }
    }

    #[test]
    fn fleet_status_bar_keeps_the_essentials_at_80_columns() {
        let mut app = crate::app::test_fleet_app(crate::app::test_fleet_view());
        app.cost_usd = Some(1.5);
        app.mode = AppMode::Confirming;
        let profile = app.default_profile_name.clone();
        app.per_profile.insert(profile, Default::default());
        let sid = app.sessions[app.active].id;
        app.handle_agent_event(crate::event::TuiEvent::FleetStatus {
            session_id: sid,
            status: fleet_status(1, true),
        });
        for width in [80u16, 60, 55] {
            let row = render_status_row(&mut app, width);
            for essential in ["fleet web", "1/3 live", "confirm", "$1.5000"] {
                assert!(row.contains(essential), "{essential} at {width}, got: {row}");
            }
            assert!(row.ends_with(" Always"), "at {width}, got: {row}");
            assert_eq!(row.chars().count(), width as usize);
        }
        // 55 is exactly the essentials: the model and the token counter
        // have given way, the group, count, mode badge, cost and confirm mode stay.
        let row = render_status_row(&mut app, 55);
        assert!(!row.contains("toks"), "tokens yield before the essentials, got: {row}");
    }

    #[test]
    fn a_late_count_of_an_older_operation_is_ignored() {
        let mut app = crate::app::test_fleet_app(crate::app::test_fleet_view());
        let sid = app.sessions[app.active].id;
        // Each helper call opens a fresh operation, so the later id is newer.
        let older = fleet_status(0, true);
        let newer = fleet_status(3, false);
        assert!(newer.operation > older.operation, "ids grow");
        app.handle_agent_event(crate::event::TuiEvent::FleetStatus { session_id: sid, status: newer });
        app.handle_agent_event(crate::event::TuiEvent::FleetStatus { session_id: sid, status: older });
        assert_eq!(app.active_session().fleet_status, Some(newer));
    }

    #[test]
    fn the_fleet_segment_is_ascii_in_ascii_mode() {
        let (target, live) = fleet_segment("web", 12, None, &Glyphs::ASCII);
        assert_eq!(format!("{target}{live}"), "fleet web - ?/12 live");
        let (_, live) = fleet_segment(
            "web",
            12,
            Some(filar_agent::fleet_view::FleetStatus {
                answering: 11,
                total: 12,
                ..fleet_status(0, false)
            }),
            &Glyphs::ASCII,
        );
        assert!(live.is_ascii(), "{live}");
        assert_eq!(live, " - 11/12 live");
    }

    #[test]
    fn the_eviction_order_is_the_documented_one() {
        assert_eq!(
            EVICTION_ORDER,
            [
                Evictable::Context,
                Evictable::Model,
                Evictable::Tokens,
                Evictable::Ops,
                Evictable::Tags,
            ]
        );
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
        // 48 = exact fit of the tag segment next to the never-evicted
        // segments (tags are the last optional segment to go, #439);
        // 47 = one cell short — the whole segment must yield, the padded
        // confirm_mode must stay pinned to the right edge.
        let mut app = app_with_tagged_target(&["work", "prod"]);
        let row_fit = render_status_row(&mut app, 48);
        assert!(
            row_fit.contains("10.0.0.5 [work,prod] /srv"),
            "tags must appear when they exactly fit, got: {row_fit}"
        );
        assert!(row_fit.ends_with(" Always"), "got: {row_fit}");

        let mut app = app_with_tagged_target(&["work", "prod"]);
        let row_narrow = render_status_row(&mut app, 47);
        assert!(
            !row_narrow.contains("[work,prod]"),
            "tags must fully yield when they do not fit, got: {row_narrow}"
        );
        assert!(
            row_narrow.ends_with(" Always"),
            "confirm_mode must stay at the right edge after the drop, got: {row_narrow}"
        );
        assert_eq!(row_narrow.chars().count(), 47, "row must fill exactly 47 columns");
    }

    #[test]
    fn status_bar_wide_tag_budget_is_counted_in_cells() {
        // "[中]" is 3 chars but 4 terminal cells: the exact-fit boundary is
        // one column wider than for a 3-cell segment (the 11-cell
        // "[work,prod]" fits at 48, so a 4-cell segment fits at 48 − 11 + 4
        // = 41). At 40 a char-count budget (3 chars ≤ 3 cells) would have
        // admitted the segment and pushed the right side off the edge.
        let mut app = app_with_tagged_target(&["中"]);
        let row_fit = render_status_row(&mut app, 41);
        // One symbol per buffer cell: the second cell of the wide `中`
        // appears as a blank cell, hence `[中 ]` in the collected row.
        assert!(
            row_fit.contains("10.0.0.5 [中 ] /srv"),
            "tags must appear when they fit by cells, got: {row_fit}"
        );
        assert!(row_fit.ends_with(" Always"), "got: {row_fit}");
        assert_eq!(
            row_fit.chars().count(),
            41,
            "row must fill exactly 41 cells, got: {row_fit}"
        );

        let mut app = app_with_tagged_target(&["中"]);
        let row_narrow = render_status_row(&mut app, 40);
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
            40,
            "row must fill exactly 40 cells, got: {row_narrow}"
        );
    }

    #[test]
    fn status_bar_shows_policy_tightened_mode() {
        // #414: the bar renders the effective (policy-clamped) mode, not the
        // tab's own one. A tab on Allowlist over a `prod`-tagged target with
        // a prod→Always policy must show `Always` in the bar.
        let mut app = app_with_tagged_target(&["prod"]);
        app.global_confirm_mode = CommandConfirmMode::Allowlist;
        app.active_session_mut().confirm_mode = CommandConfirmMode::Allowlist;
        app.tag_policies = vec![filar_core::TagPolicy {
            tag: "prod".into(),
            confirm_mode: CommandConfirmMode::Always,
        }];
        app.sync_confirm_mode();
        let row = render_status_row(&mut app, 120);
        assert!(
            row.ends_with(" Always"),
            "policy-tightened mode must show in the bar, got: {row}"
        );

        // Counter-check: a non-matching tag leaves the tab's own mode in
        // place — the floor never opens nor closes anything by itself.
        let mut app = app_with_tagged_target(&["work"]);
        app.global_confirm_mode = CommandConfirmMode::Allowlist;
        app.active_session_mut().confirm_mode = CommandConfirmMode::Allowlist;
        app.tag_policies = vec![filar_core::TagPolicy {
            tag: "prod".into(),
            confirm_mode: CommandConfirmMode::Always,
        }];
        app.sync_confirm_mode();
        let row = render_status_row(&mut app, 120);
        assert!(
            row.ends_with(" Allowlist"),
            "without a matching policy the tab's own mode must stand, got: {row}"
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
