//! The power menu as a `veil_ui::modal` app: key and pointer mapping, and what
//! each action runs. All the Wayland plumbing lives in the modal module.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::Command;

use veil_palette::Tokens;
use veil_text::FontConfig;
use veil_ui::modal::{Flow, Key, ModalApp, Pointer};
use veil_ui::Keysym;

use crate::panel::{Action, ClickZone, Panel};

pub struct PowerApp {
    panel: Panel,
    zones: Vec<ClickZone>,
    /// What to do with a chosen action. Replaced in tests so they never
    /// reach `systemctl`.
    runner: Box<dyn FnMut(Action)>,
}

impl PowerApp {
    pub fn new(tokens: Tokens, font_cfg: Option<&FontConfig>, mut ipc: Option<UnixStream>) -> Self {
        Self::with_runner(tokens, font_cfg, Box::new(move |a| run_action(&mut ipc, a)))
    }

    pub fn with_runner(tokens: Tokens, font_cfg: Option<&FontConfig>, runner: Box<dyn FnMut(Action)>) -> Self {
        PowerApp { panel: Panel::new(tokens, font_cfg), zones: Vec::new(), runner }
    }

    /// Run the chosen action (if any) and say what the surface should do.
    fn finish(&mut self, close: bool, action: Option<Action>) -> Flow {
        if let Some(a) = action {
            (self.runner)(a);
        }
        if close {
            Flow::Close
        } else {
            Flow::Redraw
        }
    }

    fn number(&mut self, n: usize) -> Flow {
        let (close, action) = self.panel.select_number(n);
        self.finish(close, action)
    }
}

/// Keys that keep acting while held.
fn is_navigation(sym: Keysym) -> bool {
    matches!(sym, Keysym::Left | Keysym::Right | Keysym::Up | Keysym::Down | Keysym::h | Keysym::j | Keysym::k | Keysym::l)
}

impl ModalApp for PowerApp {
    fn render(&mut self, w: u32, h: u32) -> Vec<u8> {
        let (pixels, zones) = self.panel.render(w, h);
        self.zones = zones;
        pixels
    }

    fn key(&mut self, key: &Key) -> Flow {
        if key.repeat && !is_navigation(key.sym) {
            return Flow::Idle;
        }
        match key.sym {
            Keysym::Escape => {
                let close = self.panel.key_escape();
                self.finish(close, None)
            }
            Keysym::Return | Keysym::KP_Enter => {
                let (close, action) = self.panel.key_enter();
                self.finish(close, action)
            }
            Keysym::Left | Keysym::h => {
                self.panel.nav_prev();
                Flow::Redraw
            }
            Keysym::Right | Keysym::l => {
                self.panel.nav_next();
                Flow::Redraw
            }
            Keysym::Up | Keysym::k => {
                self.panel.nav_up();
                Flow::Redraw
            }
            Keysym::Down | Keysym::j => {
                self.panel.nav_down();
                Flow::Redraw
            }
            Keysym::_1 => self.number(0),
            Keysym::_2 => self.number(1),
            Keysym::_3 => self.number(2),
            Keysym::_4 => self.number(3),
            Keysym::_5 => self.number(4),
            Keysym::_6 => self.number(5),
            _ => Flow::Idle,
        }
    }

    fn pointer(&mut self, ev: Pointer) -> Flow {
        match ev {
            Pointer::Move { x, y } => {
                if self.panel.hover(x, y, &self.zones) {
                    Flow::Redraw
                } else {
                    Flow::Idle
                }
            }
            Pointer::Click { x, y } => {
                let (close, action) = self.panel.click(x, y, &self.zones);
                self.finish(close, action)
            }
            Pointer::Scroll { .. } | Pointer::Leave => Flow::Idle,
        }
    }
}

/// Ask the compositor to do it over IPC; with no compositor (the menu run on
/// its own) fall back to the system tools.
fn run_action(ipc: &mut Option<UnixStream>, action: Action) {
    tracing::info!("veil-power: executing action {action:?}");
    let cmd = match action {
        Action::Lock => Some("session.lock"),
        Action::Logout => Some("session.logout"),
        Action::Suspend => Some("session.suspend"),
        Action::Hibernate => None,
        Action::Reboot => Some("session.reboot"),
        Action::Shutdown => Some("session.poweroff"),
    };

    if let (Some(cmd), Some(stream)) = (cmd, ipc.as_mut()) {
        let msg = veil_ipc::ClientMessage::Cmd { id: 1, cmd: cmd.to_string(), args: serde_json::Value::Null };
        if veil_ipc::encode(&msg).is_ok_and(|bytes| stream.write_all(&bytes).is_ok()) {
            return;
        }
    }

    // Standalone fallback
    match action {
        Action::Lock => {
            let _ = Command::new("veil-lock").spawn().or_else(|_| Command::new("swaylock").arg("-f").spawn());
        }
        Action::Logout => {
            let _ = Command::new("pkill").args(["-TERM", "veil"]).spawn();
        }
        Action::Suspend => {
            let _ = Command::new("systemctl").arg("suspend").spawn();
        }
        Action::Hibernate => {
            let _ = Command::new("systemctl").arg("hibernate").spawn();
        }
        Action::Reboot => {
            let _ = Command::new("systemctl").arg("reboot").spawn();
        }
        Action::Shutdown => {
            let _ = Command::new("systemctl").arg("poweroff").spawn();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn key(sym: Keysym, repeat: bool) -> Key {
        Key { sym, text: None, ctrl: false, alt: false, shift: false, repeat }
    }

    fn app() -> (PowerApp, Rc<RefCell<Vec<Action>>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let sink = log.clone();
        let app = PowerApp::with_runner(Tokens::default(), None, Box::new(move |a| sink.borrow_mut().push(a)));
        (app, log)
    }

    #[test]
    fn navigation_redraws_and_unknown_keys_do_nothing() {
        let (mut a, log) = app();
        for sym in [Keysym::Left, Keysym::Right, Keysym::Up, Keysym::Down, Keysym::h, Keysym::j, Keysym::k, Keysym::l] {
            assert_eq!(a.key(&key(sym, false)), Flow::Redraw, "{sym:?}");
        }
        assert_eq!(a.key(&key(Keysym::x, false)), Flow::Idle);
        assert_eq!(a.key(&key(Keysym::F5, false)), Flow::Idle);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn only_navigation_repeats() {
        let (mut a, log) = app();
        assert_eq!(a.key(&key(Keysym::Right, true)), Flow::Redraw);
        assert_eq!(a.key(&key(Keysym::Return, true)), Flow::Idle);
        assert_eq!(a.key(&key(Keysym::_1, true)), Flow::Idle);
        assert!(log.borrow().is_empty(), "a held Enter or number must never run an action");
    }

    #[test]
    fn escape_closes_a_fresh_menu() {
        let (mut a, log) = app();
        assert_eq!(a.key(&key(Keysym::Escape, false)), Flow::Close);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn an_unconfirmed_action_runs_once_and_closes() {
        let (mut a, log) = app();
        // Lock (the first entry) needs no confirmation.
        assert_eq!(a.key(&key(Keysym::Return, false)), Flow::Close);
        assert_eq!(*log.borrow(), [Action::Lock]);
    }

    #[test]
    fn a_dangerous_action_asks_first_and_escape_backs_out() {
        let (mut a, log) = app();
        assert_eq!(a.key(&key(Keysym::_6, false)), Flow::Redraw, "Shutdown asks for confirmation");
        assert!(log.borrow().is_empty());
        assert_eq!(a.key(&key(Keysym::Escape, false)), Flow::Redraw, "backs out, the menu stays");
        assert!(log.borrow().is_empty());
        // Enter on the default (Cancel) also backs out
        a.key(&key(Keysym::_6, false));
        assert_eq!(a.key(&key(Keysym::Return, false)), Flow::Redraw);
        assert!(log.borrow().is_empty());
        // choosing Confirm runs it
        a.key(&key(Keysym::_6, false));
        a.key(&key(Keysym::Right, false));
        assert_eq!(a.key(&key(Keysym::Return, false)), Flow::Close);
        assert_eq!(*log.borrow(), [Action::Shutdown]);
    }

    #[test]
    fn rendering_fills_the_surface_and_records_click_zones() {
        let (mut a, _) = app();
        let px = a.render(800, 600);
        assert_eq!(px.len(), 800 * 600 * 4);
        assert!(!a.zones.is_empty());
    }

    #[test]
    fn pointer_events_map_to_flows() {
        let (mut a, log) = app();
        a.render(800, 600);
        assert_eq!(a.pointer(Pointer::Move { x: 1.0, y: 1.0 }), Flow::Idle, "empty space");
        assert_eq!(a.pointer(Pointer::Scroll { dy: 3.0 }), Flow::Idle);
        assert_eq!(a.pointer(Pointer::Leave), Flow::Idle);
        let z = a.zones[0].clone();
        let (cx, cy) = ((z.x0 + z.x1) / 2.0, (z.y0 + z.y1) / 2.0);
        assert_ne!(a.pointer(Pointer::Click { x: cx, y: cy }), Flow::Idle);
        assert!(log.borrow().len() <= 1);
        // a click on nothing neither runs nor closes
        let before = log.borrow().len();
        assert_eq!(a.pointer(Pointer::Click { x: 1.0, y: 1.0 }), Flow::Redraw);
        assert_eq!(log.borrow().len(), before);
    }
}
