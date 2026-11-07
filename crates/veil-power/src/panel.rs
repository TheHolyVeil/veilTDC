//! Power menu panel — state machine + tiny-skia rendering with Veil tokens and fonts.

use tiny_skia::*;
use veil_palette::Tokens;
use veil_text::{FontConfig, Role, TextSystem};
use veil_ui::draw::{fill_circle, fill_rounded_rect, stroke_circle, stroke_rounded_rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Lock,
    Logout,
    Suspend,
    Hibernate,
    Reboot,
    Shutdown,
}

impl Action {
    pub const ALL: &'static [Action] = &[
        Action::Lock,
        Action::Logout,
        Action::Suspend,
        Action::Hibernate,
        Action::Reboot,
        Action::Shutdown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::Lock => "Lock",
            Action::Logout => "Logout",
            Action::Suspend => "Suspend",
            Action::Hibernate => "Hibernate",
            Action::Reboot => "Reboot",
            Action::Shutdown => "Shutdown",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Action::Lock => "\u{f033e}",      // nf-md-lock
            Action::Logout => "\u{f0343}",    // nf-md-logout
            Action::Suspend => "\u{f04b2}",   // nf-md-sleep
            Action::Hibernate => "\u{f0e4d}", // nf-md-snowflake-melt
            Action::Reboot => "\u{f0709}",    // nf-md-restart
            Action::Shutdown => "\u{f0425}",  // nf-md-power
        }
    }

    pub fn needs_confirm(self) -> bool {
        matches!(self, Action::Reboot | Action::Shutdown | Action::Hibernate)
    }

    pub fn confirm_text(self) -> &'static str {
        match self {
            Action::Reboot => "Reboot the system?",
            Action::Shutdown => "Shut down the system?",
            Action::Hibernate => "Hibernate the system?",
            _ => "Are you sure?",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Main,
    Confirm(Action),
}

#[derive(Clone, Debug)]
pub struct ClickZone {
    pub action_idx: usize,
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

pub struct Panel {
    pub screen: Screen,
    pub selected: usize,
    pub tokens: Tokens,
    pub text_system: TextSystem,
}

impl Panel {
    pub fn new(tokens: Tokens, font_cfg: Option<&FontConfig>) -> Self {
        let text_system = match font_cfg {
            Some(cfg) => TextSystem::new(cfg),
            None => TextSystem::new(&FontConfig::default()),
        };
        Self {
            screen: Screen::Main,
            selected: 0,
            tokens,
            text_system,
        }
    }

    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    pub fn set_font(&mut self, cfg: &FontConfig) {
        self.text_system.reload(cfg);
    }

    /// Key handler for Escape. Returns true if power menu should close.
    pub fn key_escape(&mut self) -> bool {
        match self.screen {
            Screen::Confirm(_) => {
                self.screen = Screen::Main;
                false
            }
            Screen::Main => true,
        }
    }

    /// Key handler for Enter. Returns (should_close, executed_action).
    pub fn key_enter(&mut self) -> (bool, Option<Action>) {
        match self.screen {
            Screen::Main => {
                let action = Action::ALL[self.selected.min(Action::ALL.len() - 1)];
                self.activate(action)
            }
            Screen::Confirm(action) => {
                if self.selected == 1 {
                    // Confirm
                    (true, Some(action))
                } else {
                    // Cancel
                    self.screen = Screen::Main;
                    self.selected = 0;
                    (false, None)
                }
            }
        }
    }

    pub fn activate(&mut self, action: Action) -> (bool, Option<Action>) {
        if action.needs_confirm() {
            self.screen = Screen::Confirm(action);
            self.selected = 0; // Cancel by default
            (false, None)
        } else {
            (true, Some(action))
        }
    }

    pub fn nav_prev(&mut self) {
        match self.screen {
            Screen::Main => {
                if self.selected > 0 {
                    self.selected -= 1;
                } else {
                    self.selected = Action::ALL.len() - 1;
                }
            }
            Screen::Confirm(_) => {
                self.selected = 0;
            }
        }
    }

    pub fn nav_next(&mut self) {
        match self.screen {
            Screen::Main => {
                self.selected = (self.selected + 1) % Action::ALL.len();
            }
            Screen::Confirm(_) => {
                self.selected = 1;
            }
        }
    }

    pub fn nav_up(&mut self) {
        if let Screen::Main = self.screen {
            let cols = 3usize;
            if self.selected >= cols {
                self.selected -= cols;
            }
        }
    }

    pub fn nav_down(&mut self) {
        if let Screen::Main = self.screen {
            let cols = 3usize;
            if self.selected + cols < Action::ALL.len() {
                self.selected += cols;
            }
        }
    }

    pub fn select_number(&mut self, n: usize) -> (bool, Option<Action>) {
        if let Screen::Main = self.screen {
            if n < Action::ALL.len() {
                self.selected = n;
                return self.key_enter();
            }
        }
        (false, None)
    }

    pub fn hover(&mut self, x: f32, y: f32, zones: &[ClickZone]) -> bool {
        for z in zones {
            if x >= z.x0 && x <= z.x1 && y >= z.y0 && y <= z.y1 {
                let new_sel = match z.action_idx {
                    100 => 0,
                    101 => 1,
                    i => i,
                };
                if self.selected != new_sel {
                    self.selected = new_sel;
                    return true;
                }
                return false;
            }
        }
        false
    }

    pub fn click(&mut self, x: f32, y: f32, zones: &[ClickZone]) -> (bool, Option<Action>) {
        for z in zones {
            if x >= z.x0 && x <= z.x1 && y >= z.y0 && y <= z.y1 {
                return match z.action_idx {
                    100 => {
                        self.screen = Screen::Main;
                        self.selected = 0;
                        (false, None)
                    }
                    101 => match self.screen {
                        Screen::Confirm(action) => (true, Some(action)),
                        _ => (false, None),
                    },
                    i => {
                        self.selected = i;
                        self.key_enter()
                    }
                };
            }
        }
        (false, None)
    }

    /// Render panel returning BGRA bytes for wl_shm Argb8888 and click zones.
    pub fn render(&mut self, w: u32, h: u32) -> (Vec<u8>, Vec<ClickZone>) {
        let mut pm = Pixmap::new(w, h).unwrap();

        // Background overlay: semi-transparent wash of theme background
        let bg_color = Color::from_rgba8(
            self.tokens.bg[0],
            self.tokens.bg[1],
            self.tokens.bg[2],
            210,
        );
        pm.fill(bg_color);

        let mut zones = Vec::new();
        match self.screen {
            Screen::Main => self.draw_main(&mut pm, w, h, &mut zones),
            Screen::Confirm(action) => self.draw_confirm(&mut pm, w, h, action, &mut zones),
        }

        // Convert Premultiplied RGBA from tiny-skia to Argb8888 (BGRA order)
        let data = pm.data();
        let mut out = Vec::with_capacity(data.len());
        for chunk in data.as_chunks::<4>().0 {
            out.push(chunk[2]); // B
            out.push(chunk[1]); // G
            out.push(chunk[0]); // R
            out.push(chunk[3]); // A
        }
        (out, zones)
    }

    fn draw_main(&mut self, pm: &mut Pixmap, sw: u32, sh: u32, zones: &mut Vec<ClickZone>) {
        let btn_radius = 48.0f32;
        let gap = 40.0f32;
        let n_actions = Action::ALL.len() as f32;
        let total_w = n_actions * btn_radius * 2.0 + (n_actions - 1.0) * gap;
        let ox = (sw as f32 - total_w) / 2.0;
        let oy = (sh as f32) / 2.0 - btn_radius - 10.0;

        // Hint line at bottom
        let hint = "1-6  ← →  Enter  Esc";
        let (hw, _) = self.text_system.measure(hint, Role::UI, 13.0);
        let text_dim_col = Color::from_rgba8(
            self.tokens.text_dim[0],
            self.tokens.text_dim[1],
            self.tokens.text_dim[2],
            255,
        );
        self.text_system.draw(
            pm,
            hint,
            (sw as f32 - hw) / 2.0,
            oy + btn_radius * 2.0 + 70.0,
            Role::UI,
            13.0,
            text_dim_col,
        );

        for (i, &action) in Action::ALL.iter().enumerate() {
            let cx = ox + i as f32 * (btn_radius * 2.0 + gap) + btn_radius;
            let cy = oy + btn_radius;
            let selected = i == self.selected;

            // Button background
            let bg = if selected {
                Color::from_rgba8(
                    self.tokens.accent[0],
                    self.tokens.accent[1],
                    self.tokens.accent[2],
                    255,
                )
            } else {
                Color::from_rgba8(
                    self.tokens.surface_hi[0],
                    self.tokens.surface_hi[1],
                    self.tokens.surface_hi[2],
                    220,
                )
            };
            fill_circle(pm, cx, cy, btn_radius, bg);

            // Border / selection ring
            let border_color = if selected {
                Color::from_rgba8(
                    self.tokens.accent[0],
                    self.tokens.accent[1],
                    self.tokens.accent[2],
                    255,
                )
            } else {
                Color::from_rgba8(
                    self.tokens.border[0],
                    self.tokens.border[1],
                    self.tokens.border[2],
                    180,
                )
            };
            stroke_circle(pm, cx, cy, btn_radius, border_color, if selected { 2.5 } else { 1.5 });

            // Icon
            let icon = action.icon();
            let icon_sz = 34.0f32;
            let icon_color = if selected {
                Color::from_rgba8(
                    self.tokens.accent_text[0],
                    self.tokens.accent_text[1],
                    self.tokens.accent_text[2],
                    255,
                )
            } else {
                match action {
                    Action::Shutdown => Color::from_rgba8(self.tokens.err[0], self.tokens.err[1], self.tokens.err[2], 255),
                    Action::Reboot => Color::from_rgba8(self.tokens.ok[0], self.tokens.ok[1], self.tokens.ok[2], 255),
                    _ => Color::from_rgba8(self.tokens.text[0], self.tokens.text[1], self.tokens.text[2], 255),
                }
            };
            let (iw, _) = self.text_system.measure(icon, Role::Icons, icon_sz);
            self.text_system.draw(
                pm,
                icon,
                cx - iw / 2.0,
                cy - icon_sz / 2.0 - 2.0,
                Role::Icons,
                icon_sz,
                icon_color,
            );

            // Label below button
            let label = action.label();
            let (lw, _) = self.text_system.measure(label, Role::UI, 13.0);
            let label_col = if selected {
                Color::from_rgba8(self.tokens.accent[0], self.tokens.accent[1], self.tokens.accent[2], 255)
            } else {
                Color::from_rgba8(self.tokens.text[0], self.tokens.text[1], self.tokens.text[2], 255)
            };
            self.text_system.draw(
                pm,
                label,
                cx - lw / 2.0,
                cy + btn_radius + 16.0,
                Role::UI,
                13.0,
                label_col,
            );

            zones.push(ClickZone {
                action_idx: i,
                x0: cx - btn_radius,
                y0: cy - btn_radius,
                x1: cx + btn_radius,
                y1: cy + btn_radius + 30.0,
            });
        }
    }

    fn draw_confirm(
        &mut self,
        pm: &mut Pixmap,
        sw: u32,
        sh: u32,
        action: Action,
        zones: &mut Vec<ClickZone>,
    ) {
        let dialog_w = 420.0f32;
        let dialog_h = 200.0f32;
        let dx = (sw as f32 - dialog_w) / 2.0;
        let dy = (sh as f32 - dialog_h) / 2.0;

        // Card background
        let card_bg = Color::from_rgba8(
            self.tokens.surface[0],
            self.tokens.surface[1],
            self.tokens.surface[2],
            245,
        );
        fill_rounded_rect(pm, dx, dy, dialog_w, dialog_h, 14.0, card_bg);

        let border_col = Color::from_rgba8(
            self.tokens.accent[0],
            self.tokens.accent[1],
            self.tokens.accent[2],
            220,
        );
        stroke_rounded_rect(pm, dx, dy, dialog_w, dialog_h, 14.0, border_col, 1.5);

        // Icon
        let icon = action.icon();
        let icon_sz = 30.0f32;
        let icon_col = match action {
            Action::Shutdown => Color::from_rgba8(self.tokens.err[0], self.tokens.err[1], self.tokens.err[2], 255),
            Action::Reboot => Color::from_rgba8(self.tokens.ok[0], self.tokens.ok[1], self.tokens.ok[2], 255),
            _ => Color::from_rgba8(self.tokens.accent[0], self.tokens.accent[1], self.tokens.accent[2], 255),
        };
        let (iw, _) = self.text_system.measure(icon, Role::Icons, icon_sz);
        self.text_system.draw(
            pm,
            icon,
            dx + (dialog_w - iw) / 2.0,
            dy + 26.0,
            Role::Icons,
            icon_sz,
            icon_col,
        );

        // Confirmation text
        let msg = action.confirm_text();
        let (mw, _) = self.text_system.measure(msg, Role::UI, 16.0);
        let text_col = Color::from_rgba8(
            self.tokens.text[0],
            self.tokens.text[1],
            self.tokens.text[2],
            255,
        );
        self.text_system.draw(
            pm,
            msg,
            dx + (dialog_w - mw) / 2.0,
            dy + 72.0,
            Role::UI,
            16.0,
            text_col,
        );

        // Cancel / Confirm buttons
        let btn_w = 140.0f32;
        let btn_h = 40.0f32;
        let btn_y = dy + dialog_h - btn_h - 22.0;
        let cancel_x = dx + dialog_w / 2.0 - btn_w - 10.0;
        let confirm_x = dx + dialog_w / 2.0 + 10.0;

        // Cancel (index 0)
        let cancel_sel = self.selected == 0;
        let cancel_bg = if cancel_sel {
            Color::from_rgba8(self.tokens.surface_hi[0], self.tokens.surface_hi[1], self.tokens.surface_hi[2], 255)
        } else {
            Color::from_rgba8(self.tokens.surface[0], self.tokens.surface[1], self.tokens.surface[2], 200)
        };
        fill_rounded_rect(pm, cancel_x, btn_y, btn_w, btn_h, 8.0, cancel_bg);
        let cancel_border = if cancel_sel { border_col } else {
            Color::from_rgba8(self.tokens.border[0], self.tokens.border[1], self.tokens.border[2], 180)
        };
        stroke_rounded_rect(pm, cancel_x, btn_y, btn_w, btn_h, 8.0, cancel_border, if cancel_sel { 2.0 } else { 1.0 });

        let (cw, _) = self.text_system.measure("Cancel", Role::UI, 14.0);
        self.text_system.draw(
            pm,
            "Cancel",
            cancel_x + (btn_w - cw) / 2.0,
            btn_y + 11.0,
            Role::UI,
            14.0,
            text_col,
        );

        // Confirm (index 1)
        let ok_sel = self.selected == 1;
        let ok_bg = if ok_sel {
            match action {
                Action::Shutdown => Color::from_rgba8(self.tokens.err[0], self.tokens.err[1], self.tokens.err[2], 255),
                _ => Color::from_rgba8(self.tokens.accent[0], self.tokens.accent[1], self.tokens.accent[2], 255),
            }
        } else {
            Color::from_rgba8(self.tokens.surface_hi[0], self.tokens.surface_hi[1], self.tokens.surface_hi[2], 200)
        };
        fill_rounded_rect(pm, confirm_x, btn_y, btn_w, btn_h, 8.0, ok_bg);
        stroke_rounded_rect(pm, confirm_x, btn_y, btn_w, btn_h, 8.0, border_col, if ok_sel { 2.0 } else { 1.0 });

        let (ow, _) = self.text_system.measure(action.label(), Role::UI, 14.0);
        let confirm_text_col = if ok_sel {
            Color::from_rgba8(self.tokens.accent_text[0], self.tokens.accent_text[1], self.tokens.accent_text[2], 255)
        } else {
            text_col
        };
        self.text_system.draw(
            pm,
            action.label(),
            confirm_x + (btn_w - ow) / 2.0,
            btn_y + 11.0,
            Role::UI,
            14.0,
            confirm_text_col,
        );

        zones.push(ClickZone {
            action_idx: 100,
            x0: cancel_x,
            y0: btn_y,
            x1: cancel_x + btn_w,
            y1: btn_y + btn_h,
        });
        zones.push(ClickZone {
            action_idx: 101,
            x0: confirm_x,
            y0: btn_y,
            x1: confirm_x + btn_w,
            y1: btn_y + btn_h,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_panel_navigation() {
        let mut panel = Panel::new(Tokens::default(), None);
        assert_eq!(panel.selected, 0);

        panel.nav_next();
        assert_eq!(panel.selected, 1);

        panel.nav_prev();
        assert_eq!(panel.selected, 0);

        panel.nav_prev();
        assert_eq!(panel.selected, Action::ALL.len() - 1);
    }

    #[test]
    fn test_panel_confirm_screen() {
        let mut panel = Panel::new(Tokens::default(), None);
        // Shutdown needs confirmation
        let (close, action) = panel.activate(Action::Shutdown);
        assert!(!close);
        assert_eq!(action, None);
        assert_eq!(panel.screen, Screen::Confirm(Action::Shutdown));
        assert_eq!(panel.selected, 0); // Cancel

        // Enter on cancel goes back to Main
        let (close2, action2) = panel.key_enter();
        assert!(!close2);
        assert_eq!(action2, None);
        assert_eq!(panel.screen, Screen::Main);
    }
}
