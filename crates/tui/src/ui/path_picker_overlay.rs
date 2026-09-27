//! Path-picker overlay — in-TUI file/folder browser on the active target (#351).

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState};
use ratatui::Frame;

use crate::app::App;
use crate::path_picker::PathPickerKind;

const OVERLAY_MAX_WIDTH: u16 = 72;
const OVERLAY_H_MARGIN: u16 = 4;
const OVERLAY_V_MARGIN: u16 = 2;

/// Render the path-picker overlay on top of the current frame.
pub(crate) fn render_path_picker(f: &mut Frame, app: &App, area: Rect) {
    let width = OVERLAY_MAX_WIDTH.min(area.width.saturating_sub(2 * OVERLAY_H_MARGIN));
    let list_size = app.path_picker_entries.len().max(1) as u16;
    let content_height = list_size.saturating_add(5);
    let height = content_height.min(area.height.saturating_sub(2 * OVERLAY_V_MARGIN));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    let kind_label = match app.path_picker_kind {
        PathPickerKind::File => "file",
        PathPickerKind::Folder => "folder",
    };
    let title = format!(" Pick {kind_label} — {} ", app.path_picker_dir);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(
            truncate(&title, width as usize),
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ));

    let mut items: Vec<ListItem> = Vec::new();

    if app.path_picker_loading {
        items.push(ListItem::new(Line::from(vec![Span::styled(
            "  Loading…",
            app.theme.muted(),
        )])));
    } else if let Some(ref err) = app.path_picker_error {
        items.push(ListItem::new(Line::from(vec![Span::styled(
            format!("  Error: {err}"),
            app.theme.warning_fg(),
        )])));
    } else if app.path_picker_entries.is_empty() {
        items.push(ListItem::new(Line::from(vec![Span::styled(
            "  (empty directory)",
            app.theme.muted(),
        )])));
    } else {
        for (i, entry) in app.path_picker_entries.iter().enumerate() {
            let cursor = if app.path_picker_index == i {
                crate::path_picker::SELECTION_CURSOR
            } else {
                " "
            };
            let suffix = if entry.is_dir { "/" } else { "" };
            items.push(ListItem::new(Line::from(vec![
                Span::raw(format!(" {cursor} ")),
                Span::styled(
                    format!("{}{suffix}", entry.name),
                    if entry.is_dir {
                        Style::default().fg(app.theme.accent)
                    } else {
                        Style::default().fg(app.theme.fg)
                    },
                ),
            ])));
        }
    }

    if app.path_picker_truncated {
        items.push(ListItem::new(Line::from(vec![Span::styled(
            "  … listing truncated",
            app.theme.warning_fg(),
        )])));
    }

    let mut state = ListState::default();
    if !app.path_picker_entries.is_empty() {
        state.select(Some(app.path_picker_index));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(app.theme.selection_bg));

    f.render_stateful_widget(list, overlay_area, &mut state);

    let footer = footer_hint(app.path_picker_kind, overlay_area.width);
    let footer_area = Rect::new(
        overlay_area.x,
        overlay_area.y + overlay_area.height.saturating_sub(1),
        overlay_area.width,
        1,
    );
    f.render_widget(
        ratatui::widgets::Paragraph::new(Span::styled(footer, app.theme.muted())),
        footer_area,
    );
}

/// Footer hint (#483), the longest variant that fits the overlay's width.
///
/// Both pickers navigate the same way: Enter opens a folder, `a` picks one
/// (on `..` — the folder shown, "here"). Enter on a file picks it in the file picker.
fn footer_hint(kind: PathPickerKind, width: u16) -> &'static str {
    let variants: &[&'static str] = match kind {
        PathPickerKind::File => &[
            " \u{2191}\u{2193} move  Enter open/pick file  a pick folder (.. = here)  Esc ",
            " Enter open/pick file  a pick folder (.. = here)  Esc ",
            " Enter open/file  a folder  Esc ",
            " Enter file  a folder  Esc ",
        ],
        PathPickerKind::Folder => &[
            " \u{2191}\u{2193} move  Enter open  a pick folder (.. = here)  Esc cancel ",
            " Enter open  a pick folder (.. = here)  Esc ",
            " Enter open  a pick  Esc ",
        ],
    };
    variants
        .iter()
        .copied()
        .find(|v| v.chars().count() <= width as usize)
        .unwrap_or(variants[variants.len() - 1])
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let keep = max.saturating_sub(1).max(1);
        s.chars().take(keep).collect::<String>() + "\u{2026}"
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_footer_names_the_folder_key_and_fits_the_overlay() {
        for kind in [PathPickerKind::File, PathPickerKind::Folder] {
            let full = footer_hint(kind, OVERLAY_MAX_WIDTH);
            assert!(full.contains("a pick folder"), "{full}");
            assert!(full.contains("Enter open"), "{full}");
            assert!(full.chars().count() <= OVERLAY_MAX_WIDTH as usize, "{full}");
            for width in [30u16, 40, 50, 60] {
                let hint = footer_hint(kind, width);
                assert!(hint.chars().count() <= width as usize, "{width}: {hint}");
                assert!(hint.contains("a "), "{width}: {hint}");
            }
        }
        assert!(footer_hint(PathPickerKind::File, OVERLAY_MAX_WIDTH).contains("pick file"));
    }
}
