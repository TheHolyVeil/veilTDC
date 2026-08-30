//! `<mod_key>+P` power menu (Combo 5 follow-up, alongside Alt+Tab).
//!
//! Deliberately the simplest thing that works: three fixed entries, no
//! scanning, no confirmation step. Modeled directly on `launcher.rs`'s
//! modal shape (query-less here since there's nothing to type) so the key
//! handling / overlay drawing in server.rs can reuse the exact same
//! intercept-all-input pattern.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Poweroff,
    Reboot,
    /// Same graceful-quit path as Shift+Alt+E — not a real logout (there's
    /// no session manager here), just stops veil-host.
    Logout,
}

impl PowerAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Poweroff => "POWER OFF",
            Self::Reboot   => "REBOOT",
            Self::Logout   => "LOG OUT",
        }
    }
}

pub const POWER_ACTIONS: [PowerAction; 3] =
    [PowerAction::Poweroff, PowerAction::Reboot, PowerAction::Logout];

#[derive(Clone)]
pub struct PowerMenu {
    pub selected: usize,
}

impl Default for PowerMenu {
    fn default() -> Self {
        Self::new()
    }
}

impl PowerMenu {
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    pub fn selected_action(&self) -> PowerAction {
        POWER_ACTIONS[self.selected.min(POWER_ACTIONS.len() - 1)]
    }
}
