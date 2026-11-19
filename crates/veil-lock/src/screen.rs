//! The lock screen's state and drawing. No Wayland in here: keys go in,
//! pixels and actions come out.
//!
//! The compositor dims the wallpaper behind this surface, so the screen is
//! drawn on a transparent canvas: clock, account, and the password field.

use std::time::{Duration, Instant};

use tiny_skia::{Color, FillRule, FilterQuality, Paint, PathBuilder, Pattern, Pixmap, SpreadMode, Transform};
use veil_ui::modal::Key;
use veil_ui::{fill_circle, fill_rounded_rect, stroke_circle, stroke_rounded_rect, FontConfig, Keysym, Role, TextSystem, Theme};
use zeroize::Zeroizing;

use crate::battery::{Battery, Reading};

/// How long a failure message stays up.
const FAILURE_SHOWN: Duration = Duration::from_secs(3);
/// Longest password taken; more is almost certainly a stuck key.
const MAX_PASSWORD: usize = 512;
/// Dots drawn in the field before it just stays full.
const MAX_DOTS: usize = 24;

#[derive(Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Verifying,
    Failed { until: Instant },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    /// Check this password. Input stays frozen until `verified` or `failed`.
    Submit(Zeroizing<String>),
    /// Run a power action. The session stays locked; the compositor does the rest.
    Power(PowerAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Suspend,
    Reboot,
    PowerOff,
}

impl PowerAction {
    const ALL: [PowerAction; 3] = [PowerAction::Suspend, PowerAction::Reboot, PowerAction::PowerOff];

    fn label(self) -> &'static str {
        match self {
            PowerAction::Suspend => "Suspend",
            PowerAction::Reboot => "Reboot",
            PowerAction::PowerOff => "Power off",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            PowerAction::Suspend => "suspend",
            PowerAction::Reboot => "reboot",
            PowerAction::PowerOff => "power off",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            PowerAction::Suspend => "\u{f04b2}", // nf-md-sleep
            PowerAction::Reboot => "\u{f0709}",  // nf-md-restart
            PowerAction::PowerOff => "\u{f0425}", // nf-md-power
        }
    }

    /// Suspend is harmless on a locked screen; the others take the session
    /// down, so Enter has to be pressed twice.
    fn needs_confirm(self) -> bool {
        self != PowerAction::Suspend
    }

    /// The compositor command (the same ones the power menu sends).
    pub fn ipc_cmd(self) -> &'static str {
        match self {
            PowerAction::Suspend => "session.suspend",
            PowerAction::Reboot => "session.reboot",
            PowerAction::PowerOff => "session.poweroff",
        }
    }

    /// The `systemctl` verb, for when there is no compositor to ask.
    pub fn systemctl(self) -> &'static str {
        match self {
            PowerAction::Suspend => "suspend",
            PowerAction::Reboot => "reboot",
            PowerAction::PowerOff => "poweroff",
        }
    }
}

/// The open power menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Menu {
    sel: usize,
    /// Enter was pressed once on a destructive action; the next Enter does it.
    armed: bool,
}

/// Enter on the selected action: Suspend goes at once, the rest arm first.
fn press(m: &mut Menu) -> Option<PowerAction> {
    let a = PowerAction::ALL[m.sel];
    if a.needs_confirm() && !m.armed {
        m.armed = true;
        None
    } else {
        Some(a)
    }
}

pub struct LockScreen {
    theme: Theme,
    text: TextSystem,
    user: String,
    password: Zeroizing<String>,
    phase: Phase,
    message: Option<String>,
    /// Profile picture, square, premultiplied; `None` shows the initial.
    avatar: Option<Pixmap>,
    battery: Battery,
    /// The power menu, when open (Tab).
    menu: Option<Menu>,
    caps: bool,
}

impl LockScreen {
    pub fn new(tokens: &veil_palette::Tokens, font: Option<&FontConfig>, user: &str, avatar: Option<Pixmap>) -> Self {
        let text = match font {
            Some(cfg) => TextSystem::new(cfg),
            None => TextSystem::new(&FontConfig::default()),
        };
        LockScreen {
            theme: Theme::from_palette(tokens),
            text,
            user: user.to_string(),
            password: Zeroizing::new(String::new()),
            phase: Phase::Idle,
            message: None,
            avatar,
            battery: Battery::new(Instant::now()),
            menu: None,
            caps: false,
        }
    }

    /// The profile picture arrived after the screen was up.
    pub fn set_avatar(&mut self, avatar: Pixmap) {
        self.avatar = Some(avatar);
    }

    pub fn is_verifying(&self) -> bool {
        self.phase == Phase::Verifying
    }

    /// True while something on screen changes without input (a failure
    /// message that times out, the verifying state).
    pub fn is_animating(&self) -> bool {
        self.phase != Phase::Idle
    }

    pub fn key(&mut self, key: &Key) -> Action {
        if self.phase == Phase::Verifying {
            return Action::None;
        }
        if self.menu.is_some() {
            return self.menu_key(key);
        }
        // Typing after a failure starts over.
        if matches!(self.phase, Phase::Failed { .. }) {
            self.phase = Phase::Idle;
            self.message = None;
        }
        match key.sym {
            Keysym::Tab | Keysym::ISO_Left_Tab => {
                if !key.repeat {
                    self.menu = Some(Menu { sel: 0, armed: false });
                }
                Action::Redraw
            }
            Keysym::Return | Keysym::KP_Enter => {
                if self.password.is_empty() {
                    return Action::None;
                }
                self.phase = Phase::Verifying;
                self.message = None;
                Action::Submit(Zeroizing::new(self.password.as_str().to_owned()))
            }
            Keysym::BackSpace => {
                self.password.pop();
                Action::Redraw
            }
            Keysym::Escape => {
                self.clear();
                Action::Redraw
            }
            _ if key.ctrl && key.text.as_deref() == Some("u") => {
                self.clear();
                Action::Redraw
            }
            _ => match &key.text {
                Some(t) if !key.ctrl && !key.alt => {
                    for ch in t.chars() {
                        if self.password.chars().count() < MAX_PASSWORD {
                            self.password.push(ch);
                        }
                    }
                    Action::Redraw
                }
                _ => Action::None,
            },
        }
    }

    fn clear(&mut self) {
        // Zeroizing wipes the old buffer on drop; replace rather than truncate
        // so the previous contents do not linger in the allocation.
        self.password = Zeroizing::new(String::new());
    }

    /// Keys while the power menu is open. Nothing here reaches the password.
    fn menu_key(&mut self, key: &Key) -> Action {
        let Some(mut m) = self.menu else { return Action::None };
        let n = PowerAction::ALL.len();
        let text = if key.ctrl || key.alt { None } else { key.text.as_deref() };
        let mut go = None;
        match key.sym {
            // Back out of a pending confirm first, then close.
            Keysym::Escape | Keysym::Tab | Keysym::ISO_Left_Tab => {
                if key.repeat {
                    return Action::None;
                }
                if m.armed {
                    m.armed = false;
                } else {
                    self.menu = None;
                    return Action::Redraw;
                }
            }
            Keysym::Left | Keysym::KP_Left => (m.sel, m.armed) = ((m.sel + n - 1) % n, false),
            Keysym::Right | Keysym::KP_Right => (m.sel, m.armed) = ((m.sel + 1) % n, false),
            // A held Enter must never fire a reboot.
            Keysym::Return | Keysym::KP_Enter if key.repeat => return Action::None,
            Keysym::Return | Keysym::KP_Enter => go = press(&mut m),
            _ => match text {
                Some("h") => (m.sel, m.armed) = ((m.sel + n - 1) % n, false),
                Some("l") => (m.sel, m.armed) = ((m.sel + 1) % n, false),
                Some(d @ ("1" | "2" | "3")) => {
                    let i = d.as_bytes()[0] as usize - b'1' as usize;
                    if i != m.sel {
                        (m.sel, m.armed) = (i, false);
                    }
                    go = press(&mut m);
                }
                // Any other key cancels a pending confirm.
                _ if m.armed => m.armed = false,
                _ => return Action::None,
            },
        }
        if let Some(a) = go {
            self.menu = None;
            return Action::Power(a);
        }
        self.menu = Some(m);
        Action::Redraw
    }

    /// Caps Lock changed. True when the screen did.
    pub fn set_caps(&mut self, on: bool) -> bool {
        std::mem::replace(&mut self.caps, on) != on
    }

    /// The password was wrong (or PAM refused): clear the field, say why.
    pub fn failed(&mut self, message: String, now: Instant) {
        self.clear();
        self.message = Some(message);
        self.phase = Phase::Failed { until: now + FAILURE_SHOWN };
    }

    /// The failure message times out. True when the screen changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = self.battery.poll(now);
        if let Phase::Failed { until } = self.phase {
            if now >= until {
                self.phase = Phase::Idle;
                self.message = None;
                changed = true;
            }
        }
        changed
    }

    /// Premultiplied RGBA8, exactly `w * h * 4` bytes.
    pub fn render(&mut self, w: u32, h: u32) -> Vec<u8> {
        let Some(mut pm) = Pixmap::new(w.max(1), h.max(1)) else { return vec![0; (w * h * 4) as usize] };
        let s = (h as f32 / 1080.0).clamp(0.6, 2.5);
        let cx = w as f32 / 2.0;
        let t = &self.theme;

        // Clock and date, a little above the middle.
        let now = chrono::Local::now();
        let clock = now.format("%H:%M").to_string();
        let date = now.format("%A, %B %-d").to_string();
        let clock_size = 120.0 * s;
        let (cw, ch) = self.text.measure(&clock, Role::UI, clock_size);
        let top = h as f32 * 0.22;
        self.text.draw(&mut pm, &clock, cx - cw / 2.0, top, Role::UI, clock_size, t.text);
        let date_size = 26.0 * s;
        let (dw, _) = self.text.measure(&date, Role::UI, date_size);
        self.text.draw(&mut pm, &date, cx - dw / 2.0, top + ch + 6.0 * s, Role::UI, date_size, t.text_dim);

        // Account badge: the initial in an accent circle, then the name.
        let badge_y = h as f32 * 0.52;
        let r = 40.0 * s;
        if let Some(av) = &self.avatar {
            // The picture in an accent ring.
            fill_circle(&mut pm, cx, badge_y, r + 3.0 * s, t.accent);
            draw_avatar(&mut pm, av, cx, badge_y, r);
        } else {
            fill_circle(&mut pm, cx, badge_y, r, t.accent);
            let initial: String = self.user.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
            let isz = 40.0 * s;
            let (iw, ih) = self.text.measure(&initial, Role::UI, isz);
            self.text.draw(&mut pm, &initial, cx - iw / 2.0, badge_y - ih / 2.0, Role::UI, isz, t.accent_text);
        }
        let name_size = 24.0 * s;
        let (nw, nh) = self.text.measure(&self.user, Role::UI, name_size);
        let name_y = badge_y + r + 17.0 * s;
        self.text.draw(&mut pm, &self.user, cx - nw / 2.0, name_y, Role::UI, name_size, t.text);

        // Password field.
        let fw = (420.0 * s).min(w as f32 * 0.8);
        let fh = 56.0 * s;
        let fy = name_y + nh + 22.0 * s;
        fill_rounded_rect(&mut pm, cx - fw / 2.0, fy, fw, fh, 14.0 * s, with_alpha(t.surface, 0.85));
        let border = match self.phase {
            Phase::Failed { .. } => t.err,
            Phase::Verifying => t.warn,
            Phase::Idle => t.border,
        };
        stroke_rounded_rect(&mut pm, cx - fw / 2.0, fy, fw, fh, 14.0 * s, border, 2.0 * s);

        let n = self.password.chars().count();
        if n == 0 && self.phase == Phase::Idle {
            let hint = "Password";
            let hs = 20.0 * s;
            let (hw, hh) = self.text.measure(hint, Role::UI, hs);
            self.text.draw(&mut pm, hint, cx - hw / 2.0, fy + (fh - hh) / 2.0, Role::UI, hs, t.text_dim);
        } else {
            let shown = n.min(MAX_DOTS);
            let pitch = 22.0 * s;
            let x0 = cx - (shown as f32 - 1.0) * pitch / 2.0;
            for i in 0..shown {
                fill_circle(&mut pm, x0 + i as f32 * pitch, fy + fh / 2.0, 6.0 * s, t.text);
            }
        }

        // Status line.
        let status = match (&self.phase, &self.message) {
            (Phase::Verifying, _) => Some(("Checking...".to_string(), t.text_dim)),
            (_, Some(m)) => Some((m.clone(), t.err)),
            _ if self.caps => Some(("Caps Lock is on".to_string(), t.warn)),
            _ => None,
        };
        if let Some((msg, color)) = status {
            let ms = 20.0 * s;
            let msg = self.text.ellipsize(&msg, Role::UI, ms, w as f32 * 0.8);
            let (mw, _) = self.text.measure(&msg, Role::UI, ms);
            self.text.draw(&mut pm, &msg, cx - mw / 2.0, fy + fh + 18.0 * s, Role::UI, ms, color);
        }

        // Battery, top right.
        if let Some(r) = self.battery.reading() {
            draw_battery(&mut self.text, &mut pm, t, &r, w as f32, s);
        }

        // The power menu under the field, or the hint that opens it.
        match self.menu {
            Some(m) => draw_menu(&mut self.text, &mut pm, t, m, cx, fy + fh + 110.0 * s, s),
            None => {
                let hint = "Tab  Power options";
                let hs = 16.0 * s;
                let (hw, hh) = self.text.measure(hint, Role::UI, hs);
                let y = h as f32 - 36.0 * s - hh;
                self.text.draw(&mut pm, hint, cx - hw / 2.0, y, Role::UI, hs, with_alpha(t.text_dim, 0.8));
            }
        }

        pm.take()
    }
}

/// Battery pill in the top-right corner: icon, percentage, time left.
fn draw_battery(text: &mut TextSystem, pm: &mut Pixmap, t: &Theme, r: &Reading, w: f32, s: f32) {
    let color = if r.plugged_in() {
        t.ok
    } else if r.pct <= 15 {
        t.err
    } else if r.pct <= 30 {
        t.warn
    } else {
        t.text
    };
    let icon = r.icon().to_string();
    let label = r.label();
    let (isz, lsz) = (22.0 * s, 18.0 * s);
    let (iw, ih) = text.measure(&icon, Role::Icons, isz);
    let (lw, lh) = text.measure(&label, Role::UI, lsz);
    let (padx, gap, ph) = (16.0 * s, 10.0 * s, 40.0 * s);
    let pw = padx * 2.0 + iw + gap + lw;
    let (x, y) = (w - 28.0 * s - pw, 28.0 * s);
    fill_rounded_rect(pm, x, y, pw, ph, ph / 2.0, with_alpha(t.surface, 0.6));
    text.draw(pm, &icon, x + padx, y + (ph - ih) / 2.0, Role::Icons, isz, color);
    text.draw(pm, &label, x + padx + iw + gap, y + (ph - lh) / 2.0, Role::UI, lsz, t.text);
}

/// Three round buttons centred on (`cx`, `cy`) and a caption under them: the
/// selected action, or what the next Enter will do.
fn draw_menu(text: &mut TextSystem, pm: &mut Pixmap, t: &Theme, m: Menu, cx: f32, cy: f32, s: f32) {
    let r = 30.0 * s;
    let pitch = 2.0 * r + 28.0 * s;
    let x0 = cx - (PowerAction::ALL.len() as f32 - 1.0) * pitch / 2.0;
    for (i, a) in PowerAction::ALL.iter().enumerate() {
        let x = x0 + i as f32 * pitch;
        let on = i == m.sel;
        let hot = if *a == PowerAction::PowerOff { t.err } else { t.accent };
        fill_circle(pm, x, cy, r, if on { hot } else { with_alpha(t.surface, 0.85) });
        stroke_circle(pm, x, cy, r, if on { hot } else { t.border }, 2.0 * s);
        let isz = 26.0 * s;
        let (iw, ih) = text.measure(a.icon(), Role::Icons, isz);
        let ink = if on { t.accent_text } else { t.text };
        text.draw(pm, a.icon(), x - iw / 2.0, cy - ih / 2.0, Role::Icons, isz, ink);
    }
    let a = PowerAction::ALL[m.sel];
    let (cap, color) = if m.armed {
        (format!("Press Enter again to {}", a.verb()), if a == PowerAction::PowerOff { t.err } else { t.warn })
    } else {
        (a.label().to_string(), t.text)
    };
    let cs = 20.0 * s;
    let (cw, ch) = text.measure(&cap, Role::UI, cs);
    let cap_y = cy + r + 16.0 * s;
    text.draw(pm, &cap, cx - cw / 2.0, cap_y, Role::UI, cs, color);
    let keys = "\u{2190} \u{2192}  Enter  Esc";
    let ks = 15.0 * s;
    let (kw, _) = text.measure(keys, Role::UI, ks);
    text.draw(pm, keys, cx - kw / 2.0, cap_y + ch + 8.0 * s, Role::UI, ks, with_alpha(t.text_dim, 0.8));
}

/// Paint `av` as a circle of radius `r` centred on (`cx`, `cy`).
fn draw_avatar(pm: &mut Pixmap, av: &Pixmap, cx: f32, cy: f32, r: f32) {
    let scale = 2.0 * r / av.width() as f32;
    let to_canvas = Transform::from_translate(cx - r, cy - r).pre_scale(scale, scale);
    let mut paint = Paint::default();
    paint.shader = Pattern::new(av.as_ref(), SpreadMode::Pad, FilterQuality::Bicubic, 1.0, to_canvas);
    paint.anti_alias = true;
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    if let Some(path) = pb.finish() {
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn with_alpha(c: Color, a: f32) -> Color {
    Color::from_rgba(c.red(), c.green(), c.blue(), a).unwrap_or(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> LockScreen {
        LockScreen::new(&veil_palette::Tokens::default(), None, "abyss", None)
    }

    fn key(sym: Keysym, text: Option<&str>) -> Key {
        Key { sym, text: text.map(str::to_string), ctrl: false, alt: false, shift: false, repeat: false }
    }

    fn type_str(s: &mut LockScreen, text: &str) {
        for ch in text.chars() {
            let t = ch.to_string();
            s.key(&key(Keysym::from_char(ch), Some(&t)));
        }
    }

    #[test]
    fn typed_characters_build_the_password_and_backspace_removes_one() {
        let mut s = screen();
        type_str(&mut s, "hunter2");
        assert_eq!(s.password.as_str(), "hunter2");
        assert_eq!(s.key(&key(Keysym::BackSpace, None)), Action::Redraw);
        assert_eq!(s.password.as_str(), "hunter");
    }

    #[test]
    fn enter_on_an_empty_field_does_nothing() {
        let mut s = screen();
        assert_eq!(s.key(&key(Keysym::Return, None)), Action::None);
        assert!(!s.is_verifying());
    }

    #[test]
    fn enter_submits_once_and_input_is_frozen_while_verifying() {
        let mut s = screen();
        type_str(&mut s, "pw");
        let a = s.key(&key(Keysym::Return, None));
        assert_eq!(a, Action::Submit(Zeroizing::new("pw".to_string())));
        assert!(s.is_verifying());
        // Typing, a second Enter, and Escape are all ignored mid-check.
        assert_eq!(s.key(&key(Keysym::Return, None)), Action::None);
        type_str(&mut s, "zz");
        assert_eq!(s.key(&key(Keysym::Escape, None)), Action::None);
        assert_eq!(s.password.as_str(), "pw");
    }

    #[test]
    fn a_failure_clears_the_field_and_times_out() {
        let mut s = screen();
        type_str(&mut s, "wrong");
        s.key(&key(Keysym::Return, None));
        let t0 = Instant::now();
        s.failed("Wrong password".into(), t0);
        assert!(s.password.is_empty(), "a failed password must not linger");
        assert!(!s.is_verifying() && s.is_animating());
        assert!(!s.tick(t0 + Duration::from_secs(1)));
        assert!(s.tick(t0 + FAILURE_SHOWN));
        assert!(!s.is_animating());
    }

    #[test]
    fn typing_after_a_failure_starts_over() {
        let mut s = screen();
        s.failed("Wrong password".into(), Instant::now());
        type_str(&mut s, "x");
        assert_eq!(s.password.as_str(), "x");
        assert!(!s.is_animating());
    }

    #[test]
    fn escape_and_ctrl_u_clear_the_field() {
        let mut s = screen();
        type_str(&mut s, "abc");
        s.key(&key(Keysym::Escape, None));
        assert!(s.password.is_empty());
        type_str(&mut s, "abc");
        let mut k = key(Keysym::u, Some("u"));
        k.ctrl = true;
        s.key(&k);
        assert!(s.password.is_empty());
    }

    #[test]
    fn ctrl_chords_never_type_into_the_password() {
        let mut s = screen();
        let mut k = key(Keysym::c, Some("c"));
        k.ctrl = true;
        assert_eq!(s.key(&k), Action::None);
        assert!(s.password.is_empty());
    }

    #[test]
    fn a_profile_picture_replaces_the_initial_and_is_clipped_to_a_circle() {
        let mut red = Pixmap::new(crate::avatar::SIDE, crate::avatar::SIDE).unwrap();
        red.fill(Color::from_rgba8(255, 0, 0, 255));
        let mut s = LockScreen::new(&veil_palette::Tokens::default(), None, "abyss", Some(red));
        let (w, h) = (640u32, 360u32);
        let px = s.render(w, h);
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            (px[i], px[i + 1], px[i + 2])
        };
        // Badge centre is at (w/2, 0.52h): solid picture colour there.
        assert_eq!(at(w / 2, (h as f32 * 0.52) as u32), (255, 0, 0));
        // The picture is a circle: its square corner is not painted red.
        let r = 40.0 * (h as f32 / 1080.0).clamp(0.6, 2.5);
        let corner = at((w as f32 / 2.0 + r * 0.95) as u32, (h as f32 * 0.52 - r * 0.95) as u32);
        assert_ne!(corner, (255, 0, 0));
    }

    #[test]
    fn render_fills_exactly_w_by_h_pixels_and_draws_something() {
        let mut s = screen();
        type_str(&mut s, "abc");
        let px = s.render(640, 360);
        assert_eq!(px.len(), 640 * 360 * 4);
        assert!(px.iter().any(|&b| b != 0), "the clock and field are drawn");
    }

    fn tab() -> Key {
        key(Keysym::Tab, None)
    }

    fn enter() -> Key {
        key(Keysym::Return, None)
    }

    #[test]
    fn tab_opens_the_power_menu_and_nothing_in_it_types_into_the_password() {
        let mut s = screen();
        type_str(&mut s, "abc");
        s.key(&tab());
        assert!(s.menu.is_some());
        type_str(&mut s, "xyz");
        s.key(&key(Keysym::BackSpace, None));
        assert_eq!(s.password.as_str(), "abc");
        // Esc closes the menu and leaves the field alone.
        assert_eq!(s.key(&key(Keysym::Escape, None)), Action::Redraw);
        assert!(s.menu.is_none());
        assert_eq!(s.password.as_str(), "abc");
    }

    #[test]
    fn suspend_runs_at_once() {
        let mut s = screen();
        s.key(&tab());
        assert_eq!(s.key(&enter()), Action::Power(PowerAction::Suspend));
        assert!(s.menu.is_none());
    }

    #[test]
    fn reboot_and_power_off_need_enter_twice() {
        for (n, want) in [("2", PowerAction::Reboot), ("3", PowerAction::PowerOff)] {
            let mut s = screen();
            s.key(&tab());
            assert_eq!(s.key(&key(Keysym::from_char(n.chars().next().unwrap()), Some(n))), Action::Redraw);
            assert!(s.menu.unwrap().armed, "first press only arms");
            assert_eq!(s.key(&enter()), Action::Power(want));
            assert!(s.menu.is_none());
        }
    }

    #[test]
    fn any_other_key_or_moving_cancels_a_pending_confirm() {
        let mut s = screen();
        s.key(&tab());
        s.key(&key(Keysym::Right, None));
        s.key(&enter());
        assert!(s.menu.unwrap().armed);
        s.key(&key(Keysym::q, Some("q")));
        assert!(!s.menu.unwrap().armed);
        s.key(&enter());
        s.key(&key(Keysym::Right, None));
        assert!(!s.menu.unwrap().armed);
        // Esc backs out of a confirm first, and only then closes.
        s.key(&enter());
        s.key(&key(Keysym::Escape, None));
        assert!(s.menu.is_some() && !s.menu.unwrap().armed);
        s.key(&key(Keysym::Escape, None));
        assert!(s.menu.is_none());
    }

    #[test]
    fn a_held_enter_never_fires_a_destructive_action() {
        let mut s = screen();
        s.key(&tab());
        s.key(&key(Keysym::Right, None));
        s.key(&enter());
        let mut held = enter();
        held.repeat = true;
        assert_eq!(s.key(&held), Action::None);
        assert!(s.menu.unwrap().armed);
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut s = screen();
        s.key(&tab());
        s.key(&key(Keysym::Left, None));
        assert_eq!(s.menu.unwrap().sel, 2);
        s.key(&key(Keysym::Right, None));
        assert_eq!(s.menu.unwrap().sel, 0);
    }

    #[test]
    fn caps_lock_is_reported_once_per_change() {
        let mut s = screen();
        assert!(s.set_caps(true));
        assert!(!s.set_caps(true));
        assert!(s.set_caps(false));
    }

    #[test]
    fn rendering_with_the_menu_open_and_armed_draws_inside_the_buffer() {
        let mut s = screen();
        s.set_caps(true);
        s.key(&tab());
        s.key(&key(Keysym::Right, None));
        s.key(&enter());
        let px = s.render(640, 360);
        assert_eq!(px.len(), 640 * 360 * 4);
    }
}
