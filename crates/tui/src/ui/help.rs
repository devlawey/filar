//! Help overlay — modal window listing every shortcut and command.
//!
//! Both the bottom help bar (`bars.rs`) and this overlay are built from a
//! single command registry so the two never diverge.
//!
//! Entries unavailable in the current mode are shown dimmed rather than hidden
//! so the user can see the full set.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, AppMode};

// ---------------------------------------------------------------------------
// Command registry
// ---------------------------------------------------------------------------

/// One entry in the help registry.
#[derive(Debug, Clone)]
pub(crate) struct HelpEntry {
    /// Shortcut / key text (e.g. `"F1"`, `"^T"`, `"!cmd"`).
    pub key: &'static str,
    /// Human-readable description.
    pub desc: &'static str,
    /// Group name for the overlay sections.
    pub section: &'static str,
    /// Whether this entry is active/available in the given mode.
    pub available: fn(AppMode) -> bool,
}

/// Overlay copy that must render in Windows console fonts.
///
/// Windows console fonts often lack `⌘` (it shows as `?`). Keep macOS-only
/// glyphs out of the TUI on other platforms (#310).
fn overlay_desc_macos(macos: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        macos
    } else {
        other
    }
}

/// Return the full command registry — all entries, all modes.
///
/// The bottom help bar filters this by mode; the overlay shows everything,
/// dimming unavailable entries.
pub(crate) fn help_registry() -> Vec<HelpEntry> {
    vec![
        // ── Help ──────────────────────────────────────────────────────
        HelpEntry {
            key: "F1",
            desc: overlay_desc_macos(
                "Toggle this help overlay (macOS: often Fn+F1; Ctrl, not ⌘)",
                "Toggle this help overlay (Ctrl, not Cmd)",
            ),
            section: "Help",
            available: |_| true,
        },
        // ── Modes ─────────────────────────────────────────────────────
        HelpEntry {
            key: "^T",
            desc: "Toggle interactive terminal",
            section: "Modes",
            available: |m| m != AppMode::PasswordInput,
        },
        HelpEntry {
            key: "F2",
            desc: "Toggle safe mode: agent must justify each command and wait for confirmation; session is auto-saved to Markdown",
            section: "Modes",
            available: |m| m != AppMode::PasswordInput,
        },
        HelpEntry {
            key: "F3",
            desc: "Open session selection overlay (restore a saved session)",
            section: "Modes",
            available: |m| m != AppMode::PasswordInput,
        },
        HelpEntry {
            key: "^P",
            desc: "Enter password input mode",
            section: "Modes",
            available: |m| m == AppMode::Normal,
        },
        HelpEntry {
            key: "^J",
            desc: "Side panel: background operations → hosts, output tail. Up/Down select, Esc closes. Docked when the terminal is 120+ columns wide, a drawer below that",
            section: "Modes",
            available: |m| !matches!(m, AppMode::Interactive | AppMode::PasswordInput),
        },
        // ── Status bar ───────────────────────────────────────────────
        HelpEntry {
            key: "mode",
            desc: "Status bar shows the confirm mode (right side). Highlighted in accent color when safe mode is active",
            section: "Status bar",
            available: |_| true,
        },
        // ── Tabs ──────────────────────────────────────────────────────
        HelpEntry {
            key: "^N",
            desc: "New local tab",
            section: "Tabs",
            available: |m| m != AppMode::Interactive,
        },
        HelpEntry {
            key: "^W",
            desc: "Close active tab (in the fleet layer: close the fleet)",
            section: "Tabs",
            available: |m| m != AppMode::Interactive,
        },
        HelpEntry {
            key: "^Tab",
            desc: "Next tab",
            section: "Tabs",
            available: |m| m != AppMode::Interactive,
        },
        HelpEntry {
            key: "^Shift+Tab",
            desc: "Previous tab",
            section: "Tabs",
            available: |m| m != AppMode::Interactive,
        },
        HelpEntry {
            key: "^PgUp",
            desc: "Previous tab",
            section: "Tabs",
            available: |m| m != AppMode::Interactive,
        },
        HelpEntry {
            key: "^PgDn",
            desc: "Next tab",
            section: "Tabs",
            available: |m| m != AppMode::Interactive,
        },
        HelpEntry {
            key: "^1..^9",
            desc: "Switch to tab by number",
            section: "Tabs",
            available: |m| m != AppMode::Interactive,
        },
        // ── Agent ─────────────────────────────────────────────────────
        HelpEntry {
            key: "Enter",
            desc: "Send message to agent",
            section: "Agent",
            available: |m| m == AppMode::Normal,
        },
        HelpEntry {
            key: "^Z",
            desc: "Cancel agent / deny command",
            section: "Agent",
            available: |m| matches!(m, AppMode::Thinking | AppMode::Confirming),
        },
        HelpEntry {
            key: "Tab",
            desc: "Switch approve/deny",
            section: "Agent",
            available: |m| m == AppMode::Confirming,
        },
        HelpEntry {
            key: "a / y",
            desc: "Approve command",
            section: "Agent",
            available: |m| m == AppMode::Confirming,
        },
        HelpEntry {
            key: "d / n",
            desc: "Deny command",
            section: "Agent",
            available: |m| m == AppMode::Confirming,
        },
        // ── Scrolling ─────────────────────────────────────────────────
        HelpEntry {
            key: "PgUp",
            desc: "Scroll up",
            section: "Scrolling",
            available: |m| m != AppMode::Interactive,
        },
        HelpEntry {
            key: "PgDn",
            desc: "Scroll down",
            section: "Scrolling",
            available: |m| m != AppMode::Interactive,
        },
        HelpEntry {
            key: "End",
            desc: "Scroll to bottom",
            section: "Scrolling",
            available: |m| m != AppMode::Interactive,
        },
        HelpEntry {
            key: "wheel",
            desc: "Scroll",
            section: "Scrolling",
            available: |_| true,
        },
        // ── Copy ──────────────────────────────────────────────────────
        HelpEntry {
            key: "drag",
            desc: "Select text, copies on release (not if the app captured the mouse)",
            section: "Copy",
            available: |m| m != AppMode::PasswordInput,
        },
        // ── Input ─────────────────────────────────────────────────────
        HelpEntry {
            key: "^L",
            desc: "Cycle LLM profile for this tab",
            section: "Agent",
            available: |m| m == AppMode::Normal,
        },
        // ── Input ─────────────────────────────────────────────────────
        HelpEntry {
            key: "^V",
            desc: "Paste from clipboard",
            section: "Input",
            available: |m| matches!(
                m,
                AppMode::Normal | AppMode::Confirming | AppMode::PasswordInput
            ),
        },
        HelpEntry {
            key: "!cmd",
            desc: "Run shell command directly",
            section: "Input",
            available: |m| m == AppMode::Normal,
        },
        HelpEntry {
            key: "!ssh user@host",
            desc: "Connect tab to SSH host",
            section: "Input",
            available: |m| m == AppMode::Normal,
        },
        HelpEntry {
            key: "^O",
            desc: "Open host selection overlay (local, [[ssh_targets]], [[host_groups]] — a group enters the fleet layer). In the fleet: open one of its hosts in a new tab",
            section: "Input",
            available: |m| m == AppMode::Normal,
        },
        HelpEntry {
            key: "^S",
            desc: "Save current session to .md file",
            section: "Input",
            available: |m| m == AppMode::Normal,
        },
        HelpEntry {
            key: "/",
            desc: "At path-token start: open path picker (active target)",
            section: "Input",
            available: |m| m == AppMode::Normal,
        },
        HelpEntry {
            key: "^Shift+F",
            desc: "Open file picker (active target)",
            section: "Input",
            available: |m| m == AppMode::Normal,
        },
        HelpEntry {
            key: "^Shift+D",
            desc: "Open folder picker (active target)",
            section: "Input",
            available: |m| m == AppMode::Normal,
        },
        HelpEntry {
            key: "Up / Down",
            desc: "Browse input history",
            section: "Input",
            available: |m| m == AppMode::Normal,
        },
        // ── Exit ──────────────────────────────────────────────────────
        HelpEntry {
            key: "^Q",
            desc: "Quit filar",
            section: "Exit",
            available: |_| true,
        },
    ]
}

// ---------------------------------------------------------------------------
// Overlay render
// ---------------------------------------------------------------------------

/// Maximum width of the help overlay (chars).  Caps the actual width to
/// `term_width - 2*margin` so it never fills the full screen.
const OVERLAY_MAX_WIDTH: u16 = 60;
const OVERLAY_H_MARGIN: u16 = 4;
const OVERLAY_V_MARGIN: u16 = 2;

/// Render the help overlay as a centred modal window.
///
/// The overlay clears the area behind it (`Clear` widget) and draws a
/// bordered block with the full command registry grouped by section.
/// Unavailable entries are dimmed.
pub(crate) fn render_help_overlay(f: &mut Frame, app: &App, area: Rect) {
    let width = OVERLAY_MAX_WIDTH.min(area.width.saturating_sub(2 * OVERLAY_H_MARGIN));
    let height = area.height.saturating_sub(2 * OVERLAY_V_MARGIN);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + OVERLAY_V_MARGIN;

    let overlay_area = Rect::new(x, y, width, height);

    // Clear the area behind the overlay.
    f.render_widget(Clear, overlay_area);

    let block_frame = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg));
    let inner_area = block_frame.inner(overlay_area);
    let inner_width = inner_area.width as usize;

    let lines = overlay_lines(app, inner_width);

    let visible = inner_area.height as usize;
    let total = lines.len();
    let max_scroll = total.saturating_sub(visible);
    let scroll = (app.help_scroll as usize).min(max_scroll) as u16;

    // Title with scroll indicator if content overflows.
    let title = if total > visible {
        format!(
            " Help (F1/Esc close, {}/{}, PgUp/PgDn scroll) ",
            scroll.saturating_add(1),
            total
        )
    } else {
        " Help (F1 or Esc to close) ".into()
    };

    let block = block_frame.title(title);

    // No `Wrap`: the rows are already cut to the inner width, so the widget
    // cannot re-wrap them from column zero.
    let paragraph = Paragraph::new(lines).block(block).scroll((scroll, 0));

    f.render_widget(paragraph, overlay_area);
}

/// Every row of the overlay, each cut to `inner_width` cells: one element
/// is exactly one row on screen, so the scroll and the title count rows.
fn overlay_lines(app: &App, inner_width: usize) -> Vec<Line<'static>> {
    let registry = help_registry();
    let mut lines: Vec<Line> = Vec::new();
    let mut current_section: Option<&str> = None;

    for entry in &registry {
        if current_section != Some(entry.section) {
            if current_section.is_some() {
                lines.push(Line::raw("")); // blank line between sections
            }
            lines.push(Line::from(Span::styled(
                format!(" {} ", entry.section),
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            current_section = Some(entry.section);
        }

        let available = (entry.available)(app.mode);
        let key_style = if available {
            app.theme.dim()
        } else {
            app.theme.muted()
        };
        let desc_style = if available {
            app.theme.fg_style()
        } else {
            app.theme.muted()
        };
        push_entry_lines(&mut lines, entry, inner_width, key_style, desc_style);
    }

    let usage_from = lines.len();
    append_usage_summary(&mut lines, app);

    // The usage summary is free text: wrap it too, so every element of
    // `lines` is exactly one row on screen and the scroll counts rows.
    let usage: Vec<Line> = lines.split_off(usage_from);
    for line in usage {
        let style = line.spans.first().map(|s| s.style).unwrap_or_default();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if text.is_empty() {
            lines.push(Line::raw(""));
            continue;
        }
        let indent = text.len() - text.trim_start().len();
        let room = inner_width.saturating_sub(indent).max(1);
        for row in wrap_words(text.trim_start(), room) {
            lines.push(Line::from(Span::styled(format!("{}{row}", " ".repeat(indent)), style)));
        }
    }
    lines
}

/// Width of the key column: keys are right-aligned in it.
const KEY_WIDTH: usize = 14;
/// Column where descriptions start: margin, key column, gap.
const DESC_COL: usize = 2 + KEY_WIDTH + 2;
/// Narrowest description column worth laying out next to the keys. Below
/// it the key gets a row of its own and the description goes underneath.
const MIN_DESC_WIDTH: usize = 20;
/// Indent of a description placed under its key (narrow overlay).
const STACKED_INDENT: usize = 4;

/// The rows of one registry entry, cut to `inner_width` cells.
///
/// The description wraps at word boundaries inside its own column, so a
/// continuation starts under the first character of the description and the
/// key column stays empty (#500). When the overlay is too narrow for that
/// column, the key takes a row and the description is wrapped below it.
fn push_entry_lines(
    lines: &mut Vec<Line<'static>>,
    entry: &HelpEntry,
    inner_width: usize,
    key_style: Style,
    desc_style: Style,
) {
    let desc_width = inner_width.saturating_sub(DESC_COL);
    if desc_width >= MIN_DESC_WIDTH {
        let pad = KEY_WIDTH.saturating_sub(entry.key.width());
        for (i, row) in wrap_words(entry.desc, desc_width).into_iter().enumerate() {
            let key = if i == 0 {
                format!("  {}{}  ", " ".repeat(pad), entry.key)
            } else {
                " ".repeat(DESC_COL)
            };
            lines.push(Line::from(vec![
                Span::styled(key, key_style),
                Span::styled(row, desc_style),
            ]));
        }
        return;
    }
    for row in wrap_words(entry.key, inner_width.saturating_sub(2).max(1)) {
        lines.push(Line::from(Span::styled(format!("  {row}"), key_style)));
    }
    let room = inner_width.saturating_sub(STACKED_INDENT).max(1);
    let indent = " ".repeat(STACKED_INDENT.min(inner_width.saturating_sub(1)));
    for row in wrap_words(entry.desc, room) {
        lines.push(Line::from(Span::styled(format!("{indent}{row}"), desc_style)));
    }
}

/// Wrap `text` to `width` terminal cells at word boundaries. A word wider
/// than the whole width is cut; nothing is ever wider than `width`.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let fits = if current.is_empty() {
            word.width() <= width
        } else {
            current.width() + 1 + word.width() <= width
        };
        if fits {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            continue;
        }
        if !current.is_empty() {
            rows.push(std::mem::take(&mut current));
        }
        if word.width() <= width {
            current.push_str(word);
        } else {
            let mut pieces = crate::ui::text::wrap_text(word, width);
            current = pieces.pop().unwrap_or_default();
            rows.extend(pieces);
        }
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows
}

/// Append session token usage summary (including arbiter) to the help overlay.
fn append_usage_summary(lines: &mut Vec<Line<'static>>, app: &App) {
    let active = app
        .llm_profile
        .clone()
        .unwrap_or_else(|| app.default_profile_name.clone());
    let profile_usage = app.per_profile.get(&active);

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " Usage ",
        Style::default()
            .fg(app.theme.accent)
            .add_modifier(Modifier::BOLD),
    )));

    if let Some(pu) = profile_usage {
        if pu.tokens_in > 0 || pu.tokens_out > 0 {
            lines.push(Line::from(Span::styled(
                format!("  Session ({active}): {}↑ {}↓", pu.tokens_in, pu.tokens_out),
                app.theme.fg_style(),
            )));
        }
    } else if app.tokens_in > 0 || app.tokens_out > 0 {
        lines.push(Line::from(Span::styled(
            format!("  Session: {}↑ {}↓", app.tokens_in, app.tokens_out),
            app.theme.fg_style(),
        )));
    }

    if app.arbiter_tokens_in > 0 || app.arbiter_tokens_out > 0 {
        let mut arb = format!(
            "  Arbiter: {}↑ {}↓",
            app.arbiter_tokens_in, app.arbiter_tokens_out
        );
        if let Some(cost) = app.arbiter_cost_usd {
            if cost > 0.0 {
                arb.push_str(&format!(" ${cost:.4}"));
            }
        }
        lines.push(Line::from(Span::styled(arb, app.theme.dim())));
    }

    if let Some(cost) = app.cost_usd {
        if cost > 0.0 {
            lines.push(Line::from(Span::styled(
                format!("  Total cost: ${cost:.4}"),
                app.theme.muted(),
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_descriptions_carry_no_manual_layout() {
        for e in help_registry() {
            assert!(!e.desc.contains('\n'), "{}: line break in {:?}", e.key, e.desc);
            assert!(!e.desc.contains("  "), "{}: double space in {:?}", e.key, e.desc);
            assert_eq!(e.desc.trim(), e.desc, "{}: padded description", e.key);
        }
    }

    #[test]
    fn words_wrap_by_terminal_cells() {
        assert_eq!(wrap_words("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap_words("a → b c", 5), ["a → b", "c"]);
        // Wide characters take two cells each.
        assert_eq!(wrap_words("日本 語", 4), ["日本", "語"]);
        assert_eq!(wrap_words("abcdefgh ij", 3), ["abc", "def", "gh", "ij"]);
        assert_eq!(wrap_words("", 5), [""]);
        assert_eq!(wrap_words("x", 0), ["x"]);
    }

    fn row_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Inner width of the overlay on a terminal `cols` wide.
    fn inner_width(cols: u16) -> usize {
        (OVERLAY_MAX_WIDTH.min(cols.saturating_sub(2 * OVERLAY_H_MARGIN)) as usize).saturating_sub(2)
    }

    #[test]
    fn continuations_start_in_the_description_column() {
        use filar_core::CommandConfirmMode;
        let app = App::new("t".into(), CommandConfirmMode::Always);
        for cols in [60u16, 80, 120] {
            let width = inner_width(cols);
            let lines = overlay_lines(&app, width);
            let mut continuations = 0;
            for line in &lines {
                let text = row_text(line);
                assert!(text.width() <= width, "{cols}: {text:?} is wider than {width}");
                if line.spans.len() == 2 && line.spans[0].content.trim().is_empty() {
                    continuations += 1;
                    assert_eq!(line.spans[0].content.width(), DESC_COL, "{cols}: {text:?}");
                    assert!(!line.spans[1].content.starts_with(' '), "{cols}: {text:?}");
                }
            }
            assert!(continuations > 0, "{cols}: long descriptions must wrap");
        }
    }

    #[test]
    fn unavailable_entries_are_muted_on_every_row() {
        use filar_core::CommandConfirmMode;
        let mut app = App::new("t".into(), CommandConfirmMode::Always);
        app.mode = AppMode::Interactive;
        let lines = overlay_lines(&app, inner_width(80));
        // ^J is unavailable in terminal mode and long enough to wrap.
        let at = lines
            .iter()
            .position(|l| l.spans.first().is_some_and(|s| s.content.trim() == "^J"))
            .expect("^J entry");
        assert_eq!(lines[at].spans[1].style, app.theme.muted());
        assert!(lines[at + 1].spans[0].content.trim().is_empty(), "^J wraps");
        assert_eq!(lines[at + 1].spans[1].style, app.theme.muted());
    }

    #[test]
    fn a_narrow_overlay_stacks_the_description_under_the_key() {
        use filar_core::CommandConfirmMode;
        let app = App::new("t".into(), CommandConfirmMode::Always);
        // No width panics; from a section header's width on, nothing is
        // wider than the overlay (a clipped header is all a 3-cell overlay
        // can offer).
        for width in [0usize, 1, 3, 10, 16, 30, DESC_COL + MIN_DESC_WIDTH - 1] {
            let lines = overlay_lines(&app, width);
            assert!(!lines.is_empty());
            if width >= 16 {
                for line in &lines {
                    let text = row_text(line);
                    assert!(text.width() <= width, "{width}: {text:?}");
                }
            }
        }
        let lines = overlay_lines(&app, 30);
        let at = lines.iter().position(|l| row_text(l) == "  F2").expect("F2 on its own row");
        assert!(row_text(&lines[at + 1]).starts_with("    Toggle safe mode"));
    }

    fn rendered(cols: u16, rows: u16, scroll: u16) -> Vec<String> {
        use filar_core::CommandConfirmMode;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::new("t".into(), CommandConfirmMode::Always);
        app.help_scroll = scroll;
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        terminal.draw(|f| render_help_overlay(f, &app, f.area())).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..rows)
            .map(|y| (0..cols).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect()
    }

    #[test]
    fn the_title_counts_rows_and_the_scroll_reaches_the_last_one() {
        use filar_core::CommandConfirmMode;
        let app = App::new("t".into(), CommandConfirmMode::Always);
        for cols in [20u16, 60, 80, 120] {
            let total = overlay_lines(&app, inner_width(cols)).len();
            let screen = rendered(cols, 24, 0);
            let title = screen.iter().find(|r| r.contains("Help (")).expect("title row");
            assert!(title.contains(&format!("1/{total},")) || cols < 60, "{cols}: {title}");

            // Scrolled to the end, the last row of the overlay body is the
            // last line of the content (the usage header at least).
            let last = row_text(overlay_lines(&app, inner_width(cols)).last().unwrap());
            let screen = rendered(cols, 24, u16::MAX);
            assert!(
                screen.iter().any(|r| r.contains(last.trim())),
                "{cols}: {last:?} not on screen:\n{}",
                screen.join("\n")
            );
        }
    }

    #[test]
    fn rendered_continuations_sit_under_the_description() {
        let screen = rendered(80, 40, 0);
        let first = screen.iter().position(|r| r.contains("Toggle safe mode")).expect("F2 row");
        let col = screen[first].find("Toggle").unwrap();
        let next = &screen[first + 1];
        let next_col = next.find(|c: char| c.is_alphanumeric()).expect("continuation text");
        assert_eq!(next_col, col, "{}\n{next}", screen[first]);
    }

    #[test]
    fn registry_is_nonempty() {
        let r = help_registry();
        assert!(!r.is_empty(), "help registry must not be empty");
    }

    #[test]
    fn f1_desc_avoids_command_glyph_off_macos() {
        let f1 = help_registry()
            .into_iter()
            .find(|e| e.key == "F1")
            .expect("F1 entry");
        #[cfg(target_os = "macos")]
        {
            assert!(
                f1.desc.contains("Fn+F1"),
                "macOS F1 help should mention Fn+F1: {}",
                f1.desc
            );
            assert!(
                f1.desc.contains('⌘'),
                "macOS F1 help should mention ⌘: {}",
                f1.desc
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(
                !f1.desc.contains('⌘'),
                "non-macOS overlay must not use ⌘ (Windows console renders it as ?): {}",
                f1.desc
            );
            assert!(
                f1.desc.contains("Ctrl"),
                "non-macOS F1 help should still mention Ctrl: {}",
                f1.desc
            );
        }
    }

    #[test]
    fn registry_has_key_sections() {
        let r = help_registry();
        let sections: std::collections::HashSet<&str> =
            r.iter().map(|e| e.section).collect();
        assert!(sections.contains("Modes"), "must have Modes section");
        assert!(sections.contains("Tabs"), "must have Tabs section");
        assert!(sections.contains("Agent"), "must have Agent section");
        assert!(sections.contains("Scrolling"), "must have Scrolling section");
        assert!(sections.contains("Input"), "must have Input section");
        assert!(sections.contains("Exit"), "must have Exit section");
    }

    #[test]
    fn most_entries_available_in_normal_mode() {
        let r = help_registry();
        let available: Vec<&str> = r
            .iter()
            .filter(|e| (e.available)(AppMode::Normal))
            .map(|e| e.key)
            .collect();
        // Most entries should be available; a few mode-specific ones
        // (^Z, Tab approve/deny) are correctly restricted.
        assert!(available.contains(&"F1"));
        assert!(available.contains(&"^T"));
        assert!(available.contains(&"Enter"));
        assert!(available.contains(&"!cmd"));
        assert!(available.contains(&"^N"));
        assert!(available.contains(&"^Q"));
        assert!(available.contains(&"^S"));
        assert!(available.contains(&"wheel"));
        assert!(available.contains(&"drag"));
        assert!(available.contains(&"PgUp"));
        assert!(available.contains(&"Up / Down"));
        // These are NOT in Normal:
        assert!(!available.contains(&"^Z")); // only Thinking/Confirming
        assert!(!available.contains(&"Tab"), "Tab switch is only for Confirming");
    }

    #[test]
    fn interactive_mode_restricts_most_entries() {
        let r = help_registry();
        let always_available: Vec<&str> = r
            .iter()
            .filter(|e| (e.available)(AppMode::Interactive))
            .map(|e| e.key)
            .collect();
        // Help, modes, scrolling, and exit should work in Interactive.
        assert!(always_available.contains(&"F1"));
        assert!(always_available.contains(&"^T")); // toggles out of interactive
        assert!(always_available.contains(&"wheel"));
        assert!(always_available.contains(&"^Q"));
        assert!(always_available.contains(&"drag"));
        // Tabs/agent/input entries should be dimmed.
        assert!(!always_available.contains(&"^N"));
        assert!(!always_available.contains(&"Enter"));
        assert!(!always_available.contains(&"!cmd"));
    }
}
