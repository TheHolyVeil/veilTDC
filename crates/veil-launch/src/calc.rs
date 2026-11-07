//! The launcher's calculator: type `2*(3+4)` or `sqrt(2)^2` and the answer
//! shows as the first row.
//!
//! A small recursive-descent parser. Precedence, loosest to tightest:
//! `+ -`, then `* / %`, then unary `-`/`+`, then `^` (right-associative).
//! So `-2^2` is -4, `2^-2` is 0.25, `2*-3` is -6 and `2^3^2` is 512.
//! Division by zero and any non-finite result give no answer instead of
//! `inf`. Input is length- and depth-limited, since it is typed live.

const MAX_LEN: usize = 200;
const MAX_DEPTH: usize = 48;

/// The text to show for `query`, or `None` when it is not a calculation.
/// A bare number or constant (`42`, `pi`) is not offered: it is just a query.
pub fn answer(query: &str) -> Option<String> {
    let q = query.trim();
    if q.is_empty() || q.len() > MAX_LEN || !q.chars().any(|c| c.is_ascii_digit() || c.is_ascii_alphabetic()) {
        return None;
    }
    let mut p = Parser { toks: tokenize(q)?, pos: 0, depth: 0, composite: false };
    let v = p.expr()?;
    if p.pos != p.toks.len() || !p.composite || !v.is_finite() {
        return None;
    }
    Some(format(v))
}

/// Evaluate without the "is it worth showing" filter.
pub fn evaluate(expr: &str) -> Option<f64> {
    let e = expr.trim();
    if e.is_empty() || e.len() > MAX_LEN {
        return None;
    }
    let mut p = Parser { toks: tokenize(e)?, pos: 0, depth: 0, composite: false };
    let v = p.expr()?;
    (p.pos == p.toks.len() && v.is_finite()).then_some(v)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char),
    Open,
    Close,
}

fn tokenize(s: &str) -> Option<Vec<Tok>> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            ' ' | '\t' => i += 1,
            '+' | '-' | '*' | '/' | '%' | '^' => {
                out.push(Tok::Op(c[i]));
                i += 1;
            }
            '×' | '·' => {
                out.push(Tok::Op('*'));
                i += 1;
            }
            '÷' => {
                out.push(Tok::Op('/'));
                i += 1;
            }
            '(' => {
                out.push(Tok::Open);
                i += 1;
            }
            ')' => {
                out.push(Tok::Close);
                i += 1;
            }
            '0'..='9' | '.' => {
                let start = i;
                while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.' || c[i] == '_') {
                    i += 1;
                }
                // exponent: 1e3, 2.5E-4 (only when digits follow, so `2e` stays an error)
                if i < c.len() && (c[i] == 'e' || c[i] == 'E') {
                    let mut j = i + 1;
                    if j < c.len() && (c[j] == '+' || c[j] == '-') {
                        j += 1;
                    }
                    if j < c.len() && c[j].is_ascii_digit() {
                        while j < c.len() && c[j].is_ascii_digit() {
                            j += 1;
                        }
                        i = j;
                    }
                }
                let text: String = c[start..i].iter().filter(|&&ch| ch != '_').collect();
                out.push(Tok::Num(text.parse().ok()?));
            }
            'a'..='z' | 'A'..='Z' => {
                let start = i;
                while i < c.len() && (c[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                out.push(Tok::Ident(c[start..i].iter().collect::<String>().to_lowercase()));
            }
            'π' => {
                out.push(Tok::Ident("pi".into()));
                i += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
    /// Saw an operator, a function or parentheses: worth showing as a sum.
    composite: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn eat_op(&mut self, ops: &[char]) -> Option<char> {
        if let Some(Tok::Op(c)) = self.peek() {
            if ops.contains(c) {
                let c = *c;
                self.pos += 1;
                self.composite = true;
                return Some(c);
            }
        }
        None
    }

    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(op) = self.eat_op(&['+', '-']) {
            let r = self.term()?;
            v = if op == '+' { v + r } else { v - r };
        }
        Some(v)
    }

    fn term(&mut self) -> Option<f64> {
        let mut v = self.unary()?;
        while let Some(op) = self.eat_op(&['*', '/', '%']) {
            let r = self.unary()?;
            v = match op {
                '*' => v * r,
                '/' if r == 0.0 => return None,
                '/' => v / r,
                _ if r == 0.0 => return None,
                _ => v % r,
            };
        }
        Some(v)
    }

    fn unary(&mut self) -> Option<f64> {
        if let Some(op) = self.eat_op(&['-', '+']) {
            self.enter()?;
            let v = self.unary();
            self.depth -= 1;
            let v = v?;
            return Some(if op == '-' { -v } else { v });
        }
        self.power()
    }

    fn power(&mut self) -> Option<f64> {
        let base = self.atom()?;
        if self.eat_op(&['^']).is_some() {
            self.enter()?;
            let exp = self.unary(); // right-associative, and `2^-2` works
            self.depth -= 1;
            let v = base.powf(exp?);
            return v.is_finite().then_some(v);
        }
        Some(base)
    }

    fn atom(&mut self) -> Option<f64> {
        let tok = self.peek()?.clone();
        self.pos += 1;
        match tok {
            Tok::Num(n) => Some(n),
            Tok::Open => {
                self.composite = true;
                self.enter()?;
                let v = self.expr();
                self.depth -= 1;
                let v = v?;
                matches!(self.peek(), Some(Tok::Close)).then(|| self.pos += 1)?;
                Some(v)
            }
            Tok::Ident(name) => {
                if let Some(c) = constant(&name) {
                    return Some(c);
                }
                self.composite = true;
                matches!(self.peek(), Some(Tok::Open)).then(|| self.pos += 1)?;
                self.enter()?;
                let arg = self.expr();
                self.depth -= 1;
                let arg = arg?;
                matches!(self.peek(), Some(Tok::Close)).then(|| self.pos += 1)?;
                let v = function(&name, arg)?;
                v.is_finite().then_some(v)
            }
            Tok::Op(_) | Tok::Close => None,
        }
    }

    fn enter(&mut self) -> Option<()> {
        self.depth += 1;
        (self.depth <= MAX_DEPTH).then_some(())
    }
}

fn constant(name: &str) -> Option<f64> {
    match name {
        "pi" => Some(std::f64::consts::PI),
        "e" => Some(std::f64::consts::E),
        "tau" => Some(std::f64::consts::TAU),
        _ => None,
    }
}

fn function(name: &str, x: f64) -> Option<f64> {
    Some(match name {
        "sqrt" if x >= 0.0 => x.sqrt(),
        "cbrt" => x.cbrt(),
        "sin" => x.sin(),
        "cos" => x.cos(),
        "tan" => x.tan(),
        "asin" if (-1.0..=1.0).contains(&x) => x.asin(),
        "acos" if (-1.0..=1.0).contains(&x) => x.acos(),
        "atan" => x.atan(),
        "ln" if x > 0.0 => x.ln(),
        "log" | "log10" if x > 0.0 => x.log10(),
        "log2" if x > 0.0 => x.log2(),
        "exp" => x.exp(),
        "abs" => x.abs(),
        "floor" => x.floor(),
        "ceil" => x.ceil(),
        "round" => x.round(),
        _ => return None,
    })
}

/// Twelve significant digits with trailing zeros trimmed, so `0.1+0.2` reads
/// `0.3`; very large and very small values switch to scientific notation.
fn format(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let abs = v.abs();
    if !(1e-6..1e15).contains(&abs) {
        let s = format!("{:.11e}", v);
        let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let mant = mant.trim_end_matches('0').trim_end_matches('.');
        return format!("{mant}e{exp}");
    }
    let decimals = (11 - abs.log10().floor() as i32).clamp(0, 15) as usize;
    let s = format!("{v:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str) -> Option<f64> {
        evaluate(s)
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn precedence_and_unary_minus() {
        assert!(close(ev("2+3*4"), 14.0));
        assert!(close(ev("(2+3)*4"), 20.0));
        assert!(close(ev("10-3-2"), 5.0));
        assert!(close(ev("100/10/2"), 5.0));
        // the cases the woven-launch calculator got wrong
        assert!(close(ev("2*-3"), -6.0));
        assert!(close(ev("2^-2"), 0.25));
        assert!(close(ev("10/-2"), -5.0));
        assert!(close(ev("--3"), 3.0));
        assert!(close(ev("3--3"), 6.0));
        // and the ones it got right
        assert!(close(ev("-2^2"), -4.0));
        assert!(close(ev("2^3^2"), 512.0));
        assert!(close(ev("-(2+3)*2"), -10.0));
        assert!(close(ev("+5"), 5.0));
    }

    #[test]
    fn modulo_constants_functions_and_notation() {
        assert!(close(ev("17%5"), 2.0));
        assert!(close(ev("2*pi"), std::f64::consts::TAU));
        assert!(close(ev("π"), std::f64::consts::PI));
        assert!(close(ev("e^1"), std::f64::consts::E));
        assert!(close(ev("sqrt(16)+1"), 5.0));
        assert!(close(ev("SQRT(2)^2"), 2.0));
        assert!(close(ev("log(1000)"), 3.0));
        assert!(close(ev("ln(e)"), 1.0));
        assert!(close(ev("round(2.5)"), 3.0));
        assert!(close(ev("1e3+1"), 1001.0));
        assert!(close(ev("2.5E-1"), 0.25));
        assert!(close(ev("1_000*2"), 2000.0));
        assert!(close(ev("6×7"), 42.0));
        assert!(close(ev("9÷3"), 3.0));
    }

    #[test]
    fn malformed_and_impossible_input_gives_no_answer() {
        for bad in ["", "   ", "2+", "*3", "(2", "2)", "()", "1 2", "foo", "foo(1)", "sqrt", "sqrt()", "sqrt(-1)", "ln(0)", "asin(2)", "2e", "1.2.3", "2 $ 3", "1/0", "5%0", "0/0", "10^1000", "sin(1,2)"] {
            assert_eq!(ev(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn hostile_depth_and_length_are_refused_not_crashed() {
        let deep = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
        assert_eq!(ev(&deep), None);
        assert_eq!(ev(&"-".repeat(5000)), None);
        assert_eq!(ev(&"1+".repeat(5000)), None);
        let ok_depth = format!("{}1{}", "(".repeat(40), ")".repeat(40));
        assert!(close(ev(&ok_depth), 1.0));
        assert_eq!(answer(&"9".repeat(500)), None);
    }

    #[test]
    fn answer_only_offers_real_sums() {
        assert_eq!(answer("42"), None);
        assert_eq!(answer("pi"), None);
        assert_eq!(answer("firefox"), None);
        assert_eq!(answer("1.5"), None);
        assert_eq!(answer("2+2").as_deref(), Some("4"));
        assert_eq!(answer("(5)").as_deref(), Some("5"));
        assert_eq!(answer("sqrt(16)").as_deref(), Some("4"));
        assert_eq!(answer("  2 * 3  ").as_deref(), Some("6"));
        assert_eq!(answer("2*"), None);
        assert_eq!(answer("1/0"), None);
    }

    #[test]
    fn numbers_read_cleanly() {
        let a = |s: &str| answer(s).unwrap();
        assert_eq!(a("0.1+0.2"), "0.3");
        assert_eq!(a("1/3"), "0.333333333333");
        assert_eq!(a("2/3"), "0.666666666667");
        assert_eq!(a("10/4"), "2.5");
        assert_eq!(a("1000000*1000000"), "1000000000000");
        assert_eq!(a("2^60"), "1.15292150461e18");
        assert_eq!(a("1/1000000000"), "1e-9");
        assert_eq!(a("-1/3"), "-0.333333333333");
        assert_eq!(a("5-5"), "0");
        assert_eq!(a("123456789*10"), "1234567890");
        assert_eq!(a("1e20+1"), "1e20");
    }
}
