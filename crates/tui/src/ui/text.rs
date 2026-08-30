//! Text utility helpers — emoji stripping, line wrapping, and markdown-lite rendering.

use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme::Theme;

/// Tab stop used when expanding `\t` for wrap/pad (columnar `ps` / tables).
const TAB_STOP: usize = 8;

/// Upper bound for **explicit cursor jumps** (`CSI G`/`C`, tabs) while
/// replaying a line. Command output is untrusted: `ESC[1000000000G` would
/// otherwise make the next printable character pad a billion cells and hang
/// the TUI (#373 review). Natural left-to-right writing is not capped, so a
/// genuinely long line is never truncated.
const MAX_CURSOR_COL: usize = 4096;

/// Expand horizontal tabs to spaces at [`TAB_STOP`] boundaries so wrap and
/// pad agree with typical terminal columnar layout (#333).
pub(crate) fn expand_tabs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    for c in s.chars() {
        if c == '\t' {
            let spaces = TAB_STOP - (col % TAB_STOP);
            out.push_str(&" ".repeat(spaces));
            col += spaces;
        } else {
            out.push(c);
            col += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    out
}

/// One line of a terminal, addressed in **display columns**.
///
/// `None` marks the right half of a double-width character, so overwriting the
/// left half leaves a visible gap exactly as a real terminal does.
struct LineCells {
    cells: Vec<Option<String>>,
    col: usize,
}

impl LineCells {
    fn new() -> Self {
        Self { cells: Vec::new(), col: 0 }
    }

    fn pad_to(&mut self, n: usize) {
        let n = n.min(MAX_CURSOR_COL);
        while self.cells.len() < n {
            self.cells.push(Some(" ".to_string()));
        }
    }

    fn put(&mut self, c: char) {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if w == 0 {
            // Combining mark: attach to the nearest base cell to the left. A
            // wide character puts its base two columns back, not one.
            let mut k = self.col.min(self.cells.len());
            while k > 0 {
                k -= 1;
                if let Some(text) = self.cells[k].as_mut() {
                    text.push(c);
                    break;
                }
            }
            return;
        }
        // Writing into the right half of a wide character destroys the whole
        // character; its orphaned left half becomes a blank column.
        if matches!(self.cells.get(self.col), Some(None)) && self.col > 0 {
            self.cells[self.col - 1] = Some(" ".to_string());
        }
        self.pad_to(self.col);
        for i in 0..w {
            let val = if i == 0 { Some(c.to_string()) } else { None };
            if self.col + i < self.cells.len() {
                self.cells[self.col + i] = val;
            } else {
                self.cells.push(val);
            }
        }
        // If we clobbered the left half of a wide character, its orphaned
        // right half becomes a blank column.
        let next = self.col + w;
        if matches!(self.cells.get(next), Some(None)) {
            self.cells[next] = Some(" ".to_string());
        }
        self.col += w;
    }

    fn tab(&mut self) {
        let target = (self.col + (TAB_STOP - (self.col % TAB_STOP))).min(MAX_CURSOR_COL - 1);
        self.pad_to(target);
        self.col = target;
    }

    fn erase_to_end(&mut self) {
        self.cells.truncate(self.col);
    }

    fn erase_to_start(&mut self) {
        self.pad_to(self.col + 1);
        for cell in self.cells.iter_mut().take(self.col + 1) {
            *cell = Some(" ".to_string());
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for cell in &self.cells {
            if let Some(text) = cell {
                out.push_str(text);
            }
        }
        out.trim_end().to_string()
    }
}

/// Replay one line of raw command output as a terminal would, returning what
/// would be visible on screen.
///
/// Escape parsing and cursor movement happen in a **single pass** on purpose:
/// erase sequences act relative to the cursor, so stripping escapes separately
/// would lose the position they apply to. `\r` followed by `CSI K` is exactly
/// how progress bars clear the tail of the previous frame — handling the `\r`
/// but discarding the erase would leave the stale tail on screen, which is the
/// artifact this whole change exists to remove (#366).
///
/// Both the 7-bit (`ESC [`, `ESC ]`) and 8-bit (`U+009B`, `U+009D`) forms of
/// CSI and OSC are recognised. Sequences that only affect presentation (SGR
/// colours and the like) are discarded; those that move the cursor or erase
/// content (`K`, `G`, `C`, `D`) are applied. A sequence truncated at end of
/// line is dropped whole.
fn sanitize_line(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut line = LineCells::new();
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        let cp = c as u32;
        let esc_next = if c == '\u{1b}' && i + 1 < chars.len() {
            Some(chars[i + 1])
        } else {
            None
        };

        if esc_next == Some('[') || cp == 0x9B {
            let mut j = i + if c == '\u{1b}' { 2 } else { 1 };
            let start = j;
            while j < chars.len() {
                let b = chars[j] as u32;
                if (0x40..=0x7E).contains(&b) {
                    break;
                }
                j += 1;
            }
            if j >= chars.len() {
                break; // truncated sequence at end of line
            }
            let params: String = chars[start..j].iter().collect();
            let num: usize = params
                .trim_start_matches('?')
                .split(';')
                .next()
                .unwrap_or("")
                .parse()
                .unwrap_or(0);
            match chars[j] {
                'K' => match num {
                    1 => line.erase_to_start(),
                    2 => line.cells.clear(),
                    _ => line.erase_to_end(),
                },
                'G' => line.col = num.saturating_sub(1).min(MAX_CURSOR_COL - 1),
                'C' => line.col = line.col.saturating_add(num.max(1)).min(MAX_CURSOR_COL - 1),
                'D' => line.col = line.col.saturating_sub(num.max(1)),
                _ => {} // SGR and friends: presentation only
            }
            i = j + 1;
            continue;
        }

        if esc_next == Some(']') || cp == 0x9D {
            let mut j = i + if c == '\u{1b}' { 2 } else { 1 };
            while j < chars.len() {
                if chars[j] == '\u{7}' || chars[j] == '\u{9c}' {
                    j += 1; // BEL or 8-bit ST
                    break;
                }
                if chars[j] == '\u{1b}' && j + 1 < chars.len() && chars[j + 1] == '\\' {
                    j += 2;
                    break;
                }
                j += 1;
            }
            i = j;
            continue;
        }

        if c == '\u{1b}' {
            i += 2; // two-character escape (ESC(B, ESC=, …)
            continue;
        }

        match c {
            '\r' => line.col = 0,
            '\u{8}' => line.col = line.col.saturating_sub(1),
            '\t' => line.tab(),
            _ if cp < 0x20 || cp == 0x7F || (0x80..=0x9F).contains(&cp) => {}
            _ => line.put(c),
        }
        i += 1;
    }

    line.render()
}

/// Sanitise raw command output before it becomes chat lines (#366).
///
/// Command stdout is not text: it carries `\r`, backspaces and ANSI escapes.
/// ratatui writes span content to the terminal verbatim, so a stray `\r` moves
/// the physical cursor to column 0 and the rest of the frame is painted over
/// whatever was there — while ratatui's buffer still believes the cell is
/// unchanged and its diff emits nothing. That desync is what left redraw
/// artifacts on screen until a window resize forced a full repaint.
///
/// Each line is replayed independently by [`sanitize_line`]; newlines are
/// preserved so line counts stay identical.
pub(crate) fn sanitize_output(s: &str) -> String {
    // Fast path: the overwhelming majority of command output is plain text.
    // `\t` is left for `expand_tabs`; without cursor motion its column is
    // unambiguous, so there is nothing to replay.
    let needs_work = s.chars().any(|c| {
        let cp = c as u32;
        c != '\t' && c != '\n' && (cp < 0x20 || cp == 0x7F || (0x80..=0x9F).contains(&cp))
    });
    if !needs_work {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&sanitize_line(line));
    }
    out
}

/// Strip emoji and other non-renderable Unicode characters from a string.
/// Windows terminal (conhost) can't display most emojis, so they show as '?'.
/// Conservative whitelist: ASCII, Cyrillic, Latin, punctuation, arrows, math, box drawing.
///
/// Control characters are **not** whitelisted: `ESC`/`CR`/`BS` reaching the
/// terminal desynchronise ratatui's buffer from the physical screen (#366).
/// `\t` and `\n` are kept — wrapping and [`expand_tabs`] depend on them.
pub(crate) fn strip_emoji(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            let cp = c as u32;
            c == '\n' || c == '\t'
            || ((0x20..=0x024F).contains(&cp)   // ASCII + Latin + Latin Extended
                && cp != 0x7F                   // minus DEL
                && !(0x80..=0x9F).contains(&cp))// minus C1 controls
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

/// Wrap a single line of text to fit within `width` **display columns**
/// (unicode-width, matching ratatui). Tabs are expanded first (#333).
///
/// Returns one or more strings, each at most `width` columns wide. A single
/// character wider than `width` is placed alone on its own line. Zero-width
/// combining marks stay with their preceding base character.
pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut result = Vec::new();
    for line in text.lines() {
        let line = expand_tabs(line);
        let line_width = UnicodeWidthStr::width(line.as_str());
        if line_width <= width {
            result.push(line);
            continue;
        }
        let mut current = String::new();
        let mut col = 0usize;
        for c in line.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            // Only break before a positive-width char; combining marks (cw=0)
            // stay attached to the previous base.
            if cw > 0 && col > 0 && col + cw > width {
                result.push(std::mem::take(&mut current));
                col = 0;
            }
            current.push(c);
            col += cw;
        }
        if !current.is_empty() {
            result.push(current);
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

    #[test]
    fn wrap_text_wide_chars_by_display_columns() {
        // Each CJK char is 2 columns; width 4 → two chars per line.
        assert_eq!(wrap_text("你好世界", 4), vec!["你好", "世界"]);
    }

    #[test]
    fn wrap_text_expands_tabs_for_columns() {
        let lines = wrap_text("a\tb", 16);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "a       b");
        assert_eq!(UnicodeWidthStr::width(lines[0].as_str()), 9);
    }

    #[test]
    fn wrap_text_keeps_combining_mark_with_base() {
        let acute = '\u{0301}';
        let s = format!("a{acute}b");
        assert_eq!(wrap_text(&s, 1), vec![format!("a{acute}"), "b".into()]);
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

    #[test]
    fn md_empty_string() {
        let spans = md_spans("");
        assert!(spans.is_empty() || spans == vec![""]);
    }

    #[test]
    fn md_only_markers() {
        // Just `**` with nothing between should be literal text.
        let spans = md_spans("**");
        assert!(!spans.is_empty());
    }

    #[test]
    fn md_multiple_code_spans() {
        let spans = md_spans("`foo` and `bar`");
        assert!(spans.iter().any(|s| s == "foo"));
        assert!(spans.iter().any(|s| s == "bar"));
    }

    // ── #366: command output sanitisation ────────────────────────────────

    #[test]
    fn sanitize_keeps_plain_output_untouched() {
        let plain = "total 12\ndrwxr-xr-x 2 user user 4096 Aug 30 10:00 dir\n";
        assert_eq!(sanitize_output(plain), plain);
        // Tabs alone take the fast path: without cursor motion their column is
        // unambiguous and `expand_tabs` handles them at wrap time.
        assert_eq!(sanitize_output("col1\tcol2"), "col1\tcol2");
    }

    #[test]
    fn sanitize_resolves_progress_bar_frames() {
        // `hf download` style: frames separated by \r, no \n at all.
        let src = "Fetching 4 files:   0%| | 0/4\rFetching 4 files:  25%|# | 1/4\rFetching 4 files: 100%|##| 4/4";
        assert_eq!(sanitize_output(src), "Fetching 4 files: 100%|##| 4/4");
    }

    #[test]
    fn sanitize_overwrite_keeps_tail_when_nothing_erases_it() {
        // No erase sequence: a real terminal keeps the tail of the longer
        // previous frame underneath.
        assert_eq!(sanitize_output("abcdefghij\rXY"), "XYcdefghij");
    }

    #[test]
    fn sanitize_applies_erase_to_end_of_line() {
        // `\r` + CSI K is how progress bars clear the previous frame. Dropping
        // the erase would leave the stale tail — the artifact of #366.
        assert_eq!(sanitize_output("abcdef\r\u{1b}[KXY"), "XY");
        // CSI 2K erases the whole line.
        assert_eq!(sanitize_output("abcdef\r\u{1b}[2KXY"), "XY");
        // CSI 1K erases up to and including the cursor.
        assert_eq!(sanitize_output("abcdef\u{1b}[3G\u{1b}[1KX"), "  Xdef");
    }

    #[test]
    fn sanitize_handles_eight_bit_c1_introducers() {
        // U+009B is CSI, U+009D is OSC. Dropping only the introducer would
        // leave the parameter bytes as visible text.
        assert_eq!(sanitize_output("a\u{9b}31mred"), "ared");
        assert_eq!(sanitize_output("a\u{9d}0;title\u{7}b"), "ab");
    }

    #[test]
    fn sanitize_counts_display_columns_not_chars() {
        // Backspace after a tab lands on column 7, not on the tab character.
        assert_eq!(sanitize_output("A\t\u{8}X"), "A      X");
        // Two single-width chars cover only the first wide character.
        assert_eq!(sanitize_output("\u{4f60}\u{597d}\rAB"), "AB\u{597d}");
        // Clobbering the left half of a wide char leaves a blank column.
        assert_eq!(sanitize_output("\u{4f60}x\rA"), "A x");
    }

    #[test]
    fn sanitize_handles_cursor_movement() {
        assert_eq!(sanitize_output("abcdef\u{1b}[3GX"), "abXdef");
        assert_eq!(sanitize_output("abc\u{1b}[2DX"), "aXc");
        assert_eq!(sanitize_output("ab\u{1b}[2CX"), "ab  X");
    }

    #[test]
    fn sanitize_handles_backspace() {
        assert_eq!(sanitize_output("loading |\u{8}/\u{8}-"), "loading -");
    }

    #[test]
    fn sanitize_strips_presentation_sequences() {
        // `ls --color=always`
        assert_eq!(
            sanitize_output("\u{1b}[0m\u{1b}[01;34mdir\u{1b}[0m  file"),
            "dir  file"
        );
        // `grep --color`: SGR plus erase-to-end-of-line at the cursor.
        assert_eq!(
            sanitize_output("pre\u{1b}[01;31m\u{1b}[Kmatch\u{1b}[m\u{1b}[Kpost"),
            "prematchpost"
        );
        // Truncated sequence at end of line is dropped whole.
        assert_eq!(sanitize_output("text\u{1b}[38;5;"), "text");
    }

    #[test]
    fn sanitize_strips_osc_sequences() {
        // OSC 7 (cwd report) terminated by BEL.
        assert_eq!(sanitize_output("\u{1b}]7;file://host/tmp\u{7}text"), "text");
        // OSC terminated by ST (ESC \).
        assert_eq!(sanitize_output("\u{1b}]0;title\u{1b}\\body"), "body");
    }

    #[test]
    fn sanitize_drops_stray_c0_and_del() {
        assert_eq!(sanitize_output("a\u{0}b\u{7f}c"), "abc");
    }

    #[test]
    fn sanitize_preserves_line_structure() {
        assert_eq!(sanitize_output("line1\nline2"), "line1\nline2");
        // `split('\n')` is used rather than `lines()`, so a trailing newline
        // survives and `str::lines()` on the result sees the same count.
        assert_eq!(sanitize_output("a\u{1b}[0m\nb\n"), "a\nb\n");
        assert_eq!(sanitize_output("a\u{1b}[0m\nb\n").lines().count(), 2);
    }

    #[test]
    fn sanitize_clobbering_wide_char_right_half_blanks_the_left() {
        // Writing into the right half destroys the whole wide character.
        assert_eq!(sanitize_output("\u{5bbd}\u{1b}[2GA"), " A");
    }

    #[test]
    fn sanitize_keeps_combining_mark_after_wide_char() {
        // The base cell of a wide character is two columns back, not one.
        assert_eq!(sanitize_output("\u{4f60}\u{301}"), "\u{4f60}\u{301}");
    }

    #[test]
    fn sanitize_accepts_c1_string_terminator() {
        assert_eq!(sanitize_output("a\u{9d}0;title\u{9c}b"), "ab");
    }

    #[test]
    fn sanitize_bounds_cursor_jumps() {
        // Untrusted output must not make us pad a billion cells (#373 review).
        let out = sanitize_output("\u{1b}[1000000000GX");
        assert!(out.ends_with('X'));
        assert_eq!(out.chars().count(), MAX_CURSOR_COL);
        let out = sanitize_output("a\u{1b}[999999999CX");
        assert!(out.ends_with('X'));
        assert_eq!(out.chars().count(), MAX_CURSOR_COL);
    }

    #[test]
    fn sanitize_does_not_truncate_long_lines() {
        // Only explicit cursor jumps are clamped; ordinary writing is not.
        let long: String = "y".repeat(9000);
        let src = format!("\u{1b}[0m{long}");
        assert_eq!(sanitize_output(&src), long);
    }

    #[test]
    fn strip_emoji_removes_control_characters() {
        // #366: ESC/CR/BS used to pass the `cp <= 0x024F` whitelist.
        assert_eq!(strip_emoji("a\u{1b}b"), "ab");
        assert_eq!(strip_emoji("a\rb"), "ab");
        assert_eq!(strip_emoji("a\u{8}b"), "ab");
        assert_eq!(strip_emoji("a\u{7f}b"), "ab");
        assert_eq!(strip_emoji("a\u{9b}b"), "ab");
        // Newlines and tabs are still needed by wrapping and expand_tabs.
        assert_eq!(strip_emoji("a\nb\tc"), "a\nb\tc");
        // Regression guard: normal text is unaffected.
        assert_eq!(strip_emoji("Привет — ok ✓"), "Привет — ok ✓");
    }
}
