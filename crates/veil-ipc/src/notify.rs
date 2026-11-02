//! Payloads of the notification topics and commands (phase 4).
//!
//! - `notifications` event: [`NotificationsState`], published by the `notify`
//!   component through the `notifications.set` command and rebroadcast by the
//!   master (a client cannot publish events itself, so the master relays).
//! - `notify.control` event: [`Control`], emitted by the master after a
//!   `notify.dnd` or `notify.panel` command. `notify` owns the DND flag and the
//!   panel, so `toggle` is resolved there, not in the master.
//!
//! `veil ctl notify.dnd` and `veil ctl notify.panel` send no arguments and
//! mean `toggle`, so they work as keybind targets as they are.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the bar's bell shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationsState {
    /// Unread notifications.
    pub count: u32,
    pub dnd: bool,
}

impl NotificationsState {
    /// Parse the arguments of `notifications.set`. A count too big for `u32` saturates.
    pub fn from_args(args: &Value) -> Result<Self, String> {
        const USAGE: &str = "notifications.set needs {\"count\": n, \"dnd\": true|false}";
        let count = args.get("count").and_then(Value::as_u64).ok_or(USAGE)?;
        let dnd = args.get("dnd").and_then(Value::as_bool).ok_or(USAGE)?;
        Ok(NotificationsState { count: u32::try_from(count).unwrap_or(u32::MAX), dnd })
    }
}

/// Do-not-disturb request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DndMode {
    On,
    Off,
    #[default]
    Toggle,
}

/// History panel request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelMode {
    Open,
    Close,
    #[default]
    Toggle,
}

/// The `notify.control` event: exactly one field is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Control {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dnd: Option<DndMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<PanelMode>,
}

/// The optional `mode` argument; absent or null means the type's default (toggle).
fn mode_arg<T: DeserializeOwned + Default>(args: &Value, allowed: &str) -> Result<T, String> {
    match args.get("mode") {
        None | Some(Value::Null) => Ok(T::default()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|_| format!("mode must be {allowed}")),
    }
}

impl Control {
    /// Arguments of `notify.dnd`: `{"mode": "on"|"off"|"toggle"}`, default toggle.
    pub fn dnd_from_args(args: &Value) -> Result<Control, String> {
        Ok(Control { dnd: Some(mode_arg(args, "on, off or toggle")?), panel: None })
    }

    /// Arguments of `notify.panel`: `{"mode": "open"|"close"|"toggle"}`, default toggle.
    pub fn panel_from_args(args: &Value) -> Result<Control, String> {
        Ok(Control { dnd: None, panel: Some(mode_arg(args, "open, close or toggle")?) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_parses_and_saturates() {
        assert_eq!(
            NotificationsState::from_args(&json!({"count": 3, "dnd": true})).unwrap(),
            NotificationsState { count: 3, dnd: true }
        );
        assert_eq!(NotificationsState::from_args(&json!({"count": u64::MAX, "dnd": false})).unwrap().count, u32::MAX);
    }

    #[test]
    fn state_rejects_missing_or_mistyped_fields() {
        for bad in [json!(null), json!({}), json!({"count": 1}), json!({"dnd": true}), json!({"count": "3", "dnd": true}), json!({"count": -1, "dnd": true})] {
            assert!(NotificationsState::from_args(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn state_round_trips_as_the_event_payload() {
        let s = NotificationsState { count: 7, dnd: false };
        let v = serde_json::to_value(s).unwrap();
        assert_eq!(v, json!({"count": 7, "dnd": false}), "the shape the bar already reads");
        assert_eq!(serde_json::from_value::<NotificationsState>(v).unwrap(), s);
    }

    #[test]
    fn no_arguments_means_toggle() {
        assert_eq!(Control::dnd_from_args(&Value::Null).unwrap().dnd, Some(DndMode::Toggle));
        assert_eq!(Control::dnd_from_args(&json!({})).unwrap().dnd, Some(DndMode::Toggle));
        assert_eq!(Control::panel_from_args(&Value::Null).unwrap().panel, Some(PanelMode::Toggle));
        assert_eq!(Control::panel_from_args(&json!({"mode": null})).unwrap().panel, Some(PanelMode::Toggle));
    }

    #[test]
    fn modes_parse_and_bad_ones_say_what_is_allowed() {
        assert_eq!(Control::dnd_from_args(&json!({"mode": "on"})).unwrap().dnd, Some(DndMode::On));
        assert_eq!(Control::dnd_from_args(&json!({"mode": "off"})).unwrap().dnd, Some(DndMode::Off));
        assert_eq!(Control::panel_from_args(&json!({"mode": "open"})).unwrap().panel, Some(PanelMode::Open));
        assert_eq!(Control::panel_from_args(&json!({"mode": "close"})).unwrap().panel, Some(PanelMode::Close));
        assert_eq!(Control::dnd_from_args(&json!({"mode": "maybe"})), Err("mode must be on, off or toggle".into()));
        assert_eq!(Control::panel_from_args(&json!({"mode": "on"})), Err("mode must be open, close or toggle".into()));
        assert!(Control::dnd_from_args(&json!({"mode": 1})).is_err());
    }

    #[test]
    fn control_serialises_exactly_one_key() {
        let dnd = serde_json::to_value(Control::dnd_from_args(&json!({"mode": "off"})).unwrap()).unwrap();
        assert_eq!(dnd, json!({"dnd": "off"}));
        let panel = serde_json::to_value(Control::panel_from_args(&Value::Null).unwrap()).unwrap();
        assert_eq!(panel, json!({"panel": "toggle"}));
        assert_eq!(serde_json::from_value::<Control>(dnd).unwrap().dnd, Some(DndMode::Off));
        assert_eq!(serde_json::from_value::<Control>(json!({"dnd": "banana"})).ok(), None);
    }
}
