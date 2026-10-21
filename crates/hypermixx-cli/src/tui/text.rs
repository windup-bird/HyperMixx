//! Display-width helpers shared by the TUI panels.
//!
//! Terminals measure columns, not chars: a Chinese title takes two cells per character. Everything
//! that clips a title or pads a column goes through here so the two never disagree.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Terminal cells `text` occupies.
pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Clips `text` to at most `max` cells, appending `…` when something was dropped.
///
/// The ellipsis itself is counted, so the result never exceeds `max` (a `max` of 0 is empty).
pub fn truncate_to_width(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(text) <= max {
        return text.to_owned();
    }
    let ellipsis = '…';
    let ellipsis_width = UnicodeWidthChar::width(ellipsis).unwrap_or(1);
    if max <= ellipsis_width {
        return ellipsis.to_string();
    }
    let budget = max - ellipsis_width;
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + char_width > budget {
            break;
        }
        out.push(ch);
        used += char_width;
    }
    out.push(ellipsis);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_measured_and_clipped_by_columns() {
        assert_eq!(width("deck0"), 5);
        assert_eq!(truncate_to_width("abcdef", 4), "abc…");
        assert_eq!(truncate_to_width("abc", 4), "abc");
    }

    #[test]
    fn cjk_counts_two_columns() {
        assert_eq!(width("测试"), 4);
        // One han character plus the ellipsis is four cells.
        assert_eq!(truncate_to_width("测试曲目", 4), "测…");
        assert_eq!(truncate_to_width("测试曲目", 5), "测试…");
    }

    #[test]
    fn zero_budget_is_empty() {
        assert_eq!(truncate_to_width("abc", 0), "");
    }
}
