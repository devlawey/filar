//! Text utility helpers — emoji stripping, line wrapping, and markdown-lite rendering.

use ratatui::text::Span;

use super::theme::Theme;

/// Strip emoji and other non-renderable Unicode characters from a string.
/// Windows terminal (conhost) can't display most emojis, so they show as '?'.
/// Conservative whitelist: ASCII, Cyrillic, Latin, punctuation, arrows, math, box drawing.
pub(crate) fn strip_emoji(s: &str) -> String {
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
            || (0x25A0..=0x25FF).contains(&cp)  // Geometric shapes (▶ ◆ ● ■ ▸ ▾)
            || (0x2713..=0x2717).contains(&cp)  // Dingbats: ✓ ✗ (command status)
            || (0x2800..=0x28FF).contains(&cp)  // Braille patterns (spinner: ⠋⠙⠹…)
            // Everything else (Misc symbols, Dingbats, Emojis, Flags) is stripped
        })
        .collect()
}

/// Wrap a single line of text to fit within `width` characters.
/// Returns one or more strings, each at most `width` chars wide.
pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
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

// -----------------------------------------------------------------------
// Markdown-lite rendering
// -----------------------------------------------------------------------

/// Block-level state for markdown-lite rendering.
///
/// Tracks whether we're inside a fenced code block (```...```) and
/// the optional language tag from the opening fence.
#[derive(Default, Clone)]
pub struct MarkdownState {
    in_fence: bool,
}

/// Render a single line of text with inline markdown-lite formatting.
///
/// Supports: `` `code spans` ``, `**bold**`, `# headers`, `- list markers`.
/// Fenced code blocks (```...```) are handled via [`MarkdownState`].
///
/// Returns a `Vec<Span>` ready to be assembled into a `Line`.
pub fn render_markdown_line(
    line: &str,
    theme: &Theme,
    state: &mut MarkdownState,
) -> Vec<Span<'static>> {
    let glyphs = theme.glyphs();

    // --- Fenced code block ---
    if state.in_fence {
        if line.trim_start().starts_with("```") {
            state.in_fence = false;
            return vec![Span::styled(
                format!("{} ```", glyphs.gutter),
                theme.dim(),
            )];
        }
        // Render line with gutter prefix, fg_dim style.
        return vec![Span::styled(
            format!("{} {}", glyphs.gutter, line),
            theme.dim(),
        )];
    }

    if line.trim_start().starts_with("```") {
        state.in_fence = true;
        let lang = line.trim_start().trim_start_matches('`').trim();
        let label = if lang.is_empty() {
            format!("{} ```", glyphs.gutter)
        } else {
            format!("{} ``` {}", glyphs.gutter, lang)
        };
        return vec![Span::styled(label, theme.dim())];
    }

    // --- Headers ---
    if let Some(rest) = line.strip_prefix("# ") {
        return vec![Span::styled(rest.to_string(), theme.header_style())];
    }
    if let Some(rest) = line.strip_prefix("## ") {
        return vec![Span::styled(rest.to_string(), theme.header_style())];
    }

    // --- List markers ---
    let (prefix, content) = if let Some(rest) = line.strip_prefix("- ") {
        (Some(format!("{} ", glyphs.bullet)), rest)
    } else {
        (None, line)
    };

    // --- Inline parsing: `code` and **bold** ---
    let mut spans = Vec::new();
    if let Some(p) = prefix {
        spans.push(Span::styled(p, theme.muted()));
    }

    let mut current = String::new();
    let mut current_style = theme.fg_style();
    let mut in_code = false;
    let mut in_bold = false;
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Code span toggle: backtick
        if chars[i] == '`' {
            if in_code {
                // Closing backtick — toggle off.
                if !current.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut current), current_style));
                }
                in_code = false;
                current_style = if in_bold { theme.bold_style() } else { theme.fg_style() };
                i += 1;
                continue;
            } else {
                // Opening backtick — check if there's a closing one ahead.
                let has_closing = chars[i + 1..].contains(&'`');
                if has_closing {
                    if !current.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut current), current_style));
                    }
                    in_code = true;
                    current_style = theme.code_span_style();
                    i += 1;
                    continue;
                }
                // No closing backtick — treat as literal text (fall through).
            }
        }

        // Bold toggle: ** (skip while inside a code span — code content is literal)
        if !in_code && i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if in_bold {
                // Closing ** — toggle off.
                if !current.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut current), current_style));
                }
                in_bold = false;
                current_style = if in_code { theme.code_span_style() } else { theme.fg_style() };
                i += 2;
                continue;
            } else {
                // Opening ** — check if there's a closing ** ahead.
                let rest = &chars[i + 2..];
                let has_closing = rest.windows(2).any(|w| w[0] == '*' && w[1] == '*');
                if has_closing {
                    if !current.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut current), current_style));
                    }
                    in_bold = true;
                    current_style = if in_code { theme.code_span_style() } else { theme.bold_style() };
                    i += 2;
                    continue;
                }
                // No closing ** — treat as literal text (fall through).
            }
        }

        current.push(chars[i]);
        i += 1;
    }

    if !current.is_empty() {
        spans.push(Span::styled(current, current_style));
    }

    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_emoji_removes_emoji() {
        assert_eq!(strip_emoji("hello 👋 world"), "hello  world");
        assert_eq!(strip_emoji("тест ✅ ок"), "тест  ок");
    }

    #[test]
    fn strip_emoji_keeps_cyrillic_and_arrows() {
        assert_eq!(strip_emoji("Привет → мир"), "Привет → мир");
        assert_eq!(strip_emoji("┌─┐"), "┌─┐");
    }

    #[test]
    fn wrap_text_short_line() {
        assert_eq!(wrap_text("hello", 10), vec!["hello"]);
    }

    #[test]
    fn wrap_text_long_line() {
        let result = wrap_text("abcdef", 3);
        assert_eq!(result, vec!["abc", "def"]);
    }

    #[test]
    fn wrap_text_zero_width() {
        assert_eq!(wrap_text("hello", 0), vec!["hello"]);
    }

    // --- Markdown-lite tests ---

    fn md_spans(line: &str) -> Vec<String> {
        let theme = Theme::default_dark();
        let mut state = MarkdownState::default();
        render_markdown_line(line, &theme, &mut state)
            .into_iter()
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn md_code_span() {
        // `code` → should have separate spans: "", "code", ""
        let spans = md_spans("hello `world` end");
        assert!(spans.iter().any(|s| s == "world"), "code span content should be a separate span: {:?}", spans);
        assert!(spans.iter().any(|s| s == "hello "), "text before code should be a span: {:?}", spans);
        assert!(spans.iter().any(|s| s == " end"), "text after code should be a span: {:?}", spans);
    }

    #[test]
    fn md_bold() {
        // **bold** → should have separate spans
        let spans = md_spans("this is **important** text");
        assert!(spans.iter().any(|s| s == "important"), "bold content should be a separate span: {:?}", spans);
        assert!(spans.iter().any(|s| s == "this is "), "text before bold: {:?}", spans);
    }

    #[test]
    fn md_mixed_code_and_bold() {
        // `code` and **bold** in the same line
        let spans = md_spans("use `fmt` for **bold** text");
        assert!(spans.iter().any(|s| s == "fmt"), "code span: {:?}", spans);
        assert!(spans.iter().any(|s| s == "bold"), "bold span: {:?}", spans);
    }

    #[test]
    fn md_unclosed_marker_is_plain_text() {
        // Unclosed ` should render as plain text (no code span)
        let spans = md_spans("hello `world");
        // Should be a single span with the full text (unclosed backtick is literal)
        let combined: String = spans.join("");
        assert_eq!(combined, "hello `world", "unclosed backtick should be literal: {:?}", spans);
    }

    #[test]
    fn md_unclosed_bold_is_plain_text() {
        // Unclosed ** should render as plain text
        let spans = md_spans("hello **world");
        let combined: String = spans.join("");
        // The ** is consumed by the parser but the content is still there
        assert!(combined.contains("world"), "unclosed bold content should still appear: {:?}", spans);
    }

    #[test]
    fn md_header() {
        let theme = Theme::default_dark();
        let mut state = MarkdownState::default();
        let spans = render_markdown_line("# Title", &theme, &mut state);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "Title");
    }

    #[test]
    fn md_list_marker() {
        let theme = Theme::default_dark();
        let mut state = MarkdownState::default();
        let spans = render_markdown_line("- item", &theme, &mut state);
        // First span is the bullet, second is the content
        let combined: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(combined.contains("item"), "list item content: {:?}", spans);
    }

    #[test]
    fn md_bold_inside_code_span_is_literal() {
        // `a**b` should render as a single code span with literal content "a**b",
        // NOT split into code/bold segments.
        let spans = md_spans("`a**b`");
        // The code span content should contain the literal `**` characters.
        assert!(
            spans.iter().any(|s| s == "a**b"),
            "`a**b` should be a single code span with literal **: {:?}",
            spans
        );
        // There should NOT be a separate "b" span (which would indicate bold splitting).
        assert!(
            !spans.iter().any(|s| s == "b"),
            "`a**b` should not split on ** inside code span: {:?}",
            spans
        );
    }
}
