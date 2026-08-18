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
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

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
            desc: "Toggle safe mode: agent must justify each command and wait\n             for confirmation; session is auto-saved to Markdown",
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
        // ── Status bar ───────────────────────────────────────────────
        HelpEntry {
            key: "mode",
            desc: "Status bar shows the confirm mode (right side).\n             Highlighted in accent color when safe mode is active",
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
            desc: "Close active tab",
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
            desc: "Select text (copies on release)",
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
            desc: "Open host selection overlay (local + [[ssh_targets]])",
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
    let registry = help_registry();

    let width = OVERLAY_MAX_WIDTH.min(area.width.saturating_sub(2 * OVERLAY_H_MARGIN));
    let height = area.height.saturating_sub(2 * OVERLAY_V_MARGIN);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + OVERLAY_V_MARGIN;

    let overlay_area = Rect::new(x, y, width, height);

    // Clear the area behind the overlay.
    f.render_widget(Clear, overlay_area);

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

        let line = Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:>14}", entry.key), key_style),
            Span::raw("  "),
            Span::styled(entry.desc, desc_style),
        ]);
        lines.push(line);
    }

    let inner = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg));
    let inner_area = inner.inner(overlay_area);
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

    let block = inner.title(title);

    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });

    f.render_widget(paragraph, overlay_area);
}

#[cfg(test)]
mod tests {
    use super::*;

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
