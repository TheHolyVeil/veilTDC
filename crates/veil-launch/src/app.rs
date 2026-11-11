//! The launcher window: what is typed, what matches, and how it is drawn.
//! Plugged into `veil_ui::modal`, which does the Wayland side; this module
//! only turns keys and pointer events into state and state into pixels, so
//! all of it is tested without a display.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tiny_skia::{Color, Pixmap};
use veil_palette::Tokens;
use veil_text::{FontConfig, Role, TextSystem};
use veil_ui::modal::{Flow, Key, ModalApp, Pointer};
use veil_ui::{fill_rect, fill_rounded_rect, stroke_rounded_rect, Keysym, Metrics, Theme};

use crate::desktop::App;
use crate::history::History;
use crate::input::LineEdit;
use crate::{calc, search};

/// Rows visible at once; the list scrolls past this.
pub const MAX_ROWS: usize = 8;
/// Matches kept; past a couple of hundred nobody is scrolling.
const MAX_HITS: usize = 200;
const BLINK: Duration = Duration::from_millis(530);

/// What choosing a row asks the outside world to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Run {
    /// Start this command line (`sh -c`).
    Exec(String),
    /// Put this text on the clipboard.
    Copy(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Calc(String),
    App(usize),
}

/// A rectangle as `(x, y, w, h)`.
type Rect = (f32, f32, f32, f32);

struct Layout {
    panel: Rect,
    search: Rect,
    /// Visible rows, top to bottom, paired with their index into `rows`.
    rows: Vec<(usize, Rect)>,
    /// The list area (rows plus padding), when there is anything below the box.
    list: Option<Rect>,
}

pub struct LauncherApp {
    apps: Vec<App>,
    history: History,
    edit: LineEdit,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
    ts: TextSystem,
    theme: Theme,
    metrics: Metrics,
    /// Last surface size drawn, for hit-testing.
    size: (u32, u32),
    blink_epoch: Instant,
    blink_on: bool,
    runner: Box<dyn FnMut(Run)>,
    clock: Box<dyn Fn() -> u64>,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn with_alpha(c: Color, a: f32) -> Color {
    Color::from_rgba(c.red(), c.green(), c.blue(), a).unwrap_or(c)
}

impl LauncherApp {
    pub fn new(apps: Vec<App>, history: History, tokens: Tokens, font: Option<&FontConfig>, runner: Box<dyn FnMut(Run)>) -> Self {
        let font_cfg = font.cloned().unwrap_or_default();
        let mut app = LauncherApp {
            apps,
            history,
            edit: LineEdit::default(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            ts: TextSystem::new(&font_cfg),
            theme: Theme::from_palette(&tokens),
            metrics: Metrics::from_font_size(font_cfg.size),
            size: (0, 0),
            blink_epoch: Instant::now(),
            blink_on: true,
            runner,
            clock: Box::new(unix_now),
        };
        app.requery();
        app
    }

    /// Parse the UI font and rasterise common glyphs now, so the first frame
    /// is not the one that pays for it.
    pub fn prewarm(&mut self) {
        let _ = self.render(640, 480);
        self.size = (0, 0);
    }

    // ---------------------------------------------------------------- state

    fn requery(&mut self) {
        let q = self.edit.text().trim().to_string();
        let now = (self.clock)();
        let typing = !q.is_empty();
        let history = &self.history;
        // Frecency reorders the idle list in full, but only nudges real matches.
        let boost = |a: &App| {
            let b = history.boost(&a.id, now);
            if typing {
                b / 2
            } else {
                b
            }
        };
        let hits = search::search(&self.apps, &q, &boost);

        self.rows.clear();
        if let Some(text) = calc::answer(&q) {
            self.rows.push(Row::Calc(text));
        }
        self.rows.extend(hits.iter().take(MAX_HITS).map(|h| Row::App(h.index)));
        self.selected = 0;
        self.scroll = 0;
    }

    fn move_selection(&mut self, delta: isize, wrap: bool) -> bool {
        let n = self.rows.len() as isize;
        if n == 0 {
            return false;
        }
        let next = if wrap { (self.selected as isize + delta).rem_euclid(n) } else { (self.selected as isize + delta).clamp(0, n - 1) };
        let changed = next as usize != self.selected;
        self.selected = next as usize;
        self.reveal_selected();
        changed
    }

    fn reveal_selected(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + MAX_ROWS {
            self.scroll = self.selected + 1 - MAX_ROWS;
        }
    }

    fn activate(&mut self, index: usize) -> Flow {
        let Some(row) = self.rows.get(index).cloned() else { return Flow::Idle };
        match row {
            Row::App(i) => {
                let (id, exec) = (self.apps[i].id.clone(), self.apps[i].exec.clone());
                self.history.record(&id, (self.clock)());
                (self.runner)(Run::Exec(exec));
            }
            Row::Calc(text) => (self.runner)(Run::Copy(text)),
        }
        Flow::Close
    }

    /// Typing changes what matches; anything else that moved the cursor only
    /// needs a redraw.
    fn edited(&mut self, changed: bool) -> Flow {
        if changed {
            self.requery();
            Flow::Redraw
        } else {
            Flow::Idle
        }
    }

    fn moved(changed: bool) -> Flow {
        if changed {
            Flow::Redraw
        } else {
            Flow::Idle
        }
    }

    // --------------------------------------------------------------- layout

    fn layout(&self, w: u32, h: u32) -> Layout {
        let m = &self.metrics;
        let (wf, hf) = (w as f32, h as f32);
        let panel_w = m.px(640.0).min(wf - m.px(32.0)).max(1.0);
        let search_h = m.px(56.0);
        let row_h = m.px(48.0);
        let pad = m.px(8.0);

        let visible = self.rows.len().saturating_sub(self.scroll).min(MAX_ROWS);
        // With text typed and nothing matching, one row holds the message.
        let list_rows = if self.rows.is_empty() && !self.edit.is_empty() { 1 } else { visible };
        let list_h = if list_rows > 0 { pad * 2.0 + list_rows as f32 * row_h } else { 0.0 };
        let panel_h = search_h + list_h;

        let x = ((wf - panel_w) / 2.0).max(0.0);
        let y = (hf * 0.2).min(hf - panel_h - m.px(16.0)).max(m.px(16.0).min(hf / 4.0));
        let rows = (0..visible)
            .map(|i| (self.scroll + i, (x + pad, y + search_h + pad + i as f32 * row_h, panel_w - pad * 2.0, row_h)))
            .collect();
        Layout {
            panel: (x, y, panel_w, panel_h),
            search: (x, y, panel_w, search_h),
            rows,
            list: (list_h > 0.0).then_some((x, y + search_h, panel_w, list_h)),
        }
    }

    fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        let l = self.layout(self.size.0, self.size.1);
        l.rows.iter().find(|(_, r)| inside(*r, x, y)).map(|(i, _)| *i)
    }

    // -------------------------------------------------------------- drawing

    fn draw(&mut self, w: u32, h: u32) -> Vec<u8> {
        let Some(mut pm) = Pixmap::new(w.max(1), h.max(1)) else { return Vec::new() };
        let l = self.layout(w, h);
        let m = self.metrics;
        let t = self.theme.clone();
        let (wf, hf) = (w as f32, h as f32);

        fill_rect(&mut pm, 0.0, 0.0, wf, hf, Color::from_rgba(0.0, 0.0, 0.0, 0.32).unwrap_or(t.bg));
        let (px, py, pw, ph) = l.panel;
        fill_rounded_rect(&mut pm, px, py, pw, ph, m.px(14.0), t.surface);
        stroke_rounded_rect(&mut pm, px + 0.5, py + 0.5, pw - 1.0, ph - 1.0, m.px(14.0), t.border, 1.0);

        self.draw_search(&mut pm, l.search);

        if let Some((lx, ly, lw, _)) = l.list {
            fill_rect(&mut pm, lx + m.px(1.0), ly, lw - m.px(2.0), 1.0, t.border);
        }
        if self.rows.is_empty() && !self.edit.is_empty() {
            let (lx, ly, lw, lh) = l.list.unwrap_or((px, py, pw, 0.0));
            let msg = "No matching apps";
            let (tw, _) = self.ts.measure(msg, Role::UI, m.font);
            self.ts.draw(&mut pm, msg, lx + (lw - tw) / 2.0, ly + (lh - m.font) / 2.0, Role::UI, m.font, t.text_dim);
        }
        let first = self.scroll;
        for (idx, rect) in &l.rows {
            self.draw_row(&mut pm, *idx, *rect, *idx == self.selected);
        }
        self.draw_scrollbar(&mut pm, &l, first);

        pm.take()
    }

    fn draw_search(&mut self, pm: &mut Pixmap, (x, y, w, h): Rect) {
        let m = self.metrics;
        let t = self.theme.clone();
        let size = m.font * 1.25;
        let pad = m.px(20.0);
        let ty = y + (h - size) / 2.0;
        let inner = (w - pad * 2.0).max(1.0);

        if self.edit.is_empty() {
            let hint = self.ts.ellipsize("Search apps or calculate\u{2026}", Role::UI, size, inner);
            self.ts.draw(pm, &hint, x + pad, ty, Role::UI, size, t.text_dim);
        } else {
            // Show the tail of a query that is wider than the box.
            let text = self.edit.text();
            let mut start = 0;
            while start < text.len() && self.ts.measure(&text[start..], Role::UI, size).0 > inner - m.px(4.0) {
                start += text[start..].chars().next().map_or(1, char::len_utf8);
            }
            let cursor = self.edit.cursor().max(start);
            let shown = text[start..].to_string();
            self.ts.draw(pm, &shown, x + pad, ty, Role::UI, size, t.text);
            if self.blink_on {
                let cx = x + pad + self.ts.measure(&text[start..cursor], Role::UI, size).0;
                fill_rect(pm, cx, ty - m.px(1.0), m.px(2.0).max(1.0), size + m.px(2.0), t.accent);
            }
            return;
        }
        if self.blink_on {
            fill_rect(pm, x + pad, ty - m.px(1.0), m.px(2.0).max(1.0), size + m.px(2.0), t.accent);
        }
    }

    fn draw_row(&mut self, pm: &mut Pixmap, idx: usize, (x, y, w, h): Rect, selected: bool) {
        let m = self.metrics;
        let t = self.theme.clone();
        if selected {
            fill_rounded_rect(pm, x, y, w, h, m.px(10.0), t.surface_hi);
            fill_rounded_rect(pm, x, y + h * 0.22, m.px(3.0), h * 0.56, m.px(1.5), t.accent);
        }

        let badge = h - m.px(16.0);
        let (bx, by) = (x + m.px(14.0), y + (h - badge) / 2.0);
        let (title, subtitle, letter, calc) = match &self.rows[idx] {
            Row::Calc(text) => (text.clone(), "Press Enter to copy".to_string(), "=".to_string(), true),
            Row::App(i) => {
                let a = &self.apps[*i];
                let sub = [&a.comment, &a.generic_name].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
                let letter = a.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into());
                (a.name.clone(), sub, letter, false)
            }
        };

        fill_rounded_rect(pm, bx, by, badge, badge, m.px(8.0), with_alpha(t.accent, if calc { 0.30 } else { 0.18 }));
        let (lw, _) = self.ts.measure(&letter, Role::UI, m.font * 1.15);
        self.ts.draw(pm, &letter, bx + (badge - lw) / 2.0, by + (badge - m.font * 1.15) / 2.0, Role::UI, m.font * 1.15, t.accent);

        let tx = bx + badge + m.px(14.0);
        let max_w = (x + w - tx - m.px(14.0)).max(1.0);
        let title_size = if calc { m.font * 1.15 } else { m.font };
        if subtitle.is_empty() {
            let s = self.ts.ellipsize(&title, Role::UI, title_size, max_w);
            self.ts.draw(pm, &s, tx, y + (h - title_size) / 2.0, Role::UI, title_size, t.text);
        } else {
            let s = self.ts.ellipsize(&title, Role::UI, title_size, max_w);
            self.ts.draw(pm, &s, tx, y + m.px(6.0), Role::UI, title_size, t.text);
            let sub = self.ts.ellipsize(&subtitle, Role::UI, m.small, max_w);
            self.ts.draw(pm, &sub, tx, y + m.px(26.0), Role::UI, m.small, t.text_dim);
        }
    }

    fn draw_scrollbar(&self, pm: &mut Pixmap, l: &Layout, first: usize) {
        let Some((lx, ly, lw, lh)) = l.list else { return };
        let total = self.rows.len();
        if total <= MAX_ROWS {
            return;
        }
        let m = &self.metrics;
        let track = lh - m.px(16.0);
        let thumb = (track * MAX_ROWS as f32 / total as f32).max(m.px(18.0));
        let pos = (track - thumb) * first as f32 / (total - MAX_ROWS) as f32;
        let w = m.px(3.0);
        fill_rounded_rect(pm, lx + lw - m.px(6.0) - w, ly + m.px(8.0) + pos, w, thumb, w / 2.0, with_alpha(self.theme.text_dim, 0.5));
    }
}

fn inside((x, y, w, h): Rect, px: f32, py: f32) -> bool {
    px >= x && px < x + w && py >= y && py < y + h
}

impl ModalApp for LauncherApp {
    fn render(&mut self, w: u32, h: u32) -> Vec<u8> {
        self.size = (w, h);
        self.draw(w, h)
    }

    fn key(&mut self, key: &Key) -> Flow {
        // Any key shows the cursor again, so it never blinks away mid-typing.
        self.blink_epoch = Instant::now();
        self.blink_on = true;
        let ctrl = key.ctrl;

        match key.sym {
            Keysym::Escape => Flow::Close,
            Keysym::Return | Keysym::KP_Enter => self.activate(self.selected),
            Keysym::Up => Self::moved(self.move_selection(-1, true)),
            Keysym::Down | Keysym::Tab => Self::moved(self.move_selection(1, true)),
            Keysym::ISO_Left_Tab => Self::moved(self.move_selection(-1, true)),
            Keysym::Page_Up => Self::moved(self.move_selection(-(MAX_ROWS as isize), false)),
            Keysym::Page_Down => Self::moved(self.move_selection(MAX_ROWS as isize, false)),
            Keysym::Left if ctrl => Self::moved(self.edit.word_left()),
            Keysym::Right if ctrl => Self::moved(self.edit.word_right()),
            Keysym::Left => Self::moved(self.edit.left()),
            Keysym::Right => Self::moved(self.edit.right()),
            Keysym::Home => Self::moved(self.edit.home()),
            Keysym::End => Self::moved(self.edit.end()),
            Keysym::BackSpace if ctrl => {
                let c = self.edit.delete_word_back();
                self.edited(c)
            }
            Keysym::BackSpace => {
                let c = self.edit.backspace();
                self.edited(c)
            }
            Keysym::Delete => {
                let c = self.edit.delete();
                self.edited(c)
            }
            Keysym::a if ctrl => Self::moved(self.edit.home()),
            Keysym::e if ctrl => Self::moved(self.edit.end()),
            Keysym::u if ctrl => {
                let c = self.edit.clear();
                self.edited(c)
            }
            Keysym::w if ctrl => {
                let c = self.edit.delete_word_back();
                self.edited(c)
            }
            Keysym::p if ctrl => Self::moved(self.move_selection(-1, true)),
            Keysym::n if ctrl => Self::moved(self.move_selection(1, true)),
            _ => match &key.text {
                Some(t) if !ctrl && !key.alt => {
                    let before = self.edit.text().len();
                    self.edit.insert(t);
                    let changed = self.edit.text().len() != before;
                    self.edited(changed)
                }
                _ => Flow::Idle,
            },
        }
    }

    fn pointer(&mut self, ev: Pointer) -> Flow {
        match ev {
            Pointer::Move { x, y } => match self.row_at(x, y) {
                Some(i) if i != self.selected => {
                    self.selected = i;
                    Flow::Redraw
                }
                _ => Flow::Idle,
            },
            Pointer::Click { x, y } => {
                if let Some(i) = self.row_at(x, y) {
                    return self.activate(i);
                }
                let l = self.layout(self.size.0, self.size.1);
                // The dimmed area around the panel is "click away to dismiss".
                if inside(l.panel, x, y) {
                    Flow::Idle
                } else {
                    Flow::Close
                }
            }
            Pointer::Scroll { dy } => Self::moved(self.move_selection(if dy > 0.0 { 1 } else { -1 }, false)),
            Pointer::Leave => Flow::Idle,
        }
    }

    fn blur(&self, w: u32, h: u32) -> Vec<(i32, i32, i32, i32)> {
        let (x, y, pw, ph) = self.layout(w, h).panel;
        vec![(x.floor() as i32, y.floor() as i32, pw.ceil() as i32, ph.ceil() as i32)]
    }

    fn next_tick(&self, now: Instant) -> Option<Duration> {
        let elapsed = now.saturating_duration_since(self.blink_epoch);
        let into = Duration::from_nanos((elapsed.as_nanos() % BLINK.as_nanos()) as u64);
        Some(BLINK - into)
    }

    fn tick(&mut self, now: Instant) -> Flow {
        let phase = now.saturating_duration_since(self.blink_epoch).as_nanos() / BLINK.as_nanos();
        let on = phase % 2 == 0;
        if on == self.blink_on {
            return Flow::Idle;
        }
        self.blink_on = on;
        Flow::Redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    fn app(name: &str, program: &str) -> App {
        App {
            id: format!("{program}.desktop"),
            name: name.into(),
            generic_name: String::new(),
            comment: String::new(),
            keywords: vec![],
            categories: vec![],
            icon: String::new(),
            exec: program.into(),
            path: Default::default(),
            program: program.into(),
            ..blank()
        }
        .index()
    }

    fn blank() -> App {
        App {
            id: String::new(),
            name: String::new(),
            generic_name: String::new(),
            comment: String::new(),
            keywords: vec![],
            categories: vec![],
            icon: String::new(),
            exec: String::new(),
            path: Default::default(),
            program: String::new(),
            name_lc: String::new(),
            generic_lc: String::new(),
            comment_lc: String::new(),
            keywords_lc: vec![],
            program_lc: String::new(),
        }
    }

    type Log = Rc<RefCell<Vec<Run>>>;

    fn launcher(apps: Vec<App>) -> (LauncherApp, Log) {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let sink = log.clone();
        let mut l = LauncherApp::new(apps, History::in_memory(), Tokens::default(), None, Box::new(move |r| sink.borrow_mut().push(r)));
        let now = Rc::new(Cell::new(1_800_000_000u64));
        l.clock = Box::new(move || now.get());
        (l, log)
    }

    fn k(sym: Keysym) -> Key {
        Key { sym, text: None, ctrl: false, alt: false, shift: false, repeat: false }
    }
    fn ctrl(sym: Keysym) -> Key {
        Key { ctrl: true, ..k(sym) }
    }
    fn typed(l: &mut LauncherApp, s: &str) {
        for c in s.chars() {
            let key = Key { text: Some(c.to_string()), ..k(Keysym::from_char(c)) };
            l.key(&key);
        }
    }
    fn names(l: &LauncherApp) -> Vec<String> {
        l.rows
            .iter()
            .map(|r| match r {
                Row::App(i) => l.apps[*i].name.clone(),
                Row::Calc(t) => format!("={t}"),
            })
            .collect()
    }
    fn many(n: usize) -> Vec<App> {
        (0..n).map(|i| app(&format!("App {i:02}"), &format!("app{i:02}"))).collect()
    }

    #[test]
    fn it_opens_listing_everything() {
        let (l, _) = launcher(vec![app("Zed", "zed"), app("Alpha", "alpha")]);
        assert_eq!(names(&l), ["Zed", "Alpha"], "in the order the scan gave them");
        assert_eq!(l.selected, 0);
    }

    #[test]
    fn typing_narrows_and_resets_the_selection() {
        let (mut l, _) = launcher(vec![app("Firefox", "firefox"), app("Files", "nautilus"), app("Foot", "foot")]);
        l.key(&k(Keysym::Down));
        assert_eq!(l.selected, 1);
        typed(&mut l, "fi");
        assert_eq!(names(&l), ["Files", "Firefox"]);
        assert_eq!(l.selected, 0);
        typed(&mut l, "x");
        assert_eq!(names(&l), ["Firefox"]);
        assert_eq!(l.key(&k(Keysym::BackSpace)), Flow::Redraw);
        assert_eq!(names(&l), ["Files", "Firefox"]);
    }

    #[test]
    fn enter_launches_the_selection_records_it_and_closes() {
        let (mut l, log) = launcher(vec![app("Firefox", "firefox"), app("Foot", "foot")]);
        typed(&mut l, "foo");
        assert_eq!(l.key(&k(Keysym::Return)), Flow::Close);
        assert_eq!(*log.borrow(), [Run::Exec("foot".into())]);
        assert!(l.history.boost("foot.desktop", 1_800_000_000) > 0);
    }

    #[test]
    fn enter_with_nothing_selected_does_nothing() {
        let (mut l, log) = launcher(vec![app("Firefox", "firefox")]);
        typed(&mut l, "zzzz");
        assert!(l.rows.is_empty());
        assert_eq!(l.key(&k(Keysym::Return)), Flow::Idle);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn the_calculator_row_comes_first_and_enter_copies_it() {
        let (mut l, log) = launcher(vec![app("Two Times", "two")]);
        typed(&mut l, "2*-3");
        assert_eq!(names(&l)[0], "=-6");
        assert_eq!(l.key(&k(Keysym::Return)), Flow::Close);
        assert_eq!(*log.borrow(), [Run::Copy("-6".into())]);
        assert!(l.history.is_empty(), "a sum is not an app launch");
    }

    #[test]
    fn a_plain_number_or_word_is_not_a_calculation() {
        let (mut l, _) = launcher(vec![app("Firefox", "firefox")]);
        typed(&mut l, "42");
        assert!(names(&l).iter().all(|n| !n.starts_with('=')));
        l.key(&ctrl(Keysym::u));
        typed(&mut l, "fire");
        assert_eq!(names(&l), ["Firefox"]);
    }

    #[test]
    fn selection_wraps_and_the_list_scrolls_to_keep_it_visible() {
        let (mut l, _) = launcher(many(20));
        assert_eq!(l.key(&k(Keysym::Up)), Flow::Redraw);
        assert_eq!(l.selected, 19, "Up from the top wraps to the end");
        assert_eq!(l.scroll, 20 - MAX_ROWS);
        l.key(&k(Keysym::Down));
        assert_eq!((l.selected, l.scroll), (0, 0));
        for _ in 0..MAX_ROWS {
            l.key(&k(Keysym::Down));
        }
        assert_eq!((l.selected, l.scroll), (MAX_ROWS, 1));
        l.key(&k(Keysym::Tab));
        assert_eq!(l.selected, MAX_ROWS + 1);
        l.key(&k(Keysym::ISO_Left_Tab));
        assert_eq!(l.selected, MAX_ROWS);
        l.key(&ctrl(Keysym::p));
        l.key(&ctrl(Keysym::n));
        assert_eq!(l.selected, MAX_ROWS);
    }

    #[test]
    fn page_keys_and_scrolling_stop_at_the_ends() {
        let (mut l, _) = launcher(many(20));
        l.key(&k(Keysym::Page_Down));
        assert_eq!(l.selected, MAX_ROWS);
        l.key(&k(Keysym::Page_Down));
        l.key(&k(Keysym::Page_Down));
        assert_eq!(l.selected, 19);
        assert_eq!(l.key(&k(Keysym::Page_Down)), Flow::Idle, "already at the end");
        for _ in 0..5 {
            l.key(&k(Keysym::Page_Up));
        }
        assert_eq!((l.selected, l.scroll), (0, 0));
        assert_eq!(l.pointer(Pointer::Scroll { dy: -15.0 }), Flow::Idle, "no wrap on the wheel");
        assert_eq!(l.pointer(Pointer::Scroll { dy: 15.0 }), Flow::Redraw);
        assert_eq!(l.selected, 1);
    }

    #[test]
    fn editing_keys() {
        let (mut l, _) = launcher(many(3));
        typed(&mut l, "foo bar");
        l.key(&k(Keysym::Left));
        l.key(&k(Keysym::BackSpace));
        assert_eq!(l.edit.text(), "foo br");
        l.key(&k(Keysym::Delete));
        assert_eq!(l.edit.text(), "foo b");
        l.key(&ctrl(Keysym::a));
        assert_eq!(l.edit.cursor(), 0);
        l.key(&ctrl(Keysym::e));
        assert_eq!(l.edit.cursor(), 5);
        l.key(&ctrl(Keysym::w));
        assert_eq!(l.edit.text(), "foo ");
        l.key(&ctrl(Keysym::BackSpace));
        assert_eq!(l.edit.text(), "");
        typed(&mut l, "abc");
        l.key(&ctrl(Keysym::u));
        assert!(l.edit.is_empty());
        assert_eq!(l.key(&k(Keysym::BackSpace)), Flow::Idle, "nothing to delete");
        assert_eq!(l.key(&k(Keysym::Left)), Flow::Idle);
        typed(&mut l, "x y");
        assert_eq!(l.key(&ctrl(Keysym::Left)), Flow::Redraw);
        assert_eq!(l.edit.cursor(), 2);
        assert_eq!(l.key(&ctrl(Keysym::Right)), Flow::Redraw);
        assert_eq!(l.key(&k(Keysym::Home)), Flow::Redraw);
        assert_eq!(l.key(&k(Keysym::End)), Flow::Redraw);
    }

    #[test]
    fn control_and_alt_chords_never_type_text_and_escape_closes() {
        let (mut l, _) = launcher(many(3));
        let chord = Key { text: Some("q".into()), ctrl: true, ..k(Keysym::q) };
        assert_eq!(l.key(&chord), Flow::Idle);
        let alt = Key { text: Some("q".into()), alt: true, ..k(Keysym::q) };
        assert_eq!(l.key(&alt), Flow::Idle);
        assert!(l.edit.is_empty());
        assert_eq!(l.key(&k(Keysym::F5)), Flow::Idle);
        assert_eq!(l.key(&k(Keysym::Escape)), Flow::Close);
    }

    #[test]
    fn what_you_launch_often_floats_up() {
        let (mut l, _) = launcher(vec![app("Alpha", "alpha"), app("Beta", "beta"), app("Gamma", "gamma")]);
        assert_eq!(names(&l), ["Alpha", "Beta", "Gamma"]);
        l.history.record("gamma.desktop", 1_800_000_000);
        l.requery();
        assert_eq!(names(&l), ["Gamma", "Alpha", "Beta"]);
    }

    #[test]
    fn history_only_nudges_a_typed_search() {
        let (mut l, _) = launcher(vec![app("Terminal", "term"), app("Term", "t2")]);
        for _ in 0..20 {
            l.history.record("term.desktop", 1_800_000_000);
        }
        l.edit.insert("term");
        l.requery();
        assert_eq!(names(&l), ["Term", "Terminal"], "an exact name still beats a favourite");
    }

    #[test]
    fn clicking_a_row_launches_it_and_clicking_away_closes() {
        let (mut l, log) = launcher(many(5));
        l.render(1280, 800);
        let lay = l.layout(1280, 800);
        let (_, (rx, ry, rw, rh)) = lay.rows[2];
        assert_eq!(l.pointer(Pointer::Click { x: rx + rw / 2.0, y: ry + rh / 2.0 }), Flow::Close);
        assert_eq!(*log.borrow(), [Run::Exec("app02".into())]);

        let (mut l, log) = launcher(many(5));
        l.render(1280, 800);
        assert_eq!(l.pointer(Pointer::Click { x: 3.0, y: 3.0 }), Flow::Close, "outside the panel");
        assert!(log.borrow().is_empty());
        let (px, py, _, _) = l.layout(1280, 800).panel;
        assert_eq!(l.pointer(Pointer::Click { x: px + 5.0, y: py + 5.0 }), Flow::Idle, "inside the search box");
    }

    #[test]
    fn hovering_selects_the_row_under_the_pointer() {
        let (mut l, _) = launcher(many(5));
        l.render(1280, 800);
        let lay = l.layout(1280, 800);
        let (_, (rx, ry, _, rh)) = lay.rows[3];
        assert_eq!(l.pointer(Pointer::Move { x: rx + 10.0, y: ry + rh / 2.0 }), Flow::Redraw);
        assert_eq!(l.selected, 3);
        assert_eq!(l.pointer(Pointer::Move { x: rx + 11.0, y: ry + rh / 2.0 }), Flow::Idle, "same row");
        assert_eq!(l.pointer(Pointer::Move { x: 1.0, y: 1.0 }), Flow::Idle, "off the list keeps the selection");
        assert_eq!(l.selected, 3);
        assert_eq!(l.pointer(Pointer::Leave), Flow::Idle);
    }

    #[test]
    fn the_cursor_blinks_and_typing_restarts_it() {
        let (mut l, _) = launcher(many(1));
        let t0 = l.blink_epoch;
        assert!(l.blink_on);
        assert_eq!(l.next_tick(t0), Some(BLINK));
        assert_eq!(l.tick(t0 + BLINK / 2), Flow::Idle);
        assert_eq!(l.tick(t0 + BLINK), Flow::Redraw);
        assert!(!l.blink_on);
        assert_eq!(l.tick(t0 + BLINK + BLINK / 4), Flow::Idle);
        assert_eq!(l.tick(t0 + BLINK * 2), Flow::Redraw);
        assert!(l.blink_on);
        l.blink_on = false;
        typed(&mut l, "a");
        assert!(l.blink_on, "a key press shows the cursor");
        let wait = l.next_tick(l.blink_epoch + Duration::from_millis(100)).unwrap();
        assert_eq!(wait, BLINK - Duration::from_millis(100));
    }

    #[test]
    fn the_blur_region_is_the_panel_and_stays_on_screen() {
        let (l, _) = launcher(many(20));
        let blur = l.blur(1920, 1080);
        assert_eq!(blur.len(), 1);
        let (x, y, w, h) = blur[0];
        assert!(x >= 0 && y >= 0 && x + w <= 1920 && y + h <= 1080, "{blur:?}");
        assert!(w < 1920 && h < 1080);
        // a surface smaller than the panel still gives a sane region
        let (x, _, w, _) = l.blur(300, 200)[0];
        assert!(x >= 0 && w <= 300);
    }

    #[test]
    fn rendering_fills_the_surface_dims_it_and_draws_the_panel() {
        let (mut l, _) = launcher(many(4));
        let (w, h) = (1280u32, 800u32);
        let px = l.render(w, h);
        assert_eq!(px.len(), (w * h * 4) as usize);
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        let corner = at(2, 2);
        assert!(corner[3] > 0 && corner[3] < 255, "scrim, not opaque: {corner:?}");
        let (x, y, pw, ph) = l.layout(w, h).panel;
        let centre = at((x + pw / 2.0) as u32, (y + ph - 4.0) as u32);
        assert_eq!(centre[3], 255, "the panel itself is opaque");
        assert_ne!(centre, corner);
    }

    #[test]
    fn the_selected_row_is_highlighted() {
        let (mut l, _) = launcher(many(4));
        let (w, h) = (1280u32, 800u32);
        let a = l.render(w, h);
        l.key(&k(Keysym::Down));
        let b = l.render(w, h);
        assert_ne!(a, b);
        let lay = l.layout(w, h);
        let (_, (rx, ry, rw, rh)) = lay.rows[1];
        let i = (((ry + rh / 2.0) as u32 * w + (rx + rw - 20.0) as u32) * 4) as usize;
        let j = (((lay.rows[0].1 .1 + rh / 2.0) as u32 * w + (rx + rw - 20.0) as u32) * 4) as usize;
        assert_ne!(b[i..i + 4], b[j..j + 4], "selected and unselected rows differ");
    }

    #[test]
    fn no_results_shows_a_message_row() {
        let (mut l, _) = launcher(many(3));
        typed(&mut l, "qqqqq");
        let lay = l.layout(1280, 800);
        assert!(lay.rows.is_empty());
        assert!(lay.list.is_some(), "room for the message");
        let px = l.render(1280, 800);
        assert_eq!(px.len(), 1280 * 800 * 4);
        let (_, _, _, ph) = lay.panel;
        assert!(ph > l.metrics.px(56.0), "taller than the bare search box");
    }

    #[test]
    fn awkward_input_and_surfaces_do_not_panic() {
        let (mut l, _) = launcher(vec![app(&"Wide ".repeat(80), "wide"), app("Ünï ✓ 😀", "uni")]);
        typed(&mut l, &"é✓".repeat(60));
        for (w, h) in [(1280, 800), (320, 200), (50, 50), (1, 1), (4000, 3000)] {
            let px = l.render(w, h);
            assert_eq!(px.len(), (w * h * 4) as usize);
            let _ = l.blur(w, h);
            let _ = l.pointer(Pointer::Click { x: 10.0, y: 10.0 });
        }
        l.key(&ctrl(Keysym::u));
        typed(&mut l, "Ünï");
        let _ = l.render(1280, 800);
    }

    #[test]
    fn prewarm_leaves_no_trace_in_the_state() {
        let (mut l, _) = launcher(many(3));
        l.prewarm();
        assert_eq!(l.size, (0, 0));
        assert_eq!(l.rows.len(), 3);
    }
}
