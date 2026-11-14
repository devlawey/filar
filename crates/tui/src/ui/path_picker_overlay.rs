//! Path-picker overlay — in-TUI file/folder browser on the active target (#351).

use std::borrow::Cow;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState};
use ratatui::Frame;

use crate::app::runbook_replay::PickerSource;
use crate::app::App;
use crate::path_picker::PathPickerKind;

/// Top-right hint in the files source (#668).
const TAB_TO_RUNBOOKS: &str = " Tab: runbooks ";
/// Top-right hint in the runbooks source (#668).
const TAB_TO_FILES: &str = " Tab: files ";

const OVERLAY_MAX_WIDTH: u16 = 72;
const OVERLAY_H_MARGIN: u16 = 4;
const OVERLAY_V_MARGIN: u16 = 2;

/// Render the path-picker overlay on top of the current frame.
pub(crate) fn render_path_picker(f: &mut Frame, app: &App, area: Rect) {
    if app.path_picker_source == PickerSource::Runbooks {
        render_runbook_list(f, app, area);
        return;
    }
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
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(
            truncate(&title, width as usize),
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    // The other source, named on the frame's top right (#668): the footer
    // has no room left for it.
    if app.runbooks_available() {
        block = block.title_top(Line::from(Span::styled(TAB_TO_RUNBOOKS, app.theme.muted())).right_aligned());
    }

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

/// Footer hint (#483): the longest variant that fits the overlay's width,
/// whatever the order of the list.
///
/// Both pickers navigate the same way: Enter opens a folder, `a` picks one
/// (on `..` — the folder shown, "here"). Enter on a file picks it in the file picker.
/// A second `/` closes the picker and types a literal `/` (#670).
fn footer_hint(kind: PathPickerKind, width: u16) -> Cow<'static, str> {
    let variants: &[&'static str] = match kind {
        PathPickerKind::File => &[
            " \u{2191}\u{2193} Enter open/pick file  a pick folder (.. = here)  // literal /  Esc ",
            " Enter open/pick file  a pick folder (.. = here)  // literal /  Esc ",
            " Enter open/pick file  a pick folder  // literal /  Esc ",
            " Enter open/file  a folder  // literal /  Esc ",
            " Enter file  a folder  // /  Esc ",
            " Enter file  a folder  Esc ",
        ],
        PathPickerKind::Folder => &[
            " \u{2191}\u{2193} move  Enter open  a pick folder (.. = here)  // literal /  Esc ",
            " Enter open  a pick folder (.. = here)  // literal /  Esc ",
            " Enter open  a pick  // /  Esc ",
            " Enter open  a pick  Esc ",
        ],
    };
    let fits = |v: &&&str| v.chars().count() <= width as usize;
    match variants.iter().filter(fits).max_by_key(|v| v.chars().count()) {
        Some(v) => Cow::Borrowed(v),
        // Narrower than every variant: the shortest, cut to the width.
        None => {
            let shortest = variants.iter().min_by_key(|v| v.chars().count()).copied().unwrap_or("");
            Cow::Owned(shortest.chars().take(width as usize).collect())
        }
    }
}

/// The picker's runbooks source (#668): the export folder's runbooks,
/// newest first, each with its date and symptom line.
fn render_runbook_list(f: &mut Frame, app: &App, area: Rect) {
    let state = &app.runbook_picker;
    let width = OVERLAY_MAX_WIDTH.min(area.width.saturating_sub(2 * OVERLAY_H_MARGIN));
    let rows = (state.entries.len() * 2).max(1) as u16;
    let notes = u16::from(state.error.is_some()) + u16::from(state.extracting.is_some());
    let height = rows
        .saturating_add(notes)
        .saturating_add(4)
        .min(area.height.saturating_sub(2 * OVERLAY_V_MARGIN));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay_area = Rect::new(x, y, width, height);
    f.render_widget(Clear, overlay_area);

    let title = format!(" Runbooks — {} ", state.dir);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(
            truncate(&title, (width as usize).saturating_sub(TAB_TO_FILES.len() + 2)),
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .title_top(Line::from(Span::styled(TAB_TO_FILES, app.theme.muted())).right_aligned());

    // Inside the frame, after the ` > ` cursor column.
    let inner = width.saturating_sub(2 + 3) as usize;
    let mut items: Vec<ListItem> = Vec::new();
    if let Some(name) = &state.extracting {
        items.push(ListItem::new(Line::from(Span::styled(
            truncate(&format!("  Extracting steps from {name}… (Esc cancels)"), inner + 2),
            app.theme.warning_fg(),
        ))));
    }
    if let Some(err) = &state.error {
        items.push(ListItem::new(Line::from(Span::styled(
            truncate(&format!("  Error: {err}"), inner + 2),
            app.theme.warning_fg(),
        ))));
    }
    let first_entry = items.len();
    if state.entries.is_empty() && state.error.is_none() {
        items.push(ListItem::new(Line::from(Span::styled(
            truncate(
                &format!("  (no runbooks in {}) — Ctrl+S writes them when save_runbook is on", state.dir),
                inner + 2,
            ),
            app.theme.muted(),
        ))));
    }
    for (i, entry) in state.entries.iter().enumerate() {
        let cursor = if state.index == i { crate::path_picker::SELECTION_CURSOR } else { " " };
        let date = crate::runbook_picker::format_modified(entry.modified);
        let name_room = inner.saturating_sub(date.chars().count() + 2);
        let name = truncate(&entry.display, name_room.max(1));
        let pad = inner.saturating_sub(name.chars().count() + date.chars().count());
        items.push(ListItem::new(vec![
            Line::from(vec![
                Span::raw(format!(" {cursor} ")),
                Span::styled(name, Style::default().fg(app.theme.fg)),
                Span::raw(" ".repeat(pad)),
                Span::styled(date, app.theme.muted()),
            ]),
            Line::from(Span::styled(
                format!("     {}", truncate(&entry.preview, inner.saturating_sub(3).max(1))),
                app.theme.muted(),
            )),
        ]));
    }
    if state.truncated {
        items.push(ListItem::new(Line::from(Span::styled(
            "  … listing truncated",
            app.theme.warning_fg(),
        ))));
    }

    let mut list_state = ListState::default();
    if !state.entries.is_empty() {
        list_state.select(Some(first_entry + state.index));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(app.theme.selection_bg));
    f.render_stateful_widget(list, overlay_area, &mut list_state);

    let footer = runbook_footer_hint(overlay_area.width);
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

/// Footer of the runbooks source: the longest variant that fits.
fn runbook_footer_hint(width: u16) -> Cow<'static, str> {
    const VARIANTS: &[&str] = &[
        " \u{2191}\u{2193} move  Enter show the plan  // literal /  Esc close ",
        " \u{2191}\u{2193} Enter plan  // literal /  Esc ",
        " Enter plan  // /  Esc ",
    ];
    let fits = |v: &&&str| v.chars().count() < width as usize;
    match VARIANTS.iter().filter(fits).max_by_key(|v| v.chars().count()) {
        Some(v) => Cow::Borrowed(v),
        None => Cow::Owned(VARIANTS[VARIANTS.len() - 1].chars().take(width as usize).collect()),
    }
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen(app: &App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render_path_picker(f, app, f.area())).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect()
    }

    #[test]
    fn the_files_source_names_the_runbooks_tab_and_the_runbooks_source_lists_them() {
        let mut app = App::new("t".into(), filar_core::CommandConfirmMode::Always);
        app.path_picker_visible = true;
        app.path_picker_dir = "/tmp".into();
        let rows = screen(&app, 100, 20).join("\n");
        assert!(rows.contains("Tab: runbooks"), "{rows}");

        app.path_picker_source = PickerSource::Runbooks;
        app.runbook_picker.dir = "/exports".into();
        app.runbook_picker.entries = vec![crate::runbook_picker::RunbookEntry {
            display: "web-1/a.runbook.md".into(),
            path: "/exports/web-1/a.runbook.md".into(),
            modified: None,
            preview: "Disk full on root".into(),
        }];
        app.runbook_picker.extracting = Some("web-1/a.runbook.md".into());
        app.runbook_picker.entries[0].modified = Some(std::time::SystemTime::now());
        let date = crate::runbook_picker::format_modified(app.runbook_picker.entries[0].modified);
        // The date stays whole at the end of the row.
        let rows = screen(&app, 100, 20).join("\n");
        for want in ["Runbooks — /exports", "Tab: files", "web-1/a.runbook.md", "Disk full on root", "Extracting steps", "Enter show the plan", date.as_str()] {
            assert!(rows.contains(want), "{want} missing:\n{rows}");
        }

        app.runbook_picker.entries.clear();
        app.runbook_picker.extracting = None;
        let rows = screen(&app, 100, 20).join("\n");
        assert!(rows.contains("no runbooks in /exports"), "{rows}");
    }

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
        // Both pickers name the arrow keys at full width, and the hint leaves
        // the right corner of the frame alone (review of #670).
        for kind in [PathPickerKind::File, PathPickerKind::Folder] {
            let full = footer_hint(kind, OVERLAY_MAX_WIDTH);
            assert!(full.contains('\u{2191}'), "{full}");
            assert!(full.chars().count() < OVERLAY_MAX_WIDTH as usize, "{full}");
        }
        // The literal-slash key is named whenever there is room for it (#670).
        for kind in [PathPickerKind::File, PathPickerKind::Folder] {
            assert!(footer_hint(kind, OVERLAY_MAX_WIDTH).contains("// literal /"));
            for width in [40u16, 50, 60] {
                let hint = footer_hint(kind, width);
                assert!(hint.contains("// "), "{width}: {hint}");
            }
        }
        // Narrower than any variant: cut to fit, never wider than the overlay.
        for width in [0u16, 5, 12] {
            let hint = footer_hint(PathPickerKind::File, width);
            assert!(hint.chars().count() <= width as usize, "{width}: {hint}");
        }
    }
}
