//! The lock screen's state and drawing. No Wayland in here: keys go in,
//! pixels and actions come out.
//!
//! The compositor dims the wallpaper behind this surface, so the screen is
//! drawn on a transparent canvas: clock, account, and the password field.

use std::time::{Duration, Instant};

use tiny_skia::{Color, FillRule, FilterQuality, Paint, PathBuilder, Pattern, Pixmap, SpreadMode, Transform};
use veil_ui::modal::Key;
use veil_ui::{fill_circle, fill_rounded_rect, stroke_rounded_rect, FontConfig, Keysym, Role, TextSystem, Theme};
use zeroize::Zeroizing;

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
        // Typing after a failure starts over.
        if matches!(self.phase, Phase::Failed { .. }) {
            self.phase = Phase::Idle;
            self.message = None;
        }
        match key.sym {
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

    /// The password was wrong (or PAM refused): clear the field, say why.
    pub fn failed(&mut self, message: String, now: Instant) {
        self.clear();
        self.message = Some(message);
        self.phase = Phase::Failed { until: now + FAILURE_SHOWN };
    }

    /// The failure message times out. True when the screen changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        if let Phase::Failed { until } = self.phase {
            if now >= until {
                self.phase = Phase::Idle;
                self.message = None;
                return true;
            }
        }
        false
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
            _ => None,
        };
        if let Some((msg, color)) = status {
            let ms = 20.0 * s;
            let msg = self.text.ellipsize(&msg, Role::UI, ms, w as f32 * 0.8);
            let (mw, _) = self.text.measure(&msg, Role::UI, ms);
            self.text.draw(&mut pm, &msg, cx - mw / 2.0, fy + fh + 18.0 * s, Role::UI, ms, color);
        }

        pm.take()
    }
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
}
