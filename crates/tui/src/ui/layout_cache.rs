//! Cached chat layout for efficient rendering and mouse hit-testing.
//!
//! [`ChatLayoutCache`] stores pre-rendered lines so that `render_chat_history`
//! does not re-wrap text on every frame (up to 60 fps).  The cache is
//! invalidated when the terminal width changes, when messages are
//! added/removed, or when the last block's revision changes (for streaming
//! updates in task 7).
//!
//! Each [`RenderedLine`] carries a `block_index` back-pointer into
//! `app.messages` and a [`LineRegion`] tag, enabling future mouse
//! hit-testing ("which block was clicked?") without a separate scan.

use std::collections::HashSet;

use ratatui::text::{Line, Span};

use filar_core::ChatBlock;

use super::text::{strip_emoji, wrap_text};
use super::theme::Theme;

/// Which part of a [`ChatBlock`] a rendered line belongs to.
///
/// Used for hit-testing precision: a click on a `Header` might select the
/// whole block, while a click on `Output` could toggle output collapsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineRegion {
    /// Header line: `> You`, `* Agent`, `> Command [ok]`, etc.
    Header,
    /// Body text: wrapped content of a user / agent / error message.
    Body,
    /// Command output line (prefixed with `  | `).
    Output,
    /// The `... (N more lines)` truncation marker (future toggle target).
    OutputToggle,
    /// Empty spacer line between blocks.
    Spacer,
}

/// A single rendered line of chat, with metadata for hit-testing.
pub struct RenderedLine {
    /// The fully-styled line, ready for display.
    pub line: Line<'static>,
    /// Index into `app.messages`, or `None` for spacer lines.
    pub block_index: Option<usize>,
    /// Which region of the block this line represents.
    pub region: LineRegion,
}

/// Cached layout of the chat history.
///
/// The cache is rebuilt only when one of its invalidation keys changes
/// (width, message count, or message revision).  Between rebuilds,
/// `render_chat_history` simply slices `lines` by scroll offset — no
/// text wrapping or emoji stripping is repeated.
pub struct ChatLayoutCache {
    /// All rendered lines, in display order.
    pub lines: Vec<RenderedLine>,
    /// Width (inner, without borders) for which the cache was built.
    width: u16,
    /// `messages.len()` at build time.
    message_count: usize,
    /// `message_rev` at build time.
    last_block_rev: u64,
}

/// Maximum number of rendered lines to cache.
///
/// Raised from 500 (pre-cache) to 2000 — the cache makes per-frame cost
/// a simple slice, so a larger buffer is cheap and reduces the chance of
/// losing context when scrolling long conversations.
const MAX_CACHED_LINES: usize = 2000;

impl ChatLayoutCache {
    /// Create an empty cache (will always need a rebuild on first use).
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            width: 0,
            message_count: 0,
            last_block_rev: 0,
        }
    }

    /// Returns `true` if the cache must be rebuilt for the given parameters.
    pub fn needs_rebuild(&self, messages: &[ChatBlock], width: u16, rev: u64) -> bool {
        self.width != width
            || self.message_count != messages.len()
            || self.last_block_rev != rev
    }

    /// Rebuild the cache from scratch.
    ///
    /// `collapsed` contains indices of blocks whose output is collapsed,
    /// computed by `App::collapsed_set()` from user overrides and defaults.
    pub fn rebuild(
        &mut self,
        messages: &[ChatBlock],
        width: u16,
        theme: &Theme,
        collapsed: &HashSet<usize>,
        rev: u64,
    ) {
        self.lines.clear();

        // Compute wrapping widths — mirrors the pre-refactor chat.rs logic.
        let inner = width as usize;
        let content_width = inner.saturating_sub(2).max(1); // "  " prefix
        let output_width = inner.saturating_sub(4).max(1); // "  | " prefix

        for (idx, msg) in messages.iter().enumerate() {
            let _is_collapsed = collapsed.contains(&idx);
            match msg {
                ChatBlock::User(text) => {
                    let text = strip_emoji(text);
                    self.push_header(idx, vec![
                        Span::styled("> ", theme.user_style()),
                        Span::styled("You", theme.user_style()),
                    ]);
                    for line in text.lines() {
                        for wrapped in wrap_text(line, content_width) {
                            self.push_body(idx, format!("  {wrapped}"));
                        }
                    }
                    self.push_spacer();
                }

                ChatBlock::Agent(text) => {
                    let text = strip_emoji(text);
                    self.push_header(idx, vec![
                        Span::styled("* ", theme.agent_style()),
                        Span::styled("Agent", theme.agent_style()),
                    ]);
                    for line in text.lines() {
                        for wrapped in wrap_text(line, content_width) {
                            self.push_body(idx, format!("  {wrapped}"));
                        }
                    }
                    self.push_spacer();
                }

                ChatBlock::Command {
                    command,
                    explanation,
                    output,
                    approved,
                } => {
                    let is_collapsed = collapsed.contains(&idx);

                    // Status glyph: ✓ for approved, ✗ for denied.
                    let status = if *approved {
                        Span::styled("  ✓", theme.success_fg())
                    } else {
                        Span::styled("  ✗", theme.danger_fg())
                    };

                    // Arrow indicator: ▸ collapsed, ▾ expanded, spaces if no output.
                    let arrow = if output.is_some() {
                        if is_collapsed { "▸ " } else { "▾ " }
                    } else {
                        "  "
                    };

                    // Compact header: `▸ $ command  ✓` / `▾ $ command  ✓`
                    self.push_header(idx, vec![
                        Span::styled(arrow, theme.warning_fg()),
                        Span::styled("$ ", theme.warning_fg()),
                        Span::styled(command.clone(), theme.command_style()),
                        status,
                    ]);

                    // Explanation (if any) — shown in both states.
                    if !explanation.is_empty() {
                        for wrapped in wrap_text(&strip_emoji(explanation), output_width) {
                            self.push_output(idx, format!("  | {wrapped}"));
                        }
                    }

                    // Output rendering with collapse/expand.
                    if let Some(out) = output {
                        let all_lines: Vec<&str> = out.lines().collect();
                        let total = all_lines.len();

                        if is_collapsed {
                            // Collapsed: show first 5 lines + toggle.
                            for line in all_lines.iter().take(5) {
                                for wrapped in wrap_text(line, output_width) {
                                    self.push_output(idx, format!("  | {wrapped}"));
                                }
                            }
                            if total > 5 {
                                let remaining = total - 5;
                                self.push_output_toggle(
                                    idx,
                                    format!(
                                        "  ▸ … {} more lines — click to expand",
                                        remaining
                                    ),
                                );
                            }
                        } else {
                            // Expanded: show up to 400 lines (safety ceiling).
                            const MAX_EXPANDED_LINES: usize = 400;
                            for line in all_lines.iter().take(MAX_EXPANDED_LINES) {
                                for wrapped in wrap_text(line, output_width) {
                                    self.push_output(idx, format!("  | {wrapped}"));
                                }
                            }
                            if total > MAX_EXPANDED_LINES {
                                let remaining = total - MAX_EXPANDED_LINES;
                                self.push_output(
                                    idx,
                                    format!(
                                        "  … truncated ({} more lines)",
                                        remaining
                                    ),
                                );
                            }
                            // Collapse toggle for long output.
                            if total > 5 {
                                self.push_output_toggle(
                                    idx,
                                    "  ▾ collapse".to_string(),
                                );
                            }
                        }
                    }
                    self.push_spacer();
                }

                ChatBlock::Error(text) => {
                    let text = strip_emoji(text);
                    self.push_header(idx, vec![
                        Span::styled("! ", theme.error_style()),
                        Span::styled("Error", theme.error_style()),
                    ]);
                    for line in text.lines() {
                        for wrapped in wrap_text(line, content_width) {
                            self.push_body(idx, format!("  {wrapped}"));
                        }
                    }
                    self.push_spacer();
                }

                ChatBlock::System(text) => {
                    let text = strip_emoji(text);
                    self.push_header(idx, vec![
                        Span::styled("- ", theme.muted()),
                        Span::styled(text, theme.muted()),
                    ]);
                    self.push_spacer();
                }
            }
        }

        // Truncate to the last MAX_CACHED_LINES to bound memory.
        if self.lines.len() > MAX_CACHED_LINES {
            let start = self.lines.len() - MAX_CACHED_LINES;
            self.lines.drain(0..start);
        }

        self.width = width;
        self.message_count = messages.len();
        self.last_block_rev = rev;
    }

    // ----- Internal push helpers -------------------------------------------

    fn push_header(&mut self, idx: usize, spans: Vec<Span<'static>>) {
        self.lines.push(RenderedLine {
            line: Line::from(spans),
            block_index: Some(idx),
            region: LineRegion::Header,
        });
    }

    fn push_body(&mut self, idx: usize, text: String) {
        self.lines.push(RenderedLine {
            line: Line::from(text),
            block_index: Some(idx),
            region: LineRegion::Body,
        });
    }

    fn push_output(&mut self, idx: usize, text: String) {
        self.lines.push(RenderedLine {
            line: Line::from(text),
            block_index: Some(idx),
            region: LineRegion::Output,
        });
    }

    fn push_output_toggle(&mut self, idx: usize, text: String) {
        self.lines.push(RenderedLine {
            line: Line::from(text),
            block_index: Some(idx),
            region: LineRegion::OutputToggle,
        });
    }

    fn push_spacer(&mut self) {
        self.lines.push(RenderedLine {
            line: Line::from(""),
            block_index: None,
            region: LineRegion::Spacer,
        });
    }
}

impl Default for ChatLayoutCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_messages() -> Vec<ChatBlock> {
        vec![
            ChatBlock::User("Hello".into()),
            ChatBlock::Agent("Hi there".into()),
            ChatBlock::System("connected".into()),
        ]
    }

    #[test]
    fn cache_invalidates_on_width_change() {
        let theme = Theme::default_dark();
        let collapsed = HashSet::new();
        let mut cache = ChatLayoutCache::new();
        let msgs = sample_messages();

        // Initial build at width 80.
        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);
        assert!(!cache.needs_rebuild(&msgs, 80, 1));

        // Width changed → needs rebuild.
        assert!(cache.needs_rebuild(&msgs, 60, 1));
    }

    #[test]
    fn cache_invalidates_on_message_added() {
        let theme = Theme::default_dark();
        let collapsed = HashSet::new();
        let mut cache = ChatLayoutCache::new();
        let msgs = sample_messages();

        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);
        assert!(!cache.needs_rebuild(&msgs, 80, 1));

        // Message added → needs rebuild.
        let mut more = msgs.clone();
        more.push(ChatBlock::User("extra".into()));
        assert!(cache.needs_rebuild(&more, 80, 1));
    }

    #[test]
    fn cache_invalidates_on_rev_change() {
        let theme = Theme::default_dark();
        let collapsed = HashSet::new();
        let mut cache = ChatLayoutCache::new();
        let msgs = sample_messages();

        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);
        assert!(!cache.needs_rebuild(&msgs, 80, 1));

        // Same messages, same width, but rev changed (e.g. last block mutated).
        assert!(cache.needs_rebuild(&msgs, 80, 2));
    }

    #[test]
    fn cache_does_not_rebuild_on_same_params() {
        let theme = Theme::default_dark();
        let collapsed = HashSet::new();
        let mut cache = ChatLayoutCache::new();
        let msgs = sample_messages();

        // Build once.
        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);
        let line_count = cache.lines.len();
        assert!(line_count > 0);

        // Check again with identical parameters → should NOT need rebuild.
        assert!(!cache.needs_rebuild(&msgs, 80, 1));

        // Rebuild anyway to verify line count is stable.
        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);
        assert_eq!(cache.lines.len(), line_count);
    }

    #[test]
    fn rendered_lines_have_correct_regions() {
        let theme = Theme::default_dark();
        let collapsed = HashSet::new();
        let mut cache = ChatLayoutCache::new();

        let msgs = vec![ChatBlock::User("hello".into())];
        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);

        // Expected: Header, Body, Spacer (3 lines for a single-line user message).
        assert_eq!(cache.lines.len(), 3);
        assert_eq!(cache.lines[0].region, LineRegion::Header);
        assert_eq!(cache.lines[0].block_index, Some(0));
        assert_eq!(cache.lines[1].region, LineRegion::Body);
        assert_eq!(cache.lines[1].block_index, Some(0));
        assert_eq!(cache.lines[2].region, LineRegion::Spacer);
        assert_eq!(cache.lines[2].block_index, None);
    }

    #[test]
    fn command_block_expanded_shows_collapse_toggle() {
        let theme = Theme::default_dark();
        let collapsed = HashSet::new();
        let mut cache = ChatLayoutCache::new();

        let long_output = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let msgs = vec![ChatBlock::Command {
            command: "test".into(),
            explanation: "expl".into(),
            output: Some(long_output),
            approved: true,
        }];
        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);

        // Expanded: Header, Output (expl), Output x50, OutputToggle (▾ collapse), Spacer.
        let has_toggle = cache.lines.iter().any(|l| l.region == LineRegion::OutputToggle);
        assert!(has_toggle, "expected a ▾ collapse toggle for expanded long output");
        let has_output = cache.lines.iter().any(|l| l.region == LineRegion::Output);
        assert!(has_output, "expected Output lines");

        // The toggle text should say "collapse", not "more lines".
        let toggle = cache.lines.iter().find(|l| l.region == LineRegion::OutputToggle).unwrap();
        let toggle_text = format!("{}", toggle.line);
        assert!(toggle_text.contains("collapse"), "toggle should say collapse, got: {toggle_text}");
    }

    #[test]
    fn command_block_collapsed_shows_5_lines_and_expand_toggle() {
        let theme = Theme::default_dark();
        let mut collapsed = HashSet::new();
        collapsed.insert(0);
        let mut cache = ChatLayoutCache::new();

        let long_output = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let msgs = vec![ChatBlock::Command {
            command: "test".into(),
            explanation: "".into(),
            output: Some(long_output),
            approved: true,
        }];
        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);

        // Collapsed: Header, Output x5, OutputToggle (▸ … 45 more lines), Spacer.
        let output_count = cache.lines.iter().filter(|l| l.region == LineRegion::Output).count();
        assert_eq!(output_count, 5, "collapsed block should show exactly 5 output lines");

        let toggle = cache.lines.iter().find(|l| l.region == LineRegion::OutputToggle);
        assert!(toggle.is_some(), "collapsed block should have a toggle line");
        let toggle_text = format!("{}", toggle.unwrap().line);
        assert!(toggle_text.contains("more lines"), "toggle should mention more lines, got: {toggle_text}");
        assert!(toggle_text.contains("45"), "toggle should say 45 more lines, got: {toggle_text}");
    }

    #[test]
    fn command_block_short_output_has_no_toggle() {
        let theme = Theme::default_dark();
        let collapsed = HashSet::new();
        let mut cache = ChatLayoutCache::new();

        let short_output = "line 1\nline 2\nline 3";
        let msgs = vec![ChatBlock::Command {
            command: "echo".into(),
            explanation: "".into(),
            output: Some(short_output.into()),
            approved: true,
        }];
        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);

        // Short output: Header, Output x3, Spacer — no toggle.
        let has_toggle = cache.lines.iter().any(|l| l.region == LineRegion::OutputToggle);
        assert!(!has_toggle, "short output should not have a toggle line");
    }

    #[test]
    fn command_block_header_has_arrow_and_status() {
        let theme = Theme::default_dark();
        let collapsed = HashSet::new();
        let mut cache = ChatLayoutCache::new();

        let msgs = vec![
            ChatBlock::Command {
                command: "ls".into(),
                explanation: "".into(),
                output: Some("file1\nfile2".into()),
                approved: true,
            },
            ChatBlock::Command {
                command: "rm".into(),
                explanation: "".into(),
                output: Some("".into()),
                approved: false,
            },
        ];
        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);

        // First header: expanded arrow (▾), $ ls, ✓.
        let header0 = &cache.lines[0];
        assert_eq!(header0.region, LineRegion::Header);
        let header0_text = format!("{}", header0.line);
        assert!(header0_text.contains('▾'), "expanded header should have ▾, got: {header0_text}");
        assert!(header0_text.contains("ls"), "header should contain command, got: {header0_text}");
        assert!(header0_text.contains('✓'), "approved header should have ✓, got: {header0_text}");

        // Find the second header (skip first block's lines + spacer).
        let header1 = cache.lines.iter().find(|l| {
            l.region == LineRegion::Header && l.block_index == Some(1)
        }).unwrap();
        let header1_text = format!("{}", header1.line);
        assert!(header1_text.contains('✗'), "denied header should have ✗, got: {header1_text}");
        assert!(header1_text.contains("rm"), "header should contain command, got: {header1_text}");
    }

    #[test]
    fn command_block_no_output_has_no_arrow() {
        let theme = Theme::default_dark();
        let collapsed = HashSet::new();
        let mut cache = ChatLayoutCache::new();

        let msgs = vec![ChatBlock::Command {
            command: "pending".into(),
            explanation: "".into(),
            output: None,
            approved: false,
        }];
        cache.rebuild(&msgs, 80, &theme, &collapsed, 1);

        let header = &cache.lines[0];
        let header_text = format!("{}", header.line);
        assert!(!header_text.contains('▸') && !header_text.contains('▾'),
            "no-output block should not have arrow, got: {header_text}");
    }
}
