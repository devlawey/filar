//! Terminal model backed by `alacritty_terminal`.
//!
//! [`TerminalModel`] wraps an `alacritty_terminal::Term` and a
//! `vte::ansi::Processor`. Output bytes from a PTY or SSH channel are fed into
//! the model via [`TerminalModel::feed`], and the grid is rendered to a ratatui
//! `Frame` via [`TerminalModel::render`]. Keyboard events are converted to
//! terminal input bytes via [`key_to_bytes`].

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as VteColor, NamedColor, Rgb};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as RatLine, Span};
use ratatui::Frame;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// ---------------------------------------------------------------------------
// TermDimensions — helper implementing the Dimensions trait
// ---------------------------------------------------------------------------

/// Simple dimensions struct for creating/resizing a `Term`.
struct TermDimensions {
    cols: usize,
    lines: usize,
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

// ---------------------------------------------------------------------------
// TerminalModel
// ---------------------------------------------------------------------------

/// A terminal emulator model backed by `alacritty_terminal::Term`.
///
/// Output bytes from a PTY or SSH channel are fed into the model, which
/// maintains a grid of cells with full ANSI escape sequence support
/// (colors, cursor movement, alternate screen, wide chars, scrollback, etc.).
pub struct TerminalModel {
    term: Term<VoidListener>,
    processor: alacritty_terminal::vte::ansi::Processor,
}

impl TerminalModel {
    /// Create a new terminal model with the given dimensions.
    pub fn new(cols: u16, rows: u16) -> Self {
        let config = Config {
            scrolling_history: 10_000,
            ..<_>::default()
        };
        let dims = TermDimensions {
            cols: cols as usize,
            lines: rows as usize,
        };
        let term = Term::new(config, &dims, VoidListener);
        let processor = <_>::default();
        Self { term, processor }
    }

    /// Feed output bytes (from PTY/SSH) into the terminal model.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// Resize the terminal to the given dimensions.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let dims = TermDimensions {
            cols: cols as usize,
            lines: rows as usize,
        };
        self.term.resize(dims);
    }

    /// Get the number of columns in the terminal.
    pub fn cols(&self) -> u16 {
        self.term.grid().columns() as u16
    }

    /// Get the number of rows (screen lines) in the terminal.
    pub fn rows(&self) -> u16 {
        self.term.grid().screen_lines() as u16
    }

    /// Get the cursor position (column, row) if the cursor is visible.
    pub fn cursor_position(&self) -> Option<(u16, u16)> {
        if !self.term.mode().contains(TermMode::SHOW_CURSOR) {
            return None;
        }
        let point = self.term.grid().cursor.point;
        // Clamp to u16 — terminal should never exceed this.
        let col = point.column.0.min(u16::MAX as usize) as u16;
        let row = point.line.0.max(0) as u16;
        Some((col, row))
    }

    /// Scroll the terminal display by `delta` lines (negative = up).
    ///
    /// This scrolls through the scrollback history without sending input
    /// to the PTY/SSH channel.
    pub fn scroll_display(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    /// Reset scrollback to the bottom (latest output).
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Check whether the application in the terminal has requested mouse
    /// tracking (click, drag, or motion mode).
    ///
    /// This checks `MOUSE_MODE` only (REPORT_CLICK | MOTION | DRAG), not
    /// `SGR_MOUSE`, which is an encoding flag rather than a tracking flag.
    pub fn mouse_mode(&self) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Check whether the application has requested SGR mouse encoding (1006).
    ///
    /// When `true`, mouse events should be encoded as SGR sequences
    /// (`\x1b[<{button};{x};{y}M/m`).  When `false` but `mouse_mode()` is
    /// `true`, legacy encoding (`\x1b[M{b}{x}{y}`) should be used instead.
    pub fn sgr_mouse(&self) -> bool {
        self.term.mode().contains(TermMode::SGR_MOUSE)
    }

    /// Check whether the terminal is in alternate screen mode.
    ///
    /// In alt-screen (used by `less`, `vim`, `htop`, etc.) scrollback
    /// scrolling doesn't apply — the wheel should be translated to arrow
    /// keys instead.
    pub fn is_alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Render the terminal grid to a ratatui frame.
    ///
    /// Each cell is rendered as a character with its foreground/background
    /// colors and text attributes (bold, italic, underline, etc.). The cursor
    /// cell is rendered with inverted colors when visible.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let grid = self.term.grid();
        let num_cols = grid.columns();
        let num_lines = grid.screen_lines();

        // Cursor position for inversion.
        let cursor = self.cursor_position();

        // Determine how many lines fit in the area.
        let max_rows = (area.height as usize).min(num_lines);
        let max_cols = (area.width as usize).min(num_cols);

        let mut lines: Vec<RatLine> = Vec::with_capacity(max_rows);

        for row in 0..max_rows {
            let grid_row = &grid[Line(row as i32)];
            let mut spans: Vec<Span> = Vec::new();
            let mut current_text = String::new();
            let mut current_style = Style::default();
            let mut style_start = 0usize;

            for col in 0..max_cols {
                let cell = &grid_row[Column(col)];

                // Skip wide char spacers.
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                // Determine if this is the cursor cell.
                let is_cursor = cursor.is_some_and(|(c, r)| c as usize == col && r as usize == row);

                let (fg, bg) = cell_colors(cell, is_cursor);
                let style = build_style(cell.flags, fg, bg);

                // If style changed, flush the current span.
                if col == style_start {
                    current_style = style;
                } else if style != current_style {
                    if !current_text.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut current_text), current_style));
                    }
                    current_style = style;
                    style_start = col;
                }

                current_text.push(cell.c);
            }

            // Flush remaining text.
            if !current_text.is_empty() {
                spans.push(Span::styled(current_text, current_style));
            }

            // Ensure each line is at least an empty line.
            if spans.is_empty() {
                spans.push(Span::raw(""));
            }

            lines.push(RatLine::from(spans));
        }

        // Render all lines as a paragraph.
        let paragraph = ratatui::widgets::Paragraph::new(lines);
        frame.render_widget(paragraph, area);

        // Set the cursor position if visible.
        if let Some((col, row)) = cursor {
            if col < area.width && row < area.height {
                frame.set_cursor_position((area.x + col, area.y + row));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Color mapping
// ---------------------------------------------------------------------------

/// Extract the foreground and background colors from a cell, applying
/// inversion for the cursor or the `INVERSE` flag.
fn cell_colors(cell: &Cell, is_cursor: bool) -> (Color, Color) {
    let mut fg = map_color(&cell.fg);
    let mut bg = map_color(&cell.bg);

    // Invert on INVERSE flag or cursor cell.
    if cell.flags.contains(Flags::INVERSE) || is_cursor {
        std::mem::swap(&mut fg, &mut bg);
    }

    (fg, bg)
}

/// Map a `vte::ansi::Color` to a ratatui `Color`.
fn map_color(color: &VteColor) -> Color {
    match color {
        VteColor::Named(named) => map_named_color(*named),
        VteColor::Spec(Rgb { r, g, b }) => Color::Rgb(*r, *g, *b),
        VteColor::Indexed(idx) => map_indexed_color(*idx),
    }
}

/// Map a `NamedColor` to a ratatui `Color`.
fn map_named_color(named: NamedColor) -> Color {
    match named {
        NamedColor::Black => Color::Black,
        NamedColor::Red => Color::Red,
        NamedColor::Green => Color::Green,
        NamedColor::Yellow => Color::Yellow,
        NamedColor::Blue => Color::Blue,
        NamedColor::Magenta => Color::Magenta,
        NamedColor::Cyan => Color::Cyan,
        NamedColor::White => Color::Gray,
        NamedColor::BrightBlack => Color::DarkGray,
        NamedColor::BrightRed => Color::LightRed,
        NamedColor::BrightGreen => Color::LightGreen,
        NamedColor::BrightYellow => Color::LightYellow,
        NamedColor::BrightBlue => Color::LightBlue,
        NamedColor::BrightMagenta => Color::LightMagenta,
        NamedColor::BrightCyan => Color::LightCyan,
        NamedColor::BrightWhite => Color::White,
        NamedColor::Foreground => Color::Reset,
        NamedColor::Background => Color::Reset,
        NamedColor::Cursor => Color::Reset,
        // Dim variants — fall back to the normal color.
        NamedColor::DimBlack => Color::Black,
        NamedColor::DimRed => Color::Red,
        NamedColor::DimGreen => Color::Green,
        NamedColor::DimYellow => Color::Yellow,
        NamedColor::DimBlue => Color::Blue,
        NamedColor::DimMagenta => Color::Magenta,
        NamedColor::DimCyan => Color::Cyan,
        NamedColor::DimWhite => Color::DarkGray,
        NamedColor::BrightForeground => Color::Reset,
        NamedColor::DimForeground => Color::Reset,
    }
}

/// Map an indexed color (0-255) to a ratatui `Color`.
fn map_indexed_color(idx: u8) -> Color {
    if idx < 16 {
        // Standard ANSI colors.
        map_named_color(match idx {
            0 => NamedColor::Black,
            1 => NamedColor::Red,
            2 => NamedColor::Green,
            3 => NamedColor::Yellow,
            4 => NamedColor::Blue,
            5 => NamedColor::Magenta,
            6 => NamedColor::Cyan,
            7 => NamedColor::White,
            8 => NamedColor::BrightBlack,
            9 => NamedColor::BrightRed,
            10 => NamedColor::BrightGreen,
            11 => NamedColor::BrightYellow,
            12 => NamedColor::BrightBlue,
            13 => NamedColor::BrightMagenta,
            14 => NamedColor::BrightCyan,
            _ => NamedColor::BrightWhite,
        })
    } else if idx < 232 {
        // 6x6x6 color cube: 16 + 36*r + 6*g + b, where r,g,b ∈ [0,5].
        let idx = idx - 16;
        let r = idx / 36;
        let g = (idx % 36) / 6;
        let b = idx % 6;
        // Convert to RGB: values are 0, 95, 135, 175, 215, 255.
        let to_rgb = |v: u8| -> u8 {
            if v == 0 {
                0
            } else {
                55 + v * 40
            }
        };
        Color::Rgb(to_rgb(r), to_rgb(g), to_rgb(b))
    } else {
        // Grayscale ramp: 24 shades from dark to light.
        let v = 8 + (idx - 232) * 10;
        Color::Rgb(v, v, v)
    }
}

/// Build a ratatui `Style` from cell flags and colors.
fn build_style(flags: Flags, fg: Color, bg: Color) -> Style {
    let mut modifier = Modifier::empty();
    if flags.contains(Flags::BOLD) {
        modifier |= Modifier::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        modifier |= Modifier::ITALIC;
    }
    if flags.contains(Flags::UNDERLINE) {
        modifier |= Modifier::UNDERLINED;
    }
    if flags.contains(Flags::DIM) {
        modifier |= Modifier::DIM;
    }
    if flags.contains(Flags::STRIKEOUT) {
        modifier |= Modifier::CROSSED_OUT;
    }
    if flags.contains(Flags::HIDDEN) {
        // Hidden text: render as space with no foreground.
        return Style::default().bg(bg);
    }

    Style::default().fg(fg).bg(bg).add_modifier(modifier)
}

// ---------------------------------------------------------------------------
// Key event → terminal input bytes
// ---------------------------------------------------------------------------

/// Convert a crossterm `KeyEvent` to the byte sequence that should be sent
/// to the terminal (PTY/SSH channel).
///
/// Handles regular characters, control characters, and special keys
/// (arrows, function keys, Home/End, etc.).
pub fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let mut bytes = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Control character: map Ctrl+<char> to the control code.
                ctrl_char_to_bytes(c)
            } else {
                // Handle Shift for letters — crossterm already gives us the
                // correct character (uppercase when Shift is pressed).
                let mut s = c.to_string();
                if alt {
                    // Alt prefix: ESC followed by the character.
                    s = format!("\x1b{s}");
                }
                s.into_bytes()
            }
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(), // Shift+Tab
        KeyCode::Backspace => b"\x7f".to_vec(), // DEL (most common)
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => {
            if ctrl {
                b"\x1b[1;5A".to_vec()
            } else {
                b"\x1b[A".to_vec()
            }
        }
        KeyCode::Down => {
            if ctrl {
                b"\x1b[1;5B".to_vec()
            } else {
                b"\x1b[B".to_vec()
            }
        }
        KeyCode::Right => {
            if ctrl {
                b"\x1b[1;5C".to_vec()
            } else {
                b"\x1b[C".to_vec()
            }
        }
        KeyCode::Left => {
            if ctrl {
                b"\x1b[1;5D".to_vec()
            } else {
                b"\x1b[D".to_vec()
            }
        }
        KeyCode::Home => {
            if ctrl {
                b"\x1b[1;5H".to_vec()
            } else {
                b"\x1b[H".to_vec()
            }
        }
        KeyCode::End => {
            if ctrl {
                b"\x1b[1;5F".to_vec()
            } else {
                b"\x1b[F".to_vec()
            }
        }
        KeyCode::PageUp => {
            if shift {
                b"\x1b[5;2~".to_vec()
            } else {
                b"\x1b[5~".to_vec()
            }
        }
        KeyCode::PageDown => {
            if shift {
                b"\x1b[6;2~".to_vec()
            } else {
                b"\x1b[6~".to_vec()
            }
        }
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(1) => b"\x1bOP".to_vec(),
        KeyCode::F(2) => b"\x1bOQ".to_vec(),
        KeyCode::F(3) => b"\x1bOR".to_vec(),
        KeyCode::F(4) => b"\x1bOS".to_vec(),
        KeyCode::F(5) => b"\x1b[15~".to_vec(),
        KeyCode::F(6) => b"\x1b[17~".to_vec(),
        KeyCode::F(7) => b"\x1b[18~".to_vec(),
        KeyCode::F(8) => b"\x1b[19~".to_vec(),
        KeyCode::F(9) => b"\x1b[20~".to_vec(),
        KeyCode::F(10) => b"\x1b[21~".to_vec(),
        KeyCode::F(11) => b"\x1b[23~".to_vec(),
        KeyCode::F(12) => b"\x1b[24~".to_vec(),
        _ => Vec::new(),
    };

    // Handle Alt prefix for non-char keys.
    if alt && !bytes.is_empty() && !bytes.starts_with(b"\x1b") {
        let mut prefixed = vec![0x1b];
        prefixed.extend_from_slice(&bytes);
        bytes = prefixed;
    }

    bytes
}

/// Convert a Ctrl+<char> combination to its control code byte(s).
fn ctrl_char_to_bytes(c: char) -> Vec<u8> {
    let lower = c.to_ascii_lowercase();
    let byte = match lower {
        'a'..='z' => (lower as u8) - b'a' + 1, // Ctrl+A = 0x01, ..., Ctrl+Z = 0x1A
        '[' => 0x1b, // Ctrl+[ = ESC
        '\\' => 0x1c, // Ctrl+\ = FS
        ']' => 0x1d,  // Ctrl+] = GS
        '^' => 0x1e,  // Ctrl+^ = RS
        '_' => 0x1f,  // Ctrl+_ = US
        ' ' => 0x00,  // Ctrl+Space = NUL
        _ => return c.to_string().into_bytes(),
    };
    vec![byte]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_model_create() {
        let model = TerminalModel::new(80, 24);
        assert_eq!(model.cols(), 80);
        assert_eq!(model.rows(), 24);
    }

    #[test]
    fn terminal_model_feed_text() {
        let mut model = TerminalModel::new(80, 24);
        model.feed(b"hello world");
        // The text should be in the grid.
        let grid = model.term.grid();
        let first_row = &grid[Line(0)];
        let cell = &first_row[Column(0)];
        assert_eq!(cell.c, 'h');
    }

    #[test]
    fn terminal_model_resize() {
        let mut model = TerminalModel::new(80, 24);
        model.resize(120, 30);
        assert_eq!(model.cols(), 120);
        assert_eq!(model.rows(), 30);
    }

    #[test]
    fn terminal_model_cursor_position() {
        let mut model = TerminalModel::new(80, 24);
        // Cursor starts at (0, 0).
        assert_eq!(model.cursor_position(), Some((0, 0)));

        // Feed some text — cursor should move.
        model.feed(b"hello");
        let pos = model.cursor_position();
        assert_eq!(pos, Some((5, 0)));
    }

    #[test]
    fn key_to_bytes_regular_char() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(key_to_bytes(key), b"a");
    }

    #[test]
    fn key_to_bytes_ctrl_c() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_to_bytes(key), b"\x03");
    }

    #[test]
    fn key_to_bytes_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_to_bytes(key), b"\r");
    }

    #[test]
    fn key_to_bytes_arrow_keys() {
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(key_to_bytes(up), b"\x1b[A");

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(key_to_bytes(down), b"\x1b[B");

        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(key_to_bytes(right), b"\x1b[C");

        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(key_to_bytes(left), b"\x1b[D");
    }

    #[test]
    fn key_to_bytes_backspace() {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(key_to_bytes(key), b"\x7f");
    }

    #[test]
    fn key_to_bytes_alt_char() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(key_to_bytes(key), b"\x1ba");
    }

    #[test]
    fn map_color_named() {
        assert_eq!(map_color(&VteColor::Named(NamedColor::Red)), Color::Red);
        assert_eq!(
            map_color(&VteColor::Named(NamedColor::BrightBlue)),
            Color::LightBlue
        );
    }

    #[test]
    fn map_color_spec() {
        let rgb = Rgb { r: 100, g: 150, b: 200 };
        assert_eq!(
            map_color(&VteColor::Spec(rgb)),
            Color::Rgb(100, 150, 200)
        );
    }

    #[test]
    fn map_indexed_color_grayscale() {
        // Index 232 = darkest grayscale.
        let c = map_indexed_color(232);
        assert_eq!(c, Color::Rgb(8, 8, 8));

        // Index 255 = lightest grayscale.
        let c = map_indexed_color(255);
        assert_eq!(c, Color::Rgb(238, 238, 238));
    }
}
