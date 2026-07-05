//! Text utility helpers — emoji stripping and line wrapping.
//!
//! Moved verbatim from the original `ui.rs`; no behaviour changes.

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
}
