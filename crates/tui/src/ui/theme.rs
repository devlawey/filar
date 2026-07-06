//! Color theme for the TUI.
//!
//! All `Color::*` literals in the `ui` module live here.  Every other
//! submodule references colours exclusively through [`Theme`] fields or
//! the convenience style helpers below.
//!
//! The palette is deliberately restrained — one accent colour and three
//! shades of gray — following `docs/DESIGN_PHILOSOPHY.md` §2 (Единая тема).

use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;

/// Unicode/ASCII glyph set for the TUI.
///
/// Modern terminals (Windows Terminal, detected via `WT_SESSION`) get the
/// full Unicode set; conhost gets ASCII fallbacks so nothing shows as `?`.
///
/// See `docs/DESIGN_PHILOSOPHY.md` §7 (Windows-first, деградация — предусмотрена).
pub struct Glyphs {
    /// Input prompt: `❯` or `>`.
    pub prompt: &'static str,
    /// Vertical gutter for code/output: `│` or `|`.
    pub gutter: &'static str,
    /// Horizontal separator: `─` or `-`.
    pub separator: &'static str,
    /// Approved command status: `✓` or `[ok]`.
    pub success: &'static str,
    /// Denied/failed status: `✗` or `[no]`.
    pub danger: &'static str,
    /// System message bullet: `·` or `-`.
    pub middle_dot: &'static str,
    /// Collapsed arrow: `▸` or `+`.
    pub collapse_arrow: &'static str,
    /// Expanded arrow: `▾` or `-`.
    pub expand_arrow: &'static str,
    /// List bullet: `•` or `*`.
    pub bullet: &'static str,
    /// Target separator in status bar: `▸` or `>`.
    pub target_sep: &'static str,
}

impl Glyphs {
    /// Unicode glyphs for modern terminals.
    const UNICODE: Self = Self {
        prompt: "❯",
        gutter: "│",
        separator: "─",
        success: "✓",
        danger: "✗",
        middle_dot: "·",
        collapse_arrow: "▸",
        expand_arrow: "▾",
        bullet: "•",
        target_sep: "▸",
    };

    /// ASCII fallback for conhost and legacy terminals.
    const ASCII: Self = Self {
        prompt: ">",
        gutter: "|",
        separator: "-",
        success: "[ok]",
        danger: "[no]",
        middle_dot: "-",
        collapse_arrow: "+",
        expand_arrow: "-",
        bullet: "*",
        target_sep: ">",
    };

    /// Detect terminal capabilities and return the appropriate glyph set.
    /// Uses `WT_SESSION` env var (cached with `OnceLock`).
    pub fn detect() -> &'static Self {
        static IS_WT: OnceLock<bool> = OnceLock::new();
        let is_wt = *IS_WT.get_or_init(|| std::env::var("WT_SESSION").is_ok());
        if is_wt {
            &Self::UNICODE
        } else {
            &Self::ASCII
        }
    }
}

/// The colour palette used by every UI element.
///
/// Colours are semantic tokens, not raw values: `accent` means "user /
/// focus", `success` means "agent / approved", and so on.  This makes it
/// trivial to re-skin the whole application by changing a single struct.
pub struct Theme {
    /// Background (usually `Color::Reset` — the terminal's own background).
    pub bg: Color,
    /// Primary text colour.
    pub fg: Color,
    /// Secondary text (gray).
    pub fg_dim: Color,
    /// Most muted text — hints, placeholders, system messages.
    pub fg_muted: Color,
    /// Main accent — user, focus, prompt.  One accent colour for the whole app.
    pub accent: Color,
    /// Success — agent messages, approved commands.
    pub success: Color,
    /// Warning — thinking state, command headers.
    pub warning: Color,
    /// Danger — errors, destructive commands, deny.
    pub danger: Color,
    /// Background of "raised" elements (status bar, help bar).
    pub surface: Color,
    /// Background for mouse text selection (reserved for future use).
    pub selection_bg: Color,
}

impl Theme {
    /// The default dark theme — matches the pre-refactor appearance.
    ///
    /// Note: Interactive and PasswordInput modes previously used
    /// `Color::Magenta`; per the design philosophy ("one accent colour")
    /// they now use `accent` (Cyan).  This is the only visible change.
    pub fn default_dark() -> Self {
        Self {
            bg: Color::Reset,
            fg: Color::White,
            fg_dim: Color::Gray,
            fg_muted: Color::DarkGray,
            accent: Color::Cyan,
            success: Color::Green,
            warning: Color::Yellow,
            danger: Color::Red,
            surface: Color::DarkGray,
            selection_bg: Color::DarkGray,
        }
    }

    // ----- Style helpers ---------------------------------------------------

    /// Style for the user's chat header: accent + bold.
    pub fn user_style(&self) -> Style {
        Style::default().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    /// Style for the agent's chat header: success + bold.
    pub fn agent_style(&self) -> Style {
        Style::default()
            .fg(self.success)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for error headers: danger + bold.
    pub fn error_style(&self) -> Style {
        Style::default()
            .fg(self.danger)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for command headers: warning + bold.
    pub fn command_style(&self) -> Style {
        Style::default()
            .fg(self.warning)
            .add_modifier(Modifier::BOLD)
    }

    /// Plain danger colour (no bold) — e.g. denied status, warning body text.
    pub fn danger_fg(&self) -> Style {
        Style::default().fg(self.danger)
    }

    /// Plain success colour (no bold) — e.g. approved status.
    pub fn success_fg(&self) -> Style {
        Style::default().fg(self.success)
    }

    /// Plain warning colour (no bold) — e.g. command prefix `> `.
    pub fn warning_fg(&self) -> Style {
        Style::default().fg(self.warning)
    }

    /// Muted text — placeholders, system messages, borders.
    pub fn muted(&self) -> Style {
        Style::default().fg(self.fg_muted)
    }

    /// Dim text — secondary content (help bar).
    pub fn dim(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

    /// Primary foreground text.
    pub fn fg_style(&self) -> Style {
        Style::default().fg(self.fg)
    }

    /// Status-bar / help-bar surface: surface background + primary text.
    pub fn surface_style(&self) -> Style {
        Style::default().bg(self.surface).fg(self.fg)
    }

    /// Help-bar surface: surface background + dim text.
    pub fn help_bar_style(&self) -> Style {
        Style::default().bg(self.surface).fg(self.fg_dim)
    }

    /// Target-name badge: black text on accent background.
    pub fn target_badge_style(&self) -> Style {
        Style::default().fg(Color::Black).bg(self.accent)
    }

    /// Mode badge: black text on the given mode colour.
    pub fn mode_badge_style(&self, mode_color: Color) -> Style {
        Style::default().fg(Color::Black).bg(mode_color)
    }

    /// Map an [`AppMode`](crate::app::AppMode) to its semantic colour.
    pub fn mode_color(&self, mode: crate::app::AppMode) -> Color {
        use crate::app::AppMode;
        match mode {
            AppMode::Normal => self.success,
            AppMode::Thinking => self.warning,
            AppMode::Confirming => self.danger,
            AppMode::Interactive => self.accent,
            AppMode::PasswordInput => self.accent,
        }
    }

    /// Return the appropriate glyph set for this terminal.
    pub fn glyphs(&self) -> &'static Glyphs {
        Glyphs::detect()
    }

    /// Style for markdown code spans: `fg` on `surface` background.
    pub fn code_span_style(&self) -> Style {
        Style::default().fg(self.fg).bg(self.surface)
    }

    /// Style for markdown bold text: `fg` + bold.
    pub fn bold_style(&self) -> Style {
        Style::default().fg(self.fg).add_modifier(Modifier::BOLD)
    }

    /// Style for markdown headers: `accent` + bold.
    pub fn header_style(&self) -> Style {
        Style::default().fg(self.accent).add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppMode;

    #[test]
    fn default_dark_has_expected_colors() {
        let t = Theme::default_dark();
        assert_eq!(t.accent, Color::Cyan);
        assert_eq!(t.success, Color::Green);
        assert_eq!(t.warning, Color::Yellow);
        assert_eq!(t.danger, Color::Red);
        assert_eq!(t.fg, Color::White);
        assert_eq!(t.fg_muted, Color::DarkGray);
        assert_eq!(t.fg_dim, Color::Gray);
    }

    #[test]
    fn mode_color_mapping() {
        let t = Theme::default_dark();
        assert_eq!(t.mode_color(AppMode::Normal), Color::Green);
        assert_eq!(t.mode_color(AppMode::Thinking), Color::Yellow);
        assert_eq!(t.mode_color(AppMode::Confirming), Color::Red);
        // Interactive and PasswordInput both use accent (was Magenta).
        assert_eq!(t.mode_color(AppMode::Interactive), Color::Cyan);
        assert_eq!(t.mode_color(AppMode::PasswordInput), Color::Cyan);
    }

    #[test]
    fn style_helpers_use_correct_colors() {
        let t = Theme::default_dark();
        // user_style → accent + bold
        let s = t.user_style();
        assert_eq!(s.fg, Some(Color::Cyan));
        assert!(s.add_modifier.contains(Modifier::BOLD));
        // agent_style → success + bold
        let s = t.agent_style();
        assert_eq!(s.fg, Some(Color::Green));
        assert!(s.add_modifier.contains(Modifier::BOLD));
        // muted → fg_muted, no bold
        let s = t.muted();
        assert_eq!(s.fg, Some(Color::DarkGray));
        assert!(!s.add_modifier.contains(Modifier::BOLD));
    }
}
