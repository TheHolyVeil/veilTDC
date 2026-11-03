//! Word wrapping for toast and panel text. `veil-text` measures one line at a
//! time, so wrapping lives here. Pure: the measurer is passed in, which makes it
//! testable without fonts.
//!
//! Rules: words break at spaces, runs of spaces collapse to one, `\n` is a hard
//! break (two or more in a row make one blank line), a word wider than the line
//! is broken between characters, and when the text needs more than `max_lines`
//! the last line ends in an ellipsis that still fits.

use crate::markup::Span;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// A piece of a line with one style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub style: Style,
}

pub type Line = Vec<Run>;

/// Width of `text` drawn bold (second argument true) or regular.
pub type Measure<'a> = &'a mut dyn FnMut(&str, bool) -> f32;

enum Tok {
    Word(String, Style),
    Space,
    Newline,
}

fn tokens(spans: &[Span]) -> Vec<Tok> {
    let mut out = Vec::new();
    for s in spans {
        let style = Style { bold: s.bold, italic: s.italic, underline: s.underline };
        let mut word = String::new();
        let flush = |word: &mut String, out: &mut Vec<Tok>| {
            if !word.is_empty() {
                out.push(Tok::Word(std::mem::take(word), style));
            }
        };
        for c in s.text.chars() {
            match c {
                '\n' => {
                    flush(&mut word, &mut out);
                    out.push(Tok::Newline);
                }
                c if c.is_whitespace() => {
                    flush(&mut word, &mut out);
                    out.push(Tok::Space);
                }
                c => word.push(c),
            }
        }
        flush(&mut word, &mut out);
    }
    out
}

struct Wrapper<'a> {
    max: f32,
    measure: Measure<'a>,
    lines: Vec<Line>,
    cur: Line,
    cur_w: f32,
    space_w: f32,
    pending_space: bool,
}

impl Wrapper<'_> {
    fn push(&mut self, text: &str, style: Style) {
        self.cur_w += (self.measure)(text, style.bold);
        match self.cur.last_mut() {
            Some(r) if r.style == style => r.text.push_str(text),
            _ => self.cur.push(Run { text: text.to_string(), style }),
        }
    }

    fn end_line(&mut self) {
        self.lines.push(std::mem::take(&mut self.cur));
        self.cur_w = 0.0;
        self.pending_space = false;
    }

    /// Place `word` on an empty line, breaking it between characters if needed.
    fn place_on_empty_line(&mut self, word: &str, style: Style) {
        if (self.measure)(word, style.bold) <= self.max {
            self.push(word, style);
            return;
        }
        let mut piece = String::new();
        for c in word.chars() {
            piece.push(c);
            if (self.measure)(&piece, style.bold) > self.max && piece.chars().count() > 1 {
                piece.pop();
                self.push(&piece, style);
                self.end_line();
                piece.clear();
                piece.push(c);
            }
        }
        if !piece.is_empty() {
            self.push(&piece, style);
        }
    }

    fn word(&mut self, word: &str, style: Style) {
        let ww = (self.measure)(word, style.bold);
        let sw = if self.pending_space && !self.cur.is_empty() { self.space_w } else { 0.0 };
        if self.cur.is_empty() {
            self.place_on_empty_line(word, style);
        } else if self.cur_w + sw + ww <= self.max {
            if sw > 0.0 {
                self.push(" ", Style { bold: false, italic: false, underline: false });
            }
            self.push(word, style);
        } else {
            self.end_line();
            self.place_on_empty_line(word, style);
        }
        self.pending_space = false;
    }
}

fn line_width(line: &Line, measure: Measure<'_>) -> f32 {
    line.iter().map(|r| measure(&r.text, r.style.bold)).sum()
}

/// Wrap `spans` to `max_width`, at most `max_lines` lines (at least 1).
pub fn wrap_spans(spans: &[Span], max_width: f32, max_lines: usize, measure: Measure<'_>) -> Vec<Line> {
    let max_lines = max_lines.max(1);
    let space_w = measure(" ", false);
    let mut w = Wrapper {
        max: max_width.max(1.0),
        measure,
        lines: Vec::new(),
        cur: Vec::new(),
        cur_w: 0.0,
        space_w,
        pending_space: false,
    };
    for tok in tokens(spans) {
        match tok {
            Tok::Space => w.pending_space = !w.cur.is_empty(),
            Tok::Newline => {
                // Two or more newlines in a row make a single blank line.
                let blank_already = w.cur.is_empty() && w.lines.last().is_some_and(|l| l.is_empty());
                if !blank_already {
                    w.end_line();
                }
                w.pending_space = false;
            }
            Tok::Word(text, style) => w.word(&text, style),
        }
    }
    if !w.cur.is_empty() {
        w.end_line();
    }
    let max = w.max;
    let mut lines = w.lines;
    let measure = w.measure;

    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            ellipsize_line(last, max, measure);
        }
    }
    lines
}

/// Drop characters from the end of `line` until `…` fits, then append it.
fn ellipsize_line(line: &mut Line, max: f32, measure: Measure<'_>) {
    let style = line.last().map(|r| r.style).unwrap_or_default();
    let dots = measure("\u{2026}", style.bold);
    while line_width(line, &mut *measure) + dots > max {
        let Some(last) = line.last_mut() else { break };
        if last.text.pop().is_none() || last.text.is_empty() {
            line.pop();
        }
    }
    match line.last_mut() {
        Some(r) if r.style == style => r.text.push('\u{2026}'),
        _ => line.push(Run { text: "\u{2026}".into(), style }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markup;

    /// 10 px per character, bold 12 px, so styles are measurable.
    fn m(text: &str, bold: bool) -> f32 {
        text.chars().count() as f32 * if bold { 12.0 } else { 10.0 }
    }

    fn wrap(input: &str, width: f32, lines: usize) -> Vec<String> {
        let spans = markup::parse(input);
        wrap_spans(&spans, width, lines, &mut m)
            .iter()
            .map(|l| l.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect()
    }

    #[test]
    fn short_text_is_one_line() {
        assert_eq!(wrap("hello world", 200.0, 5), vec!["hello world"]);
        assert!(wrap("", 200.0, 5).is_empty());
        assert!(wrap("   \n  ", 200.0, 5).is_empty());
    }

    #[test]
    fn wraps_at_spaces_and_collapses_runs_of_them() {
        assert_eq!(wrap("aaa bbb ccc", 70.0, 5), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("aaa     bbb", 200.0, 5), vec!["aaa bbb"]);
        assert_eq!(wrap("aaa bbb ccc ddd", 40.0, 5), vec!["aaa", "bbb", "ccc", "ddd"]);
    }

    #[test]
    fn long_words_break_between_characters() {
        assert_eq!(wrap("abcdefghij", 40.0, 5), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("ab abcdefgh", 50.0, 5), vec!["ab", "abcde", "fgh"]);
        // a single character wider than the line still gets its own line
        assert_eq!(wrap("abc", 5.0, 9), vec!["a", "b", "c"]);
    }

    #[test]
    fn newlines_break_and_blank_lines_collapse() {
        assert_eq!(wrap("a\nb", 200.0, 5), vec!["a", "b"]);
        assert_eq!(wrap("a\n\n\n\nb", 200.0, 5), vec!["a", "", "b"]);
        assert_eq!(wrap("\n\na\n\n", 200.0, 5), vec!["a"], "no blank edges");
    }

    #[test]
    fn styles_survive_wrapping() {
        let spans = markup::parse("<b>aa bb</b> cc");
        // bold is 12 px a character: "aa bb" is 58 px, "cc" does not fit after it
        let lines = wrap_spans(&spans, 60.0, 5, &mut m);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].iter().any(|r| r.text.contains("aa") && r.style.bold));
        assert_eq!(lines[1][0].text, "cc");
        assert!(!lines[1][0].style.bold);
    }

    #[test]
    fn too_many_lines_end_in_an_ellipsis_that_fits() {
        let out = wrap("aaa bbb ccc ddd eee", 40.0, 2);
        assert_eq!(out, vec!["aaa", "bbb\u{2026}"], "\"bbb…\" is exactly 40 px, so nothing more is cut");
        assert_eq!(wrap("aaa bbb ccc", 30.0, 2), vec!["aaa", "bb\u{2026}"], "here it has to shrink to fit");
        for line in &out {
            assert!(m(line, false) <= 40.0, "{line:?} does not fit");
        }
        assert_eq!(wrap("aaa bbb", 40.0, 2), vec!["aaa", "bbb"], "fits exactly: no ellipsis");
    }

    #[test]
    fn lines_always_fit_and_no_text_is_lost() {
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let alphabet: Vec<char> = "ab cd\nefgh <b>ij</b> klmnopqrstuvwxyz  ".chars().collect();
        for _ in 0..2000 {
            let input: String = (0..next(80)).map(|_| alphabet[next(alphabet.len() as u64) as usize]).collect();
            let width = 20.0 + next(180) as f32;
            let spans = markup::parse(&input);
            let lines = wrap_spans(&spans, width, 1000, &mut m);
            for l in &lines {
                let w: f32 = l.iter().map(|r| m(&r.text, r.style.bold)).sum();
                assert!(w <= width + 0.01 || l.iter().map(|r| r.text.chars().count()).sum::<usize>() == 1, "{l:?} wider than {width}");
            }
            let before: String = markup::plain(&spans).chars().filter(|c| !c.is_whitespace()).collect();
            let after: String = lines.iter().flatten().flat_map(|r| r.text.chars()).filter(|c| !c.is_whitespace()).collect();
            assert_eq!(before, after, "text changed for {input:?} at {width}");
            let capped = wrap_spans(&spans, width, 2, &mut m);
            assert!(capped.len() <= 2);
        }
    }
}
