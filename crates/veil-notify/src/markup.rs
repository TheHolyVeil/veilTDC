//! Notification body markup: the subset `<b> <i> <u>`, nothing else.
//!
//! One parser, used both to sanitise what apps send and to read the stored
//! canonical form back at draw time. Everything else is stripped: unknown tags
//! lose their tags but keep their text, `<br>` becomes a newline, entities
//! are decoded to text, and text is never re-parsed after decoding, so
//! `&lt;b&gt;` stays literal. A `<` that does not start a well-formed tag is
//! literal text. Known limit: `x<y and z>w` reads as a tag and loses `<y and z>`.

use crate::text::{is_banned, truncate_boundary};

const MAX_TAG_LEN: usize = 256;
const MAX_ENTITY_LEN: usize = 10;
const MAX_DEPTH: u32 = 32;

/// A run of text with one style.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Clone, Copy)]
enum Style {
    B,
    I,
    U,
}

enum Tag {
    Open(Style),
    Close(Style),
    Break,
    Other,
}

#[derive(Default)]
struct Builder {
    spans: Vec<Span>,
    bold: u32,
    italic: u32,
    underline: u32,
}

impl Builder {
    fn push_char(&mut self, c: char) {
        let c = match c {
            '\t' => ' ',
            '\r' => return,
            c => c,
        };
        if is_banned(c) {
            return;
        }
        let (bold, italic, underline) = (self.bold > 0, self.italic > 0, self.underline > 0);
        match self.spans.last_mut() {
            Some(s) if s.bold == bold && s.italic == italic && s.underline == underline => s.text.push(c),
            _ => self.spans.push(Span { text: c.to_string(), bold, italic, underline }),
        }
    }

    fn push_str(&mut self, s: &str) {
        for c in s.chars() {
            self.push_char(c);
        }
    }

    fn depth(&mut self, s: Style) -> &mut u32 {
        match s {
            Style::B => &mut self.bold,
            Style::I => &mut self.italic,
            Style::U => &mut self.underline,
        }
    }

    fn tag(&mut self, t: Tag) {
        match t {
            Tag::Open(s) => {
                let d = self.depth(s);
                *d = (*d + 1).min(MAX_DEPTH);
            }
            Tag::Close(s) => {
                let d = self.depth(s);
                *d = d.saturating_sub(1);
            }
            Tag::Break => self.push_char('\n'),
            Tag::Other => {}
        }
    }
}

/// A complete tag starting at `pos` (which holds `<`): its end offset and kind.
fn scan_tag(input: &str, pos: usize) -> Option<(usize, Tag)> {
    let rest = &input.as_bytes()[pos + 1..];
    let window = &rest[..rest.len().min(MAX_TAG_LEN)];
    let close = window.iter().position(|&c| c == b'>' || c == b'<')?;
    if window[close] != b'>' {
        return None;
    }
    // `pos` and `pos + 1 + close` both hold ASCII bytes, so these are char boundaries.
    let inner = &input[pos + 1..pos + 1 + close];
    let (closing, named) = match inner.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, inner),
    };
    if !named.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    let name_end = named.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(named.len());
    let name = &named[..name_end];
    let bare = name_end == named.len();
    let tag = if !closing && name.eq_ignore_ascii_case("br") {
        Tag::Break
    } else if !bare {
        Tag::Other
    } else {
        let style = if name.eq_ignore_ascii_case("b") {
            Some(Style::B)
        } else if name.eq_ignore_ascii_case("i") {
            Some(Style::I)
        } else if name.eq_ignore_ascii_case("u") {
            Some(Style::U)
        } else {
            None
        };
        match style {
            Some(s) if closing => Tag::Close(s),
            Some(s) => Tag::Open(s),
            None => Tag::Other,
        }
    };
    Some((pos + 1 + close + 1, tag))
}

/// A complete entity starting at `pos` (which holds `&`): end offset and char.
fn scan_entity(input: &str, pos: usize) -> Option<(usize, char)> {
    let rest = &input.as_bytes()[pos + 1..];
    let window = &rest[..rest.len().min(MAX_ENTITY_LEN)];
    let semi = window.iter().position(|&c| c == b';')?;
    let body = &input[pos + 1..pos + 1 + semi];
    let ch = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let num = body.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            let c = char::from_u32(code)?;
            if c == '\0' {
                return None;
            }
            c
        }
    };
    Some((pos + 1 + semi + 1, ch))
}

/// Parse any input (raw from an app, or our own canonical form) into styled spans.
pub fn parse(input: &str) -> Vec<Span> {
    let bytes = input.as_bytes();
    let mut b = Builder::default();
    let (mut pos, mut lit) = (0, 0);
    while pos < bytes.len() {
        match bytes[pos] {
            b'<' => {
                if let Some((end, tag)) = scan_tag(input, pos) {
                    b.push_str(&input[lit..pos]);
                    b.tag(tag);
                    pos = end;
                    lit = end;
                    continue;
                }
            }
            b'&' => {
                if let Some((end, ch)) = scan_entity(input, pos) {
                    b.push_str(&input[lit..pos]);
                    b.push_char(ch);
                    pos = end;
                    lit = end;
                    continue;
                }
            }
            _ => {}
        }
        pos += 1;
    }
    b.push_str(&input[lit..]);
    b.spans
}

fn escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

/// Serialise spans to the canonical form: only `<b> <i> <u>`, always balanced,
/// text escaped. `parse(render(s)) == s` for any spans `parse` produced.
pub fn render(spans: &[Span]) -> String {
    let mut out = String::new();
    for s in spans {
        if s.bold {
            out.push_str("<b>");
        }
        if s.italic {
            out.push_str("<i>");
        }
        if s.underline {
            out.push_str("<u>");
        }
        escape_into(&s.text, &mut out);
        if s.underline {
            out.push_str("</u>");
        }
        if s.italic {
            out.push_str("</i>");
        }
        if s.bold {
            out.push_str("</b>");
        }
    }
    out
}

/// The text without any styling.
pub fn plain(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn trim_end(spans: &mut Vec<Span>) {
    while let Some(last) = spans.last_mut() {
        let t = last.text.trim_end().len();
        last.text.truncate(t);
        if last.text.is_empty() {
            spans.pop();
        } else {
            break;
        }
    }
}

/// Sanitise an app's body into canonical markup. The input is cut at `max`
/// bytes first (never inside a half tag or entity) and an ellipsis marks the cut.
/// The result can be a little longer than `max` because of tags and escaping.
pub fn sanitize(input: &str, max: usize) -> String {
    let (mut cut, truncated) = truncate_boundary(input, max);
    if truncated {
        if let Some(i) = cut.rfind('<') {
            if !cut[i..].contains('>') {
                cut = &cut[..i];
            }
        }
        if let Some(i) = cut.rfind('&') {
            if !cut[i..].contains(';') {
                cut = &cut[..i];
            }
        }
    }
    let mut spans = parse(cut);
    trim_end(&mut spans);
    if truncated {
        match spans.last_mut() {
            Some(s) => s.text.push('\u{2026}'),
            None => spans.push(Span { text: "\u{2026}".into(), ..Span::default() }),
        }
    }
    render(&spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(text: &str, b: bool, i: bool, u: bool) -> Span {
        Span { text: text.into(), bold: b, italic: i, underline: u }
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(parse("hello world"), vec![sp("hello world", false, false, false)]);
        assert_eq!(parse(""), vec![]);
    }

    #[test]
    fn styles_apply_and_nest() {
        assert_eq!(
            parse("<b>a</b> <i>b</i><u>c</u>"),
            vec![
                sp("a", true, false, false),
                sp(" ", false, false, false),
                sp("b", false, true, false),
                sp("c", false, false, true)
            ]
        );
        assert_eq!(
            parse("<B>a<i>b</I>c</b>"),
            vec![sp("a", true, false, false), sp("b", true, true, false), sp("c", true, false, false)]
        );
    }

    #[test]
    fn unclosed_and_stray_tags_are_harmless() {
        assert_eq!(parse("<b>a<i>b"), vec![sp("a", true, false, false), sp("b", true, true, false)]);
        assert_eq!(parse("</b>x</u>"), vec![sp("x", false, false, false)]);
        assert_eq!(render(&parse("<b>a<i>b")), "<b>a</b><b><i>b</i></b>");
    }

    #[test]
    fn unknown_tags_are_stripped_but_text_stays() {
        let s = plain(&parse(r#"<a href="http://x">link</a><img src="y"><script>alert(1)</script><b class="z">k</b>"#));
        assert_eq!(s, "linkalert(1)k");
        // attributes on a style tag mean it is not the plain tag, so no style
        assert_eq!(parse(r#"<b class="z">k</b>"#), vec![sp("k", false, false, false)]);
    }

    #[test]
    fn lone_angle_brackets_are_literal() {
        assert_eq!(plain(&parse("1 < 2 and 3 > 2")), "1 < 2 and 3 > 2");
        assert_eq!(plain(&parse("a <")), "a <");
        assert_eq!(plain(&parse("<<b>x")), "<x");
        assert_eq!(plain(&parse("x<3>")), "x");
    }

    #[test]
    fn entities_decode_once_and_never_become_tags() {
        assert_eq!(plain(&parse("&amp;&lt;&gt;&quot;&apos;&#65;&#x42;&bogus;&#0;&")), "&<>\"'AB&bogus;&#0;&");
        let spans = parse("&lt;b&gt;x&lt;/b&gt;");
        assert_eq!(plain(&spans), "<b>x</b>");
        assert!(!spans[0].bold);
        assert_eq!(render(&spans), "&lt;b&gt;x&lt;/b&gt;");
    }

    #[test]
    fn br_is_a_newline() {
        assert_eq!(plain(&parse("a<br>b<br/>c<BR />d</br>e")), "a\nb\nc\nde");
    }

    #[test]
    fn control_and_bidi_characters_are_dropped() {
        assert_eq!(plain(&parse("a\u{202E}b\u{0}c\rd\te")), "abcd e");
        assert_eq!(plain(&parse("&#8238;x&#7;")), "x");
    }

    #[test]
    fn sanitize_truncates_cleanly() {
        let out = sanitize("<b>0123456789</b>", 8);
        assert_eq!(out, "<b>01234\u{2026}</b>", "cut inside the text, tags rebalanced");
        // a cut that lands inside a tag or entity drops the fragment
        assert_eq!(sanitize("abcd<b", 6), "abcd\u{2026}");
        assert_eq!(sanitize("abcd&am", 7), "abcd\u{2026}");
        assert_eq!(sanitize("ééé", 3), "é\u{2026}");
        assert_eq!(sanitize("short", 100), "short");
        assert_eq!(sanitize("trailing  \n", 100), "trailing");
    }

    #[test]
    fn output_is_idempotent_and_contains_only_our_tags() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let alphabet: Vec<char> = "<>&/;#xbiuBIU a\"'é\n=\u{202E}\u{0}!".chars().collect();
        for _ in 0..5000 {
            let len = (next(&mut seed) % 40) as usize;
            let input: String = (0..len).map(|_| alphabet[(next(&mut seed) as usize) % alphabet.len()]).collect();
            let once = sanitize(&input, 4096);
            assert_eq!(sanitize(&once, 4096), once, "not idempotent for {input:?}");
            let mut stripped = once.clone();
            for t in ["<b>", "</b>", "<i>", "</i>", "<u>", "</u>"] {
                stripped = stripped.replace(t, "");
            }
            assert!(!stripped.contains('<'), "stray tag in {once:?} from {input:?}");
        }
    }

    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }
}
