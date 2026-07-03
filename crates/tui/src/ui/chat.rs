//! Chat history rendering.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use filar_core::ChatBlock;

use super::text::{strip_emoji, wrap_text};

/// Render the chat history (scrollable).
pub(crate) fn render_chat_history(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Chat")
        .border_style(app.theme.muted());

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
                    Span::styled("> ", app.theme.user_style()),
                    Span::styled("You", app.theme.user_style()),
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
                    Span::styled("* ", app.theme.agent_style()),
                    Span::styled("Agent", app.theme.agent_style()),
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
                    Span::styled(" [ok] ", app.theme.success_fg())
                } else {
                    Span::styled(" [no] ", app.theme.danger_fg())
                };

                lines.push(Line::from(vec![
                    Span::styled("> ", app.theme.warning_fg()),
                    Span::styled("Command", app.theme.command_style()),
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
                    for (count, line) in out.lines().enumerate() {
                        if count >= 30 {
                            let remaining = out.lines().count().saturating_sub(30);
                            lines.push(Line::from(format!("  | ... ({} more lines)", remaining)));
                            break;
                        }
                        for wrapped in wrap_text(line, output_width) {
                            lines.push(Line::from(format!("  | {wrapped}")));
                        }
                    }
                }
                lines.push(Line::from(""));
            }
            ChatBlock::Error(text) => {
                let text = strip_emoji(text);
                lines.push(Line::from(vec![
                    Span::styled("! ", app.theme.error_style()),
                    Span::styled("Error", app.theme.error_style()),
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
                    Span::styled("- ", app.theme.muted()),
                    Span::styled(text, app.theme.muted()),
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
