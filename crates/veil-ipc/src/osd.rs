//! The `osd` topic: the compositor asks the OSD component to show something.
//!
//! One event shape for everything the compositor wants on screen briefly:
//! volume and brightness levels, config errors, "component failed" notices.
//! The compositor is the only sender and `veil-osd` the only subscriber, so
//! this is a one-way, additive part of protocol v1.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long an OSD stays when the sender gives no timeout.
pub const DEFAULT_TIMEOUT_MS: u32 = 2000;
/// Bounds applied to every received timeout.
pub const MIN_TIMEOUT_MS: u32 = 300;
pub const MAX_TIMEOUT_MS: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsdKind {
    /// A plain message.
    #[default]
    Info,
    /// A level with a bar (volume, brightness).
    Level,
    /// Something went wrong; drawn in the error colour.
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OsdEvent {
    #[serde(default)]
    pub kind: OsdKind,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// 0..=100 for a level bar; absent for a plain message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u32,
    /// `wl_output` name to show on (`veil-host-0`). Empty means the first output.
    #[serde(default)]
    pub output: String,
}

fn default_timeout() -> u32 {
    DEFAULT_TIMEOUT_MS
}

impl OsdEvent {
    /// Build an event the way the compositor's `show_osd` has always decided
    /// what it is: a bar means a level, `ERROR` in the title means an error.
    pub fn new(title: &str, body: &str, progress: Option<u8>, timeout_ms: u32, output: &str) -> Self {
        let kind = if progress.is_some() {
            OsdKind::Level
        } else if title.contains("ERROR") {
            OsdKind::Error
        } else {
            OsdKind::Info
        };
        OsdEvent {
            kind,
            title: title.to_string(),
            body: body.to_string(),
            progress: progress.map(|p| p.min(100)),
            timeout_ms: timeout_ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS),
            output: output.to_string(),
        }
    }

    /// Parse an event payload, clamping what a receiver must not trust:
    /// progress to 100 and the timeout to its bounds. `None` when the payload
    /// is not an OSD event at all.
    pub fn from_json(v: &Value) -> Option<Self> {
        let mut ev: OsdEvent = serde_json::from_value(v.clone()).ok()?;
        ev.progress = ev.progress.map(|p| p.min(100));
        ev.timeout_ms = ev.timeout_ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
        Some(ev)
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_follows_the_old_show_osd_rules() {
        assert_eq!(OsdEvent::new("Volume", "40%", Some(40), 2000, "").kind, OsdKind::Level);
        assert_eq!(OsdEvent::new("CONFIG ERROR", "line 3", None, 4000, "").kind, OsdKind::Error);
        assert_eq!(OsdEvent::new("Reloaded", "", None, 2000, "").kind, OsdKind::Info);
        // A bar wins over the word ERROR.
        assert_eq!(OsdEvent::new("ERROR rate", "", Some(5), 2000, "").kind, OsdKind::Level);
    }

    #[test]
    fn round_trip() {
        let ev = OsdEvent::new("Brightness", "70%", Some(70), 1500, "veil-host-1");
        assert_eq!(OsdEvent::from_json(&ev.to_json()), Some(ev));
    }

    #[test]
    fn receiver_clamps_untrusted_numbers() {
        let ev = OsdEvent::from_json(&serde_json::json!({
            "title": "x", "progress": 250, "timeout_ms": 4_000_000_000u64
        }))
        .unwrap();
        assert_eq!(ev.progress, Some(100));
        assert_eq!(ev.timeout_ms, MAX_TIMEOUT_MS);
        let ev = OsdEvent::from_json(&serde_json::json!({ "title": "x", "timeout_ms": 1 })).unwrap();
        assert_eq!(ev.timeout_ms, MIN_TIMEOUT_MS);
    }

    #[test]
    fn missing_fields_take_defaults_and_garbage_is_rejected() {
        let ev = OsdEvent::from_json(&serde_json::json!({ "title": "hi" })).unwrap();
        assert_eq!((ev.kind, ev.timeout_ms, ev.output.as_str()), (OsdKind::Info, DEFAULT_TIMEOUT_MS, ""));
        assert!(OsdEvent::from_json(&serde_json::json!({ "body": "no title" })).is_none());
        assert!(OsdEvent::from_json(&serde_json::json!("nope")).is_none());
    }
}
