//! UI rendering for the TUI.
//!
//! The layout is:
//! ```text
//! ┌─────────────────────────────────────┐
//! │ Status bar: target | mode           │
//! ├─────────────────────────────────────┤
//! │                                     │
//! │ Chat history (scrollable)           │
//! │                                     │
//! ├─────────────────────────────────────┤
//! │ Input field / Confirmation dialog   │
//! ├─────────────────────────────────────┤
//! │ Help bar                            │
//! └─────────────────────────────────────┘
//! ```

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, AppMode};
use filar_core::ChatBlock;

/// Strip emoji and other non-renderable Unicode characters from a string.
/// Windows terminal (conhost) can't display most emojis, so they show as '?'.
/// Conservative whitelist: ASCII, Cyrillic, Latin, punctuation, arrows, math, box drawing.
fn strip_emoji(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            let cp = c as u32;
            cp <= 0x024F                        // ASCII + Latin + Latin Extended
            || (0x0300..=0x036F).contains(&cp)  // Combining diacritics
            || (0x0400..=0x04FF).contains(&cp)  // Cyrillic
            || (0x2000..=0x206F).contains(&cp)  // General punctuation (— " " ' ')
            || (0x2070..=0x209F).contains(&cp)  // Super/subscripts
            || (0x20A0..=0x20CF).contains(&cp)  // Currency symbols
            || (0x2190..=0x21FF).contains(&cp)  // Arrows (→ ← ↑ ↓)
            || (0x2200..=0x22FF).contains(&cp)  // Math operators (≠ ≤ ≥ ±)
            || (0x2500..=0x257F).contains(&cp)  // Box drawing (┃ │ ┌ └)
            || (0x2580..=0x259F).contains(&cp)  // Block elements
            || (0x25A0..=0x25FF).contains(&cp)  // Geometric shapes (▶ ◆ ● ■)
            // Everything else (Misc symbols, Dingbats, Emojis, Flags) is stripped
        })
        .collect()
}

/// Wrap a single line of text to fit within `width` characters.
/// Returns one or more strings, each at most `width` chars wide.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut result = Vec::new();
    for line in text.lines() {
        let char_count = line.chars().count();
        if char_count <= width {
            result.push(line.to_string());
        } else {
            let mut current = String::new();
            let mut count = 0;
            for c in line.chars() {
                if count >= width {
                    result.push(std::mem::take(&mut current));
                    count = 0;
                }
                current.push(c);
                count += 1;
            }
            if !current.is_empty() {
                result.push(current);
            }
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Render the entire UI.
pub fn render(f: &mut Frame, app: &App) {
    if app.mode == AppMode::Interactive {
        render_interactive(f, app);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // status bar
            Constraint::Min(10),    // chat history
            Constraint::Length(5),  // input / confirm (3 text lines + 2 borders)
            Constraint::Length(1),  // help bar
        ])
        .split(f.area());

    render_status_bar(f, app, chunks[0]);
    render_chat_history(f, app, chunks[1]);
    render_input_area(f, app, chunks[2]);
    render_help_bar(f, app, chunks[3]);
}

/// Render the interactive terminal mode.
fn render_interactive(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // status bar
            Constraint::Min(1),     // terminal grid
            Constraint::Length(1),  // help bar
        ])
        .split(f.area());

    render_status_bar(f, app, chunks[0]);

    // Render the terminal model grid.
    if let Some(ref term) = app.terminal {
        term.render(f, chunks[1]);
    } else {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Terminal")
            .border_style(Style::default().fg(Color::Yellow));
        let paragraph = Paragraph::new("No terminal active").block(block);
        f.render_widget(paragraph, chunks[1]);
    }

    render_help_bar(f, app, chunks[2]);
}

/// Render the status bar (top line).
fn render_status_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let mode_text = match app.mode {
        AppMode::Normal => "NORMAL",
        AppMode::Thinking => "THINKING...",
        AppMode::Confirming => "CONFIRM",
        AppMode::Interactive => "INTERACTIVE",
        AppMode::PasswordInput => "PASSWORD",
    };

    let mode_color = match app.mode {
        AppMode::Normal => Color::Green,
        AppMode::Thinking => Color::Yellow,
        AppMode::Confirming => Color::Red,
        AppMode::Interactive => Color::Magenta,
        AppMode::PasswordInput => Color::Magenta,
    };

    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", app.target_name),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw(" │ "),
        Span::styled(
            format!(" {:?} ", app.confirm_mode),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(" │ "),
        Span::styled(
            format!(" {mode_text} "),
            Style::default().fg(Color::Black).bg(mode_color),
        ),
    ]);

    let paragraph = Paragraph::new(line).style(
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White),
    );
    f.render_widget(paragraph, area);
}

/// Render the chat history (scrollable).
fn render_chat_history(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Chat")
        .border_style(Style::default().fg(Color::DarkGray));

    // Compute wrapping width: inner area minus border (2) minus prefix (2).
    let inner_width = (area.width.saturating_sub(2)) as usize;
    let content_width = inner_width.saturating_sub(2).max(1);  // "  " prefix
    let output_width = inner_width.saturating_sub(4).max(1);   // "  | " prefix

    // Build lines from chat blocks, pre-wrapping long lines.
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.messages {
        match msg {
            ChatBlock::User(text) => {
                let text = strip_emoji(text);
                lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::styled("You", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                ]));
                for line in text.lines() {
                    for wrapped in wrap_text(line, content_width) {
                        lines.push(Line::from(format!("  {wrapped}")));
                    }
                }
                lines.push(Line::from(""));
            }
            ChatBlock::Agent(text) => {
                let text = strip_emoji(text);
                lines.push(Line::from(vec![
                    Span::styled("* ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                    Span::styled("Agent", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                ]));
                for line in text.lines() {
                    for wrapped in wrap_text(line, content_width) {
                        lines.push(Line::from(format!("  {wrapped}")));
                    }
                }
                lines.push(Line::from(""));
            }
            ChatBlock::Command {
                command,
                explanation,
                output,
                approved,
            } => {
                let status = if *approved {
                    Span::styled(" [ok] ", Style::default().fg(Color::Green))
                } else {
                    Span::styled(" [no] ", Style::default().fg(Color::Red))
                };

                lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Yellow)),
                    Span::styled("Command", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                    status,
                ]));

                if !explanation.is_empty() {
                    for wrapped in wrap_text(&strip_emoji(explanation), output_width) {
                        lines.push(Line::from(format!("  | {wrapped}")));
                    }
                }
                for wrapped in wrap_text(&format!("$ {command}"), output_width) {
                    lines.push(Line::from(format!("  | {wrapped}")));
                }

                if let Some(out) = output {
                    let mut count = 0;
                    for line in out.lines() {
                        if count >= 30 {
                            let remaining = out.lines().count().saturating_sub(30);
                            lines.push(Line::from(format!("  | ... ({} more lines)", remaining)));
                            break;
                        }
                        for wrapped in wrap_text(line, output_width) {
                            lines.push(Line::from(format!("  | {wrapped}")));
                        }
                        count += 1;
                    }
                }
                lines.push(Line::from(""));
            }
            ChatBlock::Error(text) => {
                let text = strip_emoji(text);
                lines.push(Line::from(vec![
                    Span::styled("! ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::styled("Error", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                ]));
                for line in text.lines() {
                    for wrapped in wrap_text(line, content_width) {
                        lines.push(Line::from(format!("  {wrapped}")));
                    }
                }
                lines.push(Line::from(""));
            }
            ChatBlock::System(text) => {
                lines.push(Line::from(vec![
                    Span::styled("- ", Style::default().fg(Color::DarkGray)),
                    Span::styled(text, Style::default().fg(Color::DarkGray)),
                ]));
                lines.push(Line::from(""));
            }
        }
    }

    // Limit to last 500 lines to prevent rendering lag on long conversations.
    const MAX_LINES: usize = 500;
    if lines.len() > MAX_LINES {
        lines = lines.split_off(lines.len() - MAX_LINES);
    }

    // Apply scroll: skip lines from the top if scrolled.
    let total_lines = lines.len();
    let visible_height = area.height.saturating_sub(2) as usize; // -2 for border
    let skip = if total_lines > visible_height {
        total_lines.saturating_sub(visible_height + app.scroll)
    } else {
        0
    };
    let skip = skip.min(total_lines);
    let visible_lines: Vec<Line> = lines.into_iter().skip(skip).collect();

    let paragraph = Paragraph::new(visible_lines).block(block);
    f.render_widget(paragraph, area);
}

/// Render the input area or confirmation dialog.
fn render_input_area(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    match app.mode {
        AppMode::Normal => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Input")
                .border_style(Style::default().fg(Color::Cyan));
            let input = if app.input.is_empty() {
                "Type your message and press Enter…"
            } else {
                &app.input
            };
            let style = if app.input.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            };
            let paragraph = Paragraph::new(input)
                .block(block)
                .style(style)
                .wrap(Wrap { trim: false });
            f.render_widget(paragraph, area);

            // Place cursor at the correct position (handles multi-line wrap).
            let cursor_pos = app.cursor_pos as u16;
            let inner_width = area.width.saturating_sub(2).max(1); // -2 for borders
            let cursor_col = cursor_pos % inner_width;
            let cursor_row = cursor_pos / inner_width;
            let cursor_x = area.x + 1 + cursor_col;
            let cursor_y = area.y + 1 + cursor_row.min(area.height.saturating_sub(2));
            f.set_cursor_position((cursor_x, cursor_y));
        }
        AppMode::Thinking => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Agent")
                .border_style(Style::default().fg(Color::Yellow));
            let text = "Agent is thinking... (Ctrl+C to quit)";
            let paragraph = Paragraph::new(text)
                .block(block)
                .style(Style::default().fg(Color::Yellow));
            f.render_widget(paragraph, area);
        }
        AppMode::Confirming => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Confirm Command")
                .border_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));

            let mut lines = Vec::new();

            if let Some(confirm) = &app.pending_confirm {
                if !confirm.explanation.is_empty() {
                    lines.push(Line::from(format!("  Explanation: {}", confirm.explanation)));
                }
                if confirm.destructive {
                    lines.push(Line::from(vec![
                        Span::styled("  ! WARNING: ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                        Span::styled("This command may be destructive!", Style::default().fg(Color::Red)),
                    ]));
                }
                lines.push(Line::from(format!("  $ {}", confirm.command)));
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled(" [a]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                    Span::raw("pprove  "),
                    Span::styled("[d]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::raw("eny  "),
                    Span::styled("Ctrl+C", Style::default().fg(Color::DarkGray)),
                    Span::raw(" to quit"),
                ]));
            }

            let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
            f.render_widget(paragraph, area);
        }
        AppMode::Interactive => {
            // Not used — interactive mode renders the terminal grid directly.
        }
        AppMode::PasswordInput => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Password Input (masked)")
                .border_style(Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD));
            // Show asterisks instead of actual characters.
            let masked: String = "*".repeat(app.input.chars().count());
            let display = if masked.is_empty() {
                "Type password, press Enter to send (hidden), Esc to cancel"
            } else {
                &masked
            };
            let style = if app.input.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Yellow)
            };
            let paragraph = Paragraph::new(display)
                .block(block)
                .style(style)
                .wrap(Wrap { trim: false });
            f.render_widget(paragraph, area);

            // Place cursor at end of masked text.
            let cursor_pos = app.cursor_pos as u16;
            let inner_width = area.width.saturating_sub(2).max(1);
            let cursor_col = cursor_pos % inner_width;
            let cursor_row = cursor_pos / inner_width;
            let cursor_x = area.x + 1 + cursor_col;
            let cursor_y = area.y + 1 + cursor_row.min(area.height.saturating_sub(2));
            f.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

/// Render the help bar (bottom line).
fn render_help_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let help_text = match app.mode {
        AppMode::Normal => " Enter=Send | !=Shell | Ctrl+T=Terminal | Ctrl+P=Password | Ctrl+C=Quit",
        AppMode::Thinking => " Ctrl+C=Quit | PgUp/PgDn=Scroll",
        AppMode::Confirming => " a/y/e=Approve | d/n=Deny | Ctrl+C=Quit",
        AppMode::Interactive => " Ctrl+T=Agent mode | (terminal input is forwarded)",
        AppMode::PasswordInput => " Enter=Send password | Esc=Cancel | Ctrl+C=Cancel",
    };

    let paragraph = Paragraph::new(help_text).style(
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::Gray),
    );
    f.render_widget(paragraph, area);
}
