//! Plain-text cleaning shared by the summary, the body and the history file.

/// Characters that must never reach the screen: control characters (a newline
/// is allowed through, callers decide what to do with it) and the Unicode
/// bidirectional overrides and marks, which can be used to spoof text order.
pub fn is_banned(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}'
    ) || (c.is_control() && c != '\n')
}

/// Cut `s` to at most `max` bytes on a char boundary. Returns the slice and
/// whether anything was cut.
pub fn truncate_boundary(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// Single-line plain text: newlines and tabs become spaces, banned characters
/// are dropped, runs of spaces collapse, the ends are trimmed. Cut at `max`
/// bytes with an ellipsis. Used for summaries, app names and action labels.
pub fn clean_line(s: &str, max: usize) -> String {
    let (cut, truncated) = truncate_boundary(s, max);
    let mut out = String::with_capacity(cut.len() + 3);
    for c in cut.chars() {
        let c = if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c };
        if is_banned(c) {
            continue;
        }
        if c == ' ' && (out.is_empty() || out.ends_with(' ')) {
            continue;
        }
        out.push(c);
    }
    while out.ends_with(' ') {
        out.pop();
    }
    if truncated {
        out.push('\u{2026}');
    }
    out
}

/// A path or URI hint (`image-path`, `app_icon`): trimmed, kept as given, or
/// `None` when empty, too long or containing control characters.
pub fn clean_path(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s.len() > 1024 || s.chars().any(char::is_control) {
        return None;
    }
    Some(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_collapses_and_strips() {
        assert_eq!(clean_line("  a \n\t b\r\nc  ", 100), "a b c");
        assert_eq!(clean_line("a\u{0}b\u{202E}c", 100), "abc");
    }

    #[test]
    fn line_truncates_on_a_char_boundary() {
        // 'é' is two bytes: a cut at 3 must land on 2.
        assert_eq!(clean_line("ééé", 3), "é\u{2026}");
        assert_eq!(clean_line("abcdef", 3), "abc\u{2026}");
        assert_eq!(clean_line("abc", 3), "abc");
    }

    #[test]
    fn line_keeps_markup_literal() {
        // Summaries are plain text, so tags show as typed.
        assert_eq!(clean_line("<b>x</b>", 100), "<b>x</b>");
    }

    #[test]
    fn path_rules() {
        assert_eq!(clean_path("  /usr/share/a.png "), Some("/usr/share/a.png".into()));
        assert_eq!(clean_path(""), None);
        assert_eq!(clean_path("a\nb"), None);
        assert_eq!(clean_path(&"x".repeat(2000)), None);
    }
}
