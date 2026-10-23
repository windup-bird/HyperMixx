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

/// Pads `text` to exactly `max` cells, truncating with `…` when necessary.
pub fn pad_to_width(text: &str, max: usize) -> String {
    let mut out = truncate_to_width(text, max);
    let used = width(&out);
    if used < max {
        out.push_str(&" ".repeat(max - used));
    }
    out
}

/// Word-aware wrapping on terminal cells. A word longer than `max` is hard-broken; CJK text has no
/// spaces and therefore breaks by character width. An empty input produces one empty line.
pub fn wrap_to_width(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    for word in text.split_whitespace() {
        let word_width = width(word);
        if used == 0 {
            if word_width <= max {
                line.push_str(word);
                used = word_width;
            } else {
                hard_wrap(word, max, &mut lines, &mut line, &mut used);
            }
        } else if used + 1 + word_width <= max {
            line.push(' ');
            line.push_str(word);
            used += 1 + word_width;
        } else {
            lines.push(std::mem::take(&mut line));
            used = 0;
            if word_width <= max {
                line.push_str(word);
                used = word_width;
            } else {
                hard_wrap(word, max, &mut lines, &mut line, &mut used);
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn hard_wrap(word: &str, max: usize, lines: &mut Vec<String>, line: &mut String, used: &mut usize) {
    for ch in word.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if *used > 0 && *used + char_width > max {
            lines.push(std::mem::take(line));
            *used = 0;
        }
        line.push(ch);
        *used += char_width;
    }
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

    #[test]
    fn padding_counts_cells() {
        assert_eq!(pad_to_width("abc", 5), "abc  ");
        assert_eq!(pad_to_width("测试", 5), "测试 ");
        assert_eq!(pad_to_width("abcdef", 4), "abc…");
    }

    #[test]
    fn wrapping_respects_words_and_cells() {
        assert_eq!(wrap_to_width("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap_to_width("测试曲目", 4), vec!["测试", "曲目"]);
        assert_eq!(wrap_to_width("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap_to_width("", 5), vec![""]);
    }
}
