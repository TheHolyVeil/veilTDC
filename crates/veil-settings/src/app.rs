//! The settings window: state, keys, pointer and drawing, as a
//! `veil_ui::modal` app. Like the launcher it knows nothing about Wayland or
//! sockets: saving goes through a `Sender` closure and re-reading the config
//! through a `Loader`, so every behaviour here is tested without a display.

use std::time::{Duration, Instant};

use tiny_skia::{Color, Pixmap};
use veil_config::schema::{self, EditValue, Kind, SettingDef};
use veil_config::VeilConfig;
use veil_palette::Tokens;
use veil_text::{FontConfig, Role, TextSystem};
use veil_ui::modal::{Flow, Key, ModalApp, Pointer};
use veil_ui::{fill_circle, fill_rect, fill_rounded_rect, stroke_rounded_rect, Keysym, LineEdit, Metrics, Theme};

use crate::model::{self, TabDef};

/// How long a Left/Right nudge waits for the next one before it is saved, so
/// holding a key writes the file once, not twenty-five times a second.
pub const DEBOUNCE: Duration = Duration::from_millis(300);
const BLINK: Duration = Duration::from_millis(530);

type Rect = (f32, f32, f32, f32);

/// Save a change: the `config.set` arguments in, the compositor's answer out
/// (`{"path": .., "shadowed_by": ..}`), or why it failed.
pub type Sender = Box<dyn FnMut(&serde_json::Value) -> Result<Option<serde_json::Value>, String>>;
/// Read the config file again (after a save, to show what is really in it).
pub type Loader = Box<dyn FnMut() -> VeilConfig>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    None,
    Ok(String),
    Warn(String),
    Err(String),
}

struct Pending {
    def: &'static SettingDef,
    value: EditValue,
    due: Instant,
}

pub struct SettingsApp {
    cfg: VeilConfig,
    tabs: Vec<TabDef>,
    tab: usize,
    sel: usize,
    scroll: usize,
    editing: Option<LineEdit>,
    pending: Option<Pending>,
    status: Status,
    sender: Sender,
    loader: Loader,
    ts: TextSystem,
    theme: Theme,
    metrics: Metrics,
    blink_epoch: Instant,
    blink_on: bool,
    /// Size of the last frame, for hit-testing pointer events.
    size: (u32, u32),
}

pub struct Layout {
    pub panel: Rect,
    pub tabs: Vec<(usize, Rect)>,
    /// Visible rows as (index into the tab's settings, rect).
    pub rows: Vec<(usize, Rect)>,
    pub footer: Rect,
    /// How many rows fit.
    pub visible: usize,
}

impl SettingsApp {
    pub fn new(cfg: VeilConfig, tokens: Tokens, font: Option<&FontConfig>, sender: Sender, loader: Loader) -> Self {
        let font_cfg = font.cloned().unwrap_or_default();
        Self {
            cfg,
            tabs: model::tabs(),
            tab: 0,
            sel: 0,
            scroll: 0,
            editing: None,
            pending: None,
            status: Status::None,
            sender,
            loader,
            ts: TextSystem::new(&font_cfg),
            theme: Theme::from_palette(&tokens),
            metrics: Metrics::from_font_size(font_cfg.size),
            blink_epoch: Instant::now(),
            blink_on: true,
            size: (0, 0),
        }
    }

    pub fn prewarm(&mut self) {
        let _ = self.ts.measure("Settings 0123456789 abcdefghijklmnopqrstuvwxyz", Role::UI, self.metrics.font);
    }

    // ── state ────────────────────────────────────────────────────────────

    fn defs(&self) -> &[&'static SettingDef] {
        &self.tabs[self.tab].defs
    }

    fn cur_def(&self) -> Option<&'static SettingDef> {
        self.defs().get(self.sel).copied()
    }

    /// What a row shows: a nudge still waiting to be saved, else the file.
    fn value_of(&self, def: &SettingDef) -> Option<EditValue> {
        match &self.pending {
            Some(p) if p.def.path == def.path => Some(p.value.clone()),
            _ => schema::current(&self.cfg, def.path),
        }
    }

    /// `value_of`, or a starting point for a setting that has no value yet
    /// (the weather coordinates before a location is set).
    fn effective(&self, def: &SettingDef) -> EditValue {
        self.value_of(def).unwrap_or(match def.kind {
            Kind::Bool => EditValue::Bool(false),
            Kind::Int { min, max } => EditValue::Int(0_i64.clamp(min, max)),
            Kind::Float { min, max } => EditValue::Float(0.0_f64.clamp(min, max)),
            Kind::TextList => EditValue::List(Vec::new()),
            Kind::Choice(o) => EditValue::Str(o[0].to_string()),
            Kind::Text | Kind::Color => EditValue::Str(String::new()),
        })
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    pub fn selected_path(&self) -> Option<&'static str> {
        self.cur_def().map(|d| d.path)
    }

    pub fn tab_name(&self) -> &'static str {
        self.tabs[self.tab].name
    }

    // ── saving ───────────────────────────────────────────────────────────

    /// Save `value` (or reset to default for `None`) right now.
    fn commit(&mut self, def: &'static SettingDef, value: Option<EditValue>) {
        if self.pending.as_ref().is_some_and(|p| p.def.path != def.path) {
            self.flush();
        }
        self.pending = None;
        let value = match value {
            Some(v) => match def.check(&v) {
                Ok(v) => Some(v),
                Err(e) => {
                    self.status = Status::Err(e);
                    return;
                }
            },
            None => None,
        };
        let reset = value.is_none();
        let args = model::request(def.path, value.as_ref());
        match (self.sender)(&args) {
            Ok(reply) => {
                self.cfg = (self.loader)();
                let shadow = reply.as_ref().and_then(|r| r.get("shadowed_by")).and_then(|v| v.as_str()).map(str::to_string);
                self.status = match shadow {
                    Some(p) => Status::Warn(format!("Saved, but Veil is reading {p} instead")),
                    None if reset => Status::Ok("Reset to default".into()),
                    None => Status::Ok("Saved".into()),
                };
            }
            Err(e) => self.status = Status::Err(e),
        }
    }

    /// Queue a nudge; saved after `DEBOUNCE` of quiet, or on flush.
    fn queue(&mut self, def: &'static SettingDef, value: EditValue) {
        if self.pending.as_ref().is_some_and(|p| p.def.path != def.path) {
            self.flush();
        }
        self.pending = Some(Pending { def, value, due: Instant::now() + DEBOUNCE });
    }

    /// Save anything still waiting (switching rows or tabs, closing).
    pub fn flush(&mut self) {
        if let Some(p) = self.pending.take() {
            self.commit(p.def, Some(p.value));
        }
    }

    // ── actions ──────────────────────────────────────────────────────────

    fn adjust(&mut self, dir: i32) -> Flow {
        let Some(def) = self.cur_def() else { return Flow::Idle };
        match model::step(def, &self.effective(def), dir) {
            Some(v) => {
                self.queue(def, v);
                Flow::Redraw
            }
            None => Flow::Idle,
        }
    }

    /// Enter / Space / a click on the value: toggle or cycle at once, or
    /// start typing into anything else.
    fn activate(&mut self) -> Flow {
        let Some(def) = self.cur_def() else { return Flow::Idle };
        match def.kind {
            Kind::Bool | Kind::Choice(_) => {
                if let Some(v) = model::step(def, &self.effective(def), 1) {
                    self.commit(def, Some(v));
                }
            }
            _ => {
                let start = self.value_of(def).map(|v| model::edit_text(&v)).unwrap_or_default();
                let mut e = LineEdit::default();
                e.set(&start);
                self.editing = Some(e);
                self.status = Status::None;
                self.blink_epoch = Instant::now();
                self.blink_on = true;
            }
        }
        Flow::Redraw
    }

    fn finish_edit(&mut self) -> Flow {
        let (Some(def), Some(edit)) = (self.cur_def(), self.editing.as_ref()) else { return Flow::Idle };
        let text = edit.text().to_string();
        // An emptied text field goes back to its default rather than being saved as "".
        if matches!(def.kind, Kind::Text) && text.trim().is_empty() {
            self.editing = None;
            self.commit(def, None);
            return Flow::Redraw;
        }
        match model::parse_input(def, &text).and_then(|v| def.check(&v)) {
            Ok(v) => {
                self.editing = None;
                self.commit(def, Some(v));
            }
            Err(e) => self.status = Status::Err(e),
        }
        Flow::Redraw
    }

    fn reset(&mut self) -> Flow {
        if let Some(def) = self.cur_def() {
            self.commit(def, None);
        }
        Flow::Redraw
    }

    fn move_sel(&mut self, delta: isize) -> Flow {
        let n = self.defs().len();
        if n == 0 {
            return Flow::Idle;
        }
        let next = (self.sel as isize + delta).clamp(0, n as isize - 1) as usize;
        if next == self.sel {
            return Flow::Idle;
        }
        self.flush();
        self.sel = next;
        Flow::Redraw
    }

    fn switch_tab(&mut self, dir: isize) -> Flow {
        let n = self.tabs.len() as isize;
        self.set_tab((self.tab as isize + dir).rem_euclid(n) as usize)
    }

    fn set_tab(&mut self, tab: usize) -> Flow {
        if tab >= self.tabs.len() || tab == self.tab {
            return Flow::Idle;
        }
        self.flush();
        self.editing = None;
        self.tab = tab;
        self.sel = 0;
        self.scroll = 0;
        self.status = Status::None;
        Flow::Redraw
    }

    fn ensure_visible(&mut self, visible: usize) {
        let visible = visible.max(1);
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + visible {
            self.scroll = self.sel + 1 - visible;
        }
        let max_scroll = self.defs().len().saturating_sub(visible);
        self.scroll = self.scroll.min(max_scroll);
    }

    // ── layout ───────────────────────────────────────────────────────────

    pub fn layout(&self, w: u32, h: u32) -> Layout {
        let s = self.metrics.scale;
        let (wf, hf) = (w as f32, h as f32);
        let pw = (wf * 0.92).min(940.0 * s).max(1.0);
        let ph = (hf * 0.88).min(640.0 * s).max(1.0);
        let (px, py) = ((wf - pw) / 2.0, (hf - ph) / 2.0);
        let title_h = (52.0 * s).min(ph / 4.0);
        let footer_h = (40.0 * s).min(ph / 4.0);
        let side_w = (200.0 * s).min(pw * 0.3);
        let pad = 8.0 * s;

        let tab_h = 40.0 * s;
        let tabs = (0..self.tabs.len())
            .map(|i| (i, (px + pad, py + title_h + i as f32 * tab_h, side_w - pad * 2.0, tab_h - 4.0 * s)))
            .collect();

        let (cx, cy) = (px + side_w, py + title_h);
        let (cw, ch) = (pw - side_w, ph - title_h - footer_h);
        let row_h = 56.0 * s;
        let visible = ((ch / row_h) as usize).max(1);
        let n = self.defs().len();
        let first = self.scroll.min(n.saturating_sub(visible));
        let rows = (first..n.min(first + visible))
            .map(|i| (i, (cx + pad, cy + (i - first) as f32 * row_h, cw - pad * 2.0, row_h - 2.0 * s)))
            .collect();

        Layout { panel: (px, py, pw, ph), tabs, rows, footer: (px, py + ph - footer_h, pw, footer_h), visible }
    }

    /// The part of a row that holds its value; clicks there act on the value.
    fn value_rect(&self, (x, y, w, h): Rect) -> Rect {
        let vw = (w * 0.45).min(300.0 * self.metrics.scale);
        (x + w - vw - self.metrics.pad, y + h * 0.15, vw, h * 0.7)
    }

    // ── drawing ──────────────────────────────────────────────────────────

    fn draw(&mut self, w: u32, h: u32) -> Vec<u8> {
        let Some(mut pm) = Pixmap::new(w.max(1), h.max(1)) else { return Vec::new() };
        let visible = self.layout(w, h).visible;
        self.ensure_visible(visible);
        let l = self.layout(w, h);
        let m = self.metrics;
        let t = self.theme.clone();

        fill_rect(&mut pm, 0.0, 0.0, w as f32, h as f32, Color::from_rgba(0.0, 0.0, 0.0, 0.32).unwrap_or(t.bg));
        let (px, py, pw, ph) = l.panel;
        fill_rounded_rect(&mut pm, px, py, pw, ph, m.px(14.0), t.surface);
        stroke_rounded_rect(&mut pm, px + 0.5, py + 0.5, pw - 1.0, ph - 1.0, m.px(14.0), t.border, 1.0);

        // Title.
        let title_size = m.font * 1.25;
        self.ts.draw(&mut pm, "Settings", px + m.px(20.0), py + m.px(16.0), Role::UI, title_size, t.text);
        let hint = "Esc to close";
        let (hw, _) = self.ts.measure(hint, Role::UI, m.small);
        self.ts.draw(&mut pm, hint, px + pw - hw - m.px(20.0), py + m.px(20.0), Role::UI, m.small, t.text_dim);

        // Sidebar.
        let side_x = l.tabs.first().map_or(px, |(_, r)| r.0 + r.2 + m.px(8.0));
        fill_rect(&mut pm, side_x, py + m.px(52.0), 1.0, ph - m.px(52.0) - l.footer.3, t.border);
        for (i, (x, y, tw, th)) in l.tabs.clone() {
            if i == self.tab {
                fill_rounded_rect(&mut pm, x, y, tw, th, m.px(10.0), t.surface_hi);
                fill_rounded_rect(&mut pm, x, y + th * 0.22, m.px(3.0), th * 0.56, m.px(1.5), t.accent);
            }
            let name = self.ts.ellipsize(self.tabs[i].name, Role::UI, m.font, tw - m.px(28.0));
            let color = if i == self.tab { t.text } else { t.text_dim };
            self.ts.draw(&mut pm, &name, x + m.px(14.0), y + (th - m.font) / 2.0, Role::UI, m.font, color);
        }

        // Rows.
        for (i, rect) in l.rows.clone() {
            self.draw_row(&mut pm, i, rect);
        }
        self.draw_scrollbar(&mut pm, &l);

        // Footer: the last result, else the keys.
        let (fx, fy, fw, fh) = l.footer;
        fill_rect(&mut pm, fx + m.px(1.0), fy, fw - m.px(2.0), 1.0, t.border);
        let (msg, color) = match &self.status {
            Status::Ok(s) => (s.clone(), t.ok),
            Status::Warn(s) => (s.clone(), t.warn),
            Status::Err(s) => (s.clone(), t.err),
            Status::None if self.editing.is_some() => ("Enter to save, Esc to cancel".to_string(), t.text_dim),
            Status::None => ("Up/Down select   Left/Right adjust   Enter edit   Del reset   Tab section".to_string(), t.text_dim),
        };
        let msg = self.ts.ellipsize(&msg, Role::UI, m.small, fw - m.px(40.0));
        self.ts.draw(&mut pm, &msg, fx + m.px(20.0), fy + (fh - m.small) / 2.0, Role::UI, m.small, color);

        pm.take()
    }

    fn draw_row(&mut self, pm: &mut Pixmap, i: usize, rect: Rect) {
        let m = self.metrics;
        let t = self.theme.clone();
        let (x, y, w, h) = rect;
        let Some(def) = self.defs().get(i).copied() else { return };
        let selected = i == self.sel;
        if selected {
            fill_rounded_rect(pm, x, y, w, h, m.px(10.0), t.surface_hi);
            fill_rounded_rect(pm, x, y + h * 0.22, m.px(3.0), h * 0.56, m.px(1.5), t.accent);
        }

        let (vx, vy, vw, vh) = self.value_rect(rect);
        let text_w = (vx - x - m.px(28.0)).max(1.0);
        let label = self.ts.ellipsize(&model::label(def.path), Role::UI, m.font, text_w);
        self.ts.draw(pm, &label, x + m.px(14.0), y + h * 0.16, Role::UI, m.font, t.text);
        let help = self.ts.ellipsize(def.help, Role::UI, m.small, text_w);
        self.ts.draw(pm, &help, x + m.px(14.0), y + h * 0.16 + m.font * 1.3, Role::UI, m.small, t.text_dim);

        let value = self.value_of(def);
        if selected && self.editing.is_some() {
            self.draw_editor(pm, (vx, vy, vw, vh));
            return;
        }
        match (def.kind, &value) {
            (Kind::Bool, Some(EditValue::Bool(on))) => {
                let (sw, sh) = (m.px(40.0), m.px(22.0));
                let (sx, sy) = (vx + vw - sw, vy + (vh - sh) / 2.0);
                fill_rounded_rect(pm, sx, sy, sw, sh, sh / 2.0, if *on { t.accent } else { t.border });
                let knob = sh - m.px(6.0);
                let kx = if *on { sx + sw - knob - m.px(3.0) } else { sx + m.px(3.0) };
                fill_circle(pm, kx + knob / 2.0, sy + sh / 2.0, knob / 2.0, if *on { t.accent_text } else { t.text });
            }
            (Kind::Choice(_), Some(v)) => {
                let text = format!("<  {}  >", model::show(v));
                let text = self.ts.ellipsize(&text, Role::UI, m.font, vw);
                let (tw, _) = self.ts.measure(&text, Role::UI, m.font);
                self.ts.draw(pm, &text, vx + vw - tw, vy + (vh - m.font) / 2.0, Role::UI, m.font, t.text);
            }
            _ => {
                fill_rounded_rect(pm, vx, vy, vw, vh, m.px(8.0), t.bg);
                let (text, color) = match &value {
                    Some(v) => (model::show(v), t.text),
                    None => ("(not set)".to_string(), t.text_dim),
                };
                let mut inner = vw - m.px(20.0);
                if matches!(def.kind, Kind::Color) {
                    if let Some(c) = parse_swatch(&text) {
                        let sz = vh - m.px(12.0);
                        fill_rounded_rect(pm, vx + m.px(8.0), vy + m.px(6.0), sz, sz, m.px(4.0), c);
                        inner -= sz + m.px(8.0);
                        let text = self.ts.ellipsize(&text, Role::UI, m.font, inner);
                        self.ts.draw(pm, &text, vx + m.px(16.0) + sz, vy + (vh - m.font) / 2.0, Role::UI, m.font, color);
                        return;
                    }
                }
                let text = self.ts.ellipsize(&text, Role::UI, m.font, inner);
                self.ts.draw(pm, &text, vx + m.px(10.0), vy + (vh - m.font) / 2.0, Role::UI, m.font, color);
            }
        }
    }

    fn draw_editor(&mut self, pm: &mut Pixmap, (x, y, w, h): Rect) {
        let m = self.metrics;
        let t = self.theme.clone();
        fill_rounded_rect(pm, x, y, w, h, m.px(8.0), t.bg);
        stroke_rounded_rect(pm, x + 0.5, y + 0.5, w - 1.0, h - 1.0, m.px(8.0), t.accent, 1.5);
        let Some(edit) = self.editing.as_ref() else { return };
        let (text, cursor) = (edit.text().to_string(), edit.cursor());
        let inner = w - m.px(20.0);
        // Show the tail of text that is wider than the field.
        let mut start = 0;
        while start < text.len() && self.ts.measure(&text[start..], Role::UI, m.font).0 > inner - m.px(4.0) {
            start += text[start..].chars().next().map_or(1, char::len_utf8);
        }
        let cursor = cursor.max(start);
        let ty = y + (h - m.font) / 2.0;
        self.ts.draw(pm, &text[start..], x + m.px(10.0), ty, Role::UI, m.font, t.text);
        if self.blink_on {
            let cx = x + m.px(10.0) + self.ts.measure(&text[start..cursor], Role::UI, m.font).0;
            fill_rect(pm, cx, ty - m.px(1.0), m.px(2.0).max(1.0), m.font + m.px(2.0), t.accent);
        }
    }

    fn draw_scrollbar(&mut self, pm: &mut Pixmap, l: &Layout) {
        let n = self.defs().len();
        if n <= l.visible || l.rows.is_empty() {
            return;
        }
        let m = self.metrics;
        let (first_y, last) = (l.rows[0].1 .1, l.rows[l.rows.len() - 1].1);
        let track_h = last.1 + last.3 - first_y;
        let thumb_h = (track_h * l.visible as f32 / n as f32).max(m.px(24.0));
        let first = l.rows[0].0;
        let thumb_y = first_y + (track_h - thumb_h) * first as f32 / (n - l.visible) as f32;
        let x = l.panel.0 + l.panel.2 - m.px(8.0);
        fill_rounded_rect(pm, x, thumb_y, m.px(3.0), thumb_h, m.px(1.5), self.theme.border);
    }
}

fn parse_swatch(text: &str) -> Option<Color> {
    let [r, g, b] = veil_config::parse_hex_color(text)?;
    Some(Color::from_rgba8(r, g, b, 255))
}

impl ModalApp for SettingsApp {
    fn render(&mut self, w: u32, h: u32) -> Vec<u8> {
        self.size = (w, h);
        self.draw(w, h)
    }

    /// Blur only behind the panel. The default is the whole surface, and the
    /// compositor widens its damage to every blurred footprint it touches, so
    /// a full-screen region turns every cursor move anywhere into a
    /// full-screen recomposite and blur.
    fn blur(&self, w: u32, h: u32) -> Vec<(i32, i32, i32, i32)> {
        let (x, y, pw, ph) = self.layout(w, h).panel;
        // Round the edges outwards, not the size: x + width must still reach
        // the panel's right edge when x is fractional.
        let (x0, y0) = (x.floor() as i32, y.floor() as i32);
        vec![(x0, y0, (x + pw).ceil() as i32 - x0, (y + ph).ceil() as i32 - y0)]
    }

    fn key(&mut self, key: &Key) -> Flow {
        self.blink_epoch = Instant::now();
        self.blink_on = true;
        let ctrl = key.ctrl;

        if self.editing.is_some() {
            return self.edit_key(key);
        }
        match key.sym {
            Keysym::Escape => {
                self.flush();
                Flow::Close
            }
            Keysym::Up => self.move_sel(-1),
            Keysym::Down => self.move_sel(1),
            Keysym::Page_Up if ctrl => self.switch_tab(-1),
            Keysym::Page_Down if ctrl => self.switch_tab(1),
            Keysym::Page_Up => self.move_sel(-5),
            Keysym::Page_Down => self.move_sel(5),
            Keysym::Home => self.move_sel(isize::MIN / 2),
            Keysym::End => self.move_sel(isize::MAX / 2),
            Keysym::ISO_Left_Tab => self.switch_tab(-1),
            Keysym::Tab if key.shift => self.switch_tab(-1),
            Keysym::Tab => self.switch_tab(1),
            Keysym::Left => self.adjust(-1),
            Keysym::Right => self.adjust(1),
            Keysym::Return | Keysym::KP_Enter | Keysym::space => self.activate(),
            Keysym::Delete | Keysym::BackSpace => self.reset(),
            _ => Flow::Idle,
        }
    }

    fn pointer(&mut self, ev: Pointer) -> Flow {
        let (w, h) = self.last_size();
        let l = self.layout(w, h);
        match ev {
            Pointer::Move { x, y } => {
                if self.editing.is_some() {
                    return Flow::Idle;
                }
                match l.rows.iter().find(|(_, r)| inside(*r, x, y)) {
                    Some((i, _)) if *i != self.sel => {
                        self.flush();
                        self.sel = *i;
                        Flow::Redraw
                    }
                    _ => Flow::Idle,
                }
            }
            Pointer::Click { x, y } => {
                if !inside(l.panel, x, y) {
                    self.flush();
                    return Flow::Close;
                }
                if let Some((i, _)) = l.tabs.iter().find(|(_, r)| inside(*r, x, y)) {
                    return self.set_tab(*i);
                }
                if let Some((i, rect)) = l.rows.iter().find(|(_, r)| inside(*r, x, y)).copied() {
                    if self.editing.is_some() && i != self.sel {
                        self.editing = None;
                    }
                    self.flush();
                    self.sel = i;
                    if inside(self.value_rect(rect), x, y) {
                        return self.activate();
                    }
                    return Flow::Redraw;
                }
                Flow::Idle
            }
            Pointer::Scroll { dy } => {
                let n = self.defs().len();
                let max_scroll = n.saturating_sub(l.visible);
                let step = (dy / (56.0 * self.metrics.scale)).round() as isize;
                let step = if step == 0 { dy.signum() as isize } else { step };
                let next = (self.scroll as isize + step).clamp(0, max_scroll as isize) as usize;
                if next == self.scroll {
                    return Flow::Idle;
                }
                self.scroll = next;
                Flow::Redraw
            }
            Pointer::Leave => Flow::Idle,
        }
    }

    fn next_tick(&self, now: Instant) -> Option<Duration> {
        let pending = self.pending.as_ref().map(|p| p.due.saturating_duration_since(now));
        let blink = self.editing.is_some().then(|| {
            let phase = now.saturating_duration_since(self.blink_epoch).as_millis() % (BLINK.as_millis() * 2);
            BLINK.saturating_sub(Duration::from_millis((phase % BLINK.as_millis()) as u64))
        });
        match (pending, blink) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn tick(&mut self, now: Instant) -> Flow {
        let mut redraw = Flow::Idle;
        if self.pending.as_ref().is_some_and(|p| now >= p.due) {
            self.flush();
            redraw = Flow::Redraw;
        }
        if self.editing.is_some() {
            let on = (now.saturating_duration_since(self.blink_epoch).as_millis() / BLINK.as_millis()).is_multiple_of(2);
            if on != self.blink_on {
                self.blink_on = on;
                redraw = Flow::Redraw;
            }
        }
        redraw
    }
}

impl SettingsApp {
    fn edit_key(&mut self, key: &Key) -> Flow {
        let ctrl = key.ctrl;
        let Some(edit) = self.editing.as_mut() else { return Flow::Idle };
        let changed = match key.sym {
            Keysym::Escape => {
                self.editing = None;
                self.status = Status::None;
                return Flow::Redraw;
            }
            Keysym::Return | Keysym::KP_Enter => return self.finish_edit(),
            Keysym::Left if ctrl => edit.word_left(),
            Keysym::Right if ctrl => edit.word_right(),
            Keysym::Left => edit.left(),
            Keysym::Right => edit.right(),
            Keysym::Home => edit.home(),
            Keysym::End => edit.end(),
            Keysym::BackSpace if ctrl => edit.delete_word_back(),
            Keysym::BackSpace => edit.backspace(),
            Keysym::Delete => edit.delete(),
            Keysym::a if ctrl => edit.home(),
            Keysym::e if ctrl => edit.end(),
            Keysym::u if ctrl => edit.clear(),
            Keysym::w if ctrl => edit.delete_word_back(),
            _ => match &key.text {
                Some(t) if !ctrl && !key.alt => {
                    let before = edit.text().len();
                    edit.insert(t);
                    edit.text().len() != before
                }
                _ => false,
            },
        };
        if changed {
            self.status = Status::None;
        }
        Flow::Redraw
    }

    /// The size of the last frame, for hit-testing pointer events. Before the
    /// first frame there is nothing to hit.
    fn last_size(&self) -> (u32, u32) {
        self.size
    }
}

fn inside((x, y, w, h): Rect, px: f32, py: f32) -> bool {
    px >= x && px < x + w && py >= y && py < y + h
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// An in-memory config.toml behind a real `config.set`: the same parser,
    /// schema and writer the compositor uses, so what these tests save is what
    /// would land in the file.
    struct Harness {
        app: SettingsApp,
        file: Rc<RefCell<String>>,
        sent: Rc<RefCell<Vec<Value>>>,
        fail_with: Rc<RefCell<Option<String>>>,
        shadow: Rc<RefCell<Option<String>>>,
    }

    fn harness(src: &str) -> Harness {
        let file = Rc::new(RefCell::new(src.to_string()));
        let sent = Rc::new(RefCell::new(Vec::new()));
        let fail_with = Rc::new(RefCell::new(None::<String>));
        let shadow = Rc::new(RefCell::new(None::<String>));
        let (f, s, e, sh) = (file.clone(), sent.clone(), fail_with.clone(), shadow.clone());
        let sender: Sender = Box::new(move |args| {
            s.borrow_mut().push(args.clone());
            if let Some(err) = e.borrow().clone() {
                return Err(err);
            }
            let edits = veil_config::wire::edits_from_args(args)?;
            let out = veil_config::writer::apply_edits(&f.borrow(), &edits).map_err(|e| e.to_string())?;
            *f.borrow_mut() = out;
            Ok(Some(json!({ "path": "/test/config.toml", "shadowed_by": sh.borrow().clone() })))
        });
        let f2 = file.clone();
        let loader: Loader = Box::new(move || veil_config::parse_config(&f2.borrow()).expect("valid"));
        let cfg = veil_config::parse_config(src).expect("valid test config");
        let mut app = SettingsApp::new(cfg, Tokens::default(), None, sender, loader);
        app.size = (1280, 800); // as if a frame had been drawn: pointer events are hit-tested against it
        Harness { app, file, sent, fail_with, shadow }
    }

    impl Harness {
        fn key(&mut self, sym: Keysym) -> Flow {
            self.app.key(&Key { sym, text: None, ctrl: false, alt: false, shift: false, repeat: false })
        }
        fn type_text(&mut self, text: &str) {
            for ch in text.chars() {
                let sym = Keysym::from_char(ch);
                self.app.key(&Key { sym, text: Some(ch.to_string()), ctrl: false, alt: false, shift: false, repeat: false });
            }
        }
        /// Go to the tab holding `path` and select its row.
        fn select(&mut self, path: &str) {
            let tab = self.app.tabs.iter().position(|t| t.defs.iter().any(|d| d.path == path)).expect("a tab has it");
            self.app.set_tab(tab);
            self.app.sel = self.app.defs().iter().position(|d| d.path == path).unwrap();
        }
        fn settle(&mut self) {
            self.app.tick(Instant::now() + Duration::from_secs(5));
        }
        fn file(&self) -> String {
            self.file.borrow().clone()
        }
        fn sent(&self) -> Vec<Value> {
            self.sent.borrow().clone()
        }
    }

    #[test]
    fn starts_on_the_first_tab_and_row() {
        let h = harness("");
        assert_eq!(h.app.tab_name(), "Wallpaper and theme");
        assert_eq!(h.app.selected_path(), Some("theme"));
        assert_eq!(h.app.status(), &Status::None);
        assert!(!h.app.is_editing());
    }

    #[test]
    fn space_flips_a_switch_and_saves_it_at_once() {
        let mut h = harness("");
        h.select("bar.enabled");
        assert_eq!(h.key(Keysym::space), Flow::Redraw);
        assert_eq!(h.sent(), vec![json!({ "set": { "bar.enabled": false } })]);
        assert!(h.file().contains("enabled = false"), "{}", h.file());
        assert_eq!(h.app.status(), &Status::Ok("Saved".into()));
        // The window now shows what the file holds.
        assert_eq!(h.app.value_of(h.app.cur_def().unwrap()), Some(EditValue::Bool(false)));
    }

    #[test]
    fn enter_cycles_a_choice_and_left_right_queue_it() {
        let mut h = harness("");
        h.select("theme");
        h.key(Keysym::Return);
        assert!(h.file().contains("theme = \"nord\""), "{}", h.file());
        h.key(Keysym::Right); // queued, not saved yet
        assert_eq!(h.sent().len(), 1);
        h.settle();
        assert_eq!(h.sent().len(), 2);
        assert!(h.file().contains("theme = \"dracula\""), "{}", h.file());
    }

    #[test]
    fn holding_a_key_saves_once_after_it_settles() {
        let mut h = harness("[bar]\nheight = 40\n");
        h.select("bar.height");
        for _ in 0..10 {
            h.key(Keysym::Right);
        }
        assert!(h.sent().is_empty(), "nothing written while the key is held");
        // The row already shows the new value (not the stale file value).
        assert_eq!(h.app.value_of(h.app.cur_def().unwrap()), Some(EditValue::Int(50)));
        assert!(h.app.next_tick(Instant::now()).is_some(), "a save is scheduled");
        // Not due yet: a tick right away changes nothing.
        assert_eq!(h.app.tick(Instant::now()), Flow::Idle);
        h.settle();
        assert_eq!(h.sent(), vec![json!({ "set": { "bar.height": 50 } })]);
        assert!(h.file().contains("height = 50"), "{}", h.file());
        assert_eq!(h.app.next_tick(Instant::now()), None);
    }

    #[test]
    fn a_nudge_stops_at_the_limit_without_saving() {
        let mut h = harness("[bar]\nheight = 64\n");
        h.select("bar.height");
        assert_eq!(h.key(Keysym::Right), Flow::Idle);
        h.settle();
        assert!(h.sent().is_empty());
    }

    #[test]
    fn leaving_a_row_or_tab_or_the_window_saves_what_is_waiting() {
        let mut h = harness("[bar]\nheight = 40\nposition = \"top\"\n");
        h.select("bar.height");
        h.key(Keysym::Right);
        h.key(Keysym::Down);
        assert_eq!(h.sent().len(), 1, "moving rows flushes");

        h.select("bar.position");
        h.key(Keysym::Left); // a nudge on a choice row
        assert_eq!(h.sent().len(), 1, "queued, not saved yet");
        h.key(Keysym::Tab);
        assert_eq!(h.sent().len(), 2, "changing tab flushes it");
        assert!(h.file().contains("position = \"bottom\""), "wrapped from top: {}", h.file());

        let mut h = harness("[bar]\nheight = 40\n");
        h.select("bar.height");
        h.key(Keysym::Right);
        assert_eq!(h.key(Keysym::Escape), Flow::Close);
        assert!(h.file().contains("height = 41"), "closing saves: {}", h.file());
    }

    #[test]
    fn typing_a_value_saves_it_on_enter() {
        let mut h = harness("");
        h.select("keyboard.layout");
        h.key(Keysym::Return);
        assert!(h.app.is_editing());
        h.type_text("de");
        assert!(h.sent().is_empty(), "nothing saved while typing");
        h.key(Keysym::Return);
        assert!(!h.app.is_editing());
        assert_eq!(h.sent(), vec![json!({ "set": { "keyboard.layout": "de" } })]);
        assert!(h.file().contains("layout = \"de\""), "{}", h.file());
    }

    #[test]
    fn editing_starts_from_the_current_value() {
        let mut h = harness("[keyboard]\nlayout = \"us\"\n");
        h.select("keyboard.layout");
        h.key(Keysym::Return);
        h.type_text("x");
        h.key(Keysym::Return);
        assert!(h.file().contains("layout = \"usx\""), "{}", h.file());
    }

    #[test]
    fn a_bad_number_stays_in_the_field_and_says_why_without_saving() {
        let mut h = harness("");
        h.select("fps");
        h.key(Keysym::Return);
        h.key(Keysym::u); // no ctrl: types a letter
        h.app.editing.as_mut().unwrap().clear();
        h.type_text("abc");
        h.key(Keysym::Return);
        assert!(h.app.is_editing(), "stay in the field to fix it");
        assert_eq!(h.app.status(), &Status::Err("type a whole number".into()));
        assert!(h.sent().is_empty());
        h.key(Keysym::Escape);
        assert!(!h.app.is_editing());
        assert_eq!(h.key(Keysym::Escape), Flow::Close, "second Escape closes");
    }

    #[test]
    fn an_out_of_range_value_is_refused_before_it_is_sent() {
        let mut h = harness("");
        h.select("bar.height");
        h.key(Keysym::Return);
        h.app.editing.as_mut().unwrap().clear();
        h.type_text("9999");
        h.key(Keysym::Return);
        assert_eq!(h.app.status(), &Status::Err("`bar.height` must be 24 to 64".into()));
        assert!(h.sent().is_empty());
    }

    #[test]
    fn delete_resets_a_setting_to_its_default() {
        let mut h = harness("# mine\n[bar]\nheight = 50\nposition = \"top\"\n");
        h.select("bar.height");
        h.key(Keysym::Delete);
        assert_eq!(h.sent(), vec![json!({ "unset": ["bar.height"] })]);
        assert!(!h.file().contains("height"), "{}", h.file());
        assert!(h.file().contains("# mine") && h.file().contains("position = \"top\""), "{}", h.file());
        assert_eq!(h.app.status(), &Status::Ok("Reset to default".into()));
    }

    #[test]
    fn clearing_a_text_field_resets_it_instead_of_saving_an_empty_string() {
        let mut h = harness("[font]\nfamily = \"Inter\"\n");
        h.select("font.family");
        h.key(Keysym::Return);
        h.app.editing.as_mut().unwrap().clear();
        h.key(Keysym::Return);
        assert_eq!(h.sent(), vec![json!({ "unset": ["font.family"] })]);
        assert!(!h.file().contains("Inter"), "{}", h.file());
    }

    #[test]
    fn a_list_is_edited_as_comma_separated_text() {
        let mut h = harness("");
        h.select("bar.left");
        h.key(Keysym::Return);
        h.app.editing.as_mut().unwrap().clear();
        h.type_text("launcher, clock");
        h.key(Keysym::Return);
        assert_eq!(h.sent(), vec![json!({ "set": { "bar.left": ["launcher", "clock"] } })]);
    }

    #[test]
    fn saving_wallpaper_paths_replaces_the_single_path_spelling() {
        let mut h = harness("wallpaper = \"/old.png\"\n");
        h.select("wallpaper.paths");
        h.key(Keysym::Return);
        h.app.editing.as_mut().unwrap().clear();
        h.type_text("/new.png");
        h.key(Keysym::Return);
        let cfg = veil_config::parse_config(&h.file()).unwrap();
        let wp = cfg.wallpaper.expect("still configured");
        assert_eq!(wp.paths, vec![std::path::PathBuf::from("/new.png")], "{}", h.file());
    }

    #[test]
    fn weather_coordinates_start_from_zero_when_unset() {
        let mut h = harness("");
        h.select("bar.widgets.weather.lat");
        assert_eq!(h.app.value_of(h.app.cur_def().unwrap()), None, "shown as (not set)");
        h.key(Keysym::Right);
        h.settle();
        assert_eq!(h.sent(), vec![json!({ "set": { "bar.widgets.weather.lat": 1.0 } })]);
    }

    #[test]
    fn a_failed_save_is_shown_and_changes_nothing() {
        let mut h = harness("");
        *h.fail_with.borrow_mut() = Some("the compositor did not answer".into());
        h.select("bar.enabled");
        h.key(Keysym::space);
        assert_eq!(h.app.status(), &Status::Err("the compositor did not answer".into()));
        assert_eq!(h.file(), "");
    }

    #[test]
    fn a_shadowing_config_file_is_called_out() {
        let mut h = harness("");
        *h.shadow.borrow_mut() = Some("./config.toml".into());
        h.select("bar.enabled");
        h.key(Keysym::space);
        assert_eq!(h.app.status(), &Status::Warn("Saved, but Veil is reading ./config.toml instead".into()));
    }

    #[test]
    fn navigation_clamps_and_tabs_wrap() {
        let mut h = harness("");
        assert_eq!(h.key(Keysym::Up), Flow::Idle, "already at the top");
        h.key(Keysym::End);
        assert_eq!(h.app.sel, h.app.defs().len() - 1);
        h.key(Keysym::Home);
        assert_eq!(h.app.sel, 0);
        let n = h.app.tabs.len();
        h.key(Keysym::ISO_Left_Tab);
        assert_eq!(h.app.tab, n - 1, "Shift+Tab wraps to the last tab");
        h.key(Keysym::Tab);
        assert_eq!(h.app.tab, 0);
        assert_eq!(h.app.sel, 0, "a new tab starts at its first row");
    }

    #[test]
    fn the_pointer_selects_toggles_switches_tabs_and_closes() {
        let mut h = harness("");
        h.select("bar.enabled");
        h.app.render(1280, 800);
        let l = h.app.layout(1280, 800);

        // Click the Bar tab by name.
        let bar = h.app.tabs.iter().position(|t| t.name == "Bar").unwrap();
        let (_, (tx, ty, tw, th)) = l.tabs.iter().find(|(i, _)| *i == bar).copied().unwrap();
        h.app.set_tab(0);
        assert_eq!(h.app.pointer(Pointer::Click { x: tx + tw / 2.0, y: ty + th / 2.0 }), Flow::Redraw);
        assert_eq!(h.app.tab_name(), "Bar");

        // Click the value area of the first row (bar.enabled, a switch): it flips.
        let l = h.app.layout(1280, 800);
        let (_, rect) = l.rows[0];
        let (vx, vy, vw, vh) = h.app.value_rect(rect);
        assert_eq!(h.app.selected_path(), Some("bar.enabled"));
        h.app.pointer(Pointer::Click { x: vx + vw - 10.0, y: vy + vh / 2.0 });
        assert!(h.file().contains("enabled = false"), "{}", h.file());

        // Hovering another row selects it.
        let (_, r2) = l.rows[1];
        assert_eq!(h.app.pointer(Pointer::Move { x: r2.0 + 20.0, y: r2.1 + r2.3 / 2.0 }), Flow::Redraw);
        assert_eq!(h.app.selected_path(), Some("bar.position"));

        // Outside the panel closes.
        assert_eq!(h.app.pointer(Pointer::Click { x: 1.0, y: 1.0 }), Flow::Close);
    }

    #[test]
    fn the_wheel_scrolls_a_long_tab_and_keeps_the_selection_reachable() {
        let mut h = harness("");
        h.app.render(1280, 420); // short window: fewer rows fit
        let bar = h.app.tabs.iter().position(|t| t.name == "Bar").unwrap();
        h.app.set_tab(bar);
        h.app.render(1280, 420);
        let visible = h.app.layout(1280, 420).visible;
        assert!(visible < h.app.defs().len(), "this tab needs scrolling in a short window");
        assert_eq!(h.app.pointer(Pointer::Scroll { dy: 120.0 }), Flow::Redraw);
        assert!(h.app.scroll > 0);
        h.key(Keysym::End);
        h.app.render(1280, 420);
        let l = h.app.layout(1280, 420);
        assert!(l.rows.iter().any(|(i, _)| *i == h.app.sel), "the selected row is on screen");
        h.app.pointer(Pointer::Scroll { dy: -100000.0 });
        assert_eq!(h.app.scroll, 0);
    }

    #[test]
    fn drawing_fills_the_surface_and_survives_awkward_sizes_and_values() {
        let mut h = harness("[font]\nfamily = \"Ünï ✓ 😀 very long family name that goes on and on and on\"\n");
        for (w, hh) in [(640u32, 400u32), (320, 200), (50, 50), (1, 1)] {
            for tab in 0..h.app.tabs.len() {
                h.app.set_tab(tab);
                let px = h.app.render(w, hh);
                assert_eq!(px.len(), (w * hh * 4) as usize);
                let _ = h.app.pointer(Pointer::Click { x: 10.0, y: 10.0 });
                h.app.set_tab(tab); // a click outside may have asked to close; keep going
            }
        }
        h.select("keyboard.layout");
        h.key(Keysym::Return);
        h.type_text(&"é✓".repeat(60));
        let px = h.app.render(1280, 800);
        assert_eq!(px.len(), 1280 * 800 * 4);
        // A very large surface once, not for every tab: it is slow to fill.
        assert_eq!(h.app.render(2560, 1440).len(), 2560 * 1440 * 4);
    }

    #[test]
    fn the_panel_is_opaque_over_a_dimmed_scrim() {
        let mut h = harness("");
        let (w, hh) = (1280u32, 800u32);
        let px = h.app.render(w, hh);
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        let corner = at(2, 2);
        assert!(corner[3] > 0 && corner[3] < 255, "scrim, not opaque: {corner:?}");
        let (x, y, pw, ph) = h.app.layout(w, hh).panel;
        let inside = at((x + pw / 2.0) as u32, (y + ph - 4.0) as u32);
        assert_eq!(inside[3], 255);
    }

    #[test]
    fn a_switch_looks_different_on_and_off() {
        let mut h = harness("");
        h.select("bar.enabled");
        let before = h.app.render(1280, 800);
        h.key(Keysym::space);
        let after = h.app.render(1280, 800);
        assert_ne!(before, after);
    }

    #[test]
    fn the_blink_runs_only_while_editing() {
        let mut h = harness("");
        assert_eq!(h.app.next_tick(Instant::now()), None);
        h.select("keyboard.layout");
        h.key(Keysym::Return);
        assert!(h.app.next_tick(Instant::now()).is_some());
        let later = Instant::now() + BLINK + Duration::from_millis(50);
        assert_eq!(h.app.tick(later), Flow::Redraw, "the cursor toggled");
    }

    #[test]
    fn blur_covers_the_panel_only_not_the_whole_screen() {
        let h = harness("");
        for (w, hh) in [(1920u32, 1200u32), (640, 400), (100, 100)] {
            let rects = h.app.blur(w, hh);
            assert_eq!(rects.len(), 1);
            let (bx, by, bw, bh) = rects[0];
            let (px, py, pw, ph) = h.app.layout(w, hh).panel;
            assert!(bx as f32 <= px && by as f32 <= py, "covers the panel's top-left");
            assert!((bx + bw) as f32 >= px + pw - 0.01 && (by + bh) as f32 >= py + ph - 0.01, "covers the panel's bottom-right");
            if w >= 1000 {
                assert!(i64::from(bw) * i64::from(bh) < i64::from(w) * i64::from(hh) / 2, "far less than the full surface at {w}x{hh}");
            }
        }
    }

    #[test]
    fn every_setting_the_window_lists_can_be_selected_and_drawn() {
        let mut h = harness("");
        for tab in 0..h.app.tabs.len() {
            h.app.set_tab(tab);
            for _ in 0..h.app.defs().len() {
                let px = h.app.render(640, 360);
                assert_eq!(px.len(), 640 * 360 * 4);
                h.key(Keysym::Down);
            }
        }
    }
}
