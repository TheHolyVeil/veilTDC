//! What a notification is, independent of D-Bus. `dbus.rs` (step 3) fills
//! [`NotifyArgs`] from the wire; everything here is pure and testable.

use serde::{Deserialize, Serialize};

use crate::{markup, text};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

impl Urgency {
    /// The `urgency` hint byte: 0 low, 1 normal, 2 critical, anything else normal.
    pub fn from_hint(v: u8) -> Self {
        match v {
            0 => Urgency::Low,
            2 => Urgency::Critical,
            _ => Urgency::Normal,
        }
    }
}

/// Caps applied to everything an app sends. Defaults are proposals; they become
/// `notify.*` config keys when the crate is wired to `veil-config`.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_summary: usize,
    pub max_body: usize,
    pub max_image_edge: u32,
    pub max_actions: usize,
    pub history: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_summary: 256, max_body: 4096, max_image_edge: 128, max_actions: 4, history: 100 }
    }
}

const MAX_ACTION_KEY: usize = 256;
const MAX_ACTION_LABEL: usize = 64;
/// The action key apps use for "the user clicked the toast itself".
pub const DEFAULT_ACTION: &str = "default";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub key: String,
    pub label: String,
}

/// Turn the wire's flat `[key, label, key, label, ...]` into actions. Keys are
/// echoed back to the app in `ActionInvoked`, so a key that is too long is
/// dropped, never truncated. Duplicates keep the first. The `default` action
/// is kept and does not count against `max_actions`.
pub fn parse_actions(raw: &[String], max_buttons: usize) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    let mut buttons = 0;
    for [key, label] in raw.as_chunks::<2>().0 {
        if key.is_empty() || key.len() > MAX_ACTION_KEY || key.chars().any(char::is_control) {
            continue;
        }
        if out.iter().any(|a| a.key == *key) {
            continue;
        }
        let is_default = key == DEFAULT_ACTION;
        if !is_default {
            if buttons >= max_buttons {
                continue;
            }
            buttons += 1;
        }
        out.push(Action { key: key.clone(), label: text::clean_line(label, MAX_ACTION_LABEL) });
    }
    out
}

/// `image-data` exactly as the spec sends it: `(iiibiiay)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawImage {
    pub width: i32,
    pub height: i32,
    pub rowstride: i32,
    pub has_alpha: bool,
    pub bits_per_sample: i32,
    pub channels: i32,
    pub data: Vec<u8>,
}

/// Straight (not premultiplied) RGBA8, already scaled down to the cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImageError {
    #[error("bad image size {0}x{1}")]
    Dimensions(i32, i32),
    #[error("unsupported pixel format ({0} bits, {1} channels, alpha {2})")]
    Format(i32, i32, bool),
    #[error("rowstride is smaller than a row")]
    Rowstride,
    #[error("pixel data is shorter than the header says")]
    Truncated,
    #[error("image too large")]
    TooLarge,
}

/// Largest source edge we will even look at.
const MAX_SRC_EDGE: i32 = 4096;

impl Image {
    /// Validate and convert `raw`; scale down (nearest neighbour) so the long
    /// edge is at most `max_edge`. Header fields come from an untrusted app, so
    /// every size is checked before it is used as an index.
    pub fn from_raw(raw: &RawImage, max_edge: u32) -> Result<Image, ImageError> {
        let (w, h) = (raw.width, raw.height);
        if w <= 0 || h <= 0 || w > MAX_SRC_EDGE || h > MAX_SRC_EDGE {
            return Err(ImageError::Dimensions(w, h));
        }
        let format_ok = raw.bits_per_sample == 8
            && ((raw.channels == 3 && !raw.has_alpha) || (raw.channels == 4 && raw.has_alpha));
        if !format_ok {
            return Err(ImageError::Format(raw.bits_per_sample, raw.channels, raw.has_alpha));
        }
        let (w, h, ch) = (w as usize, h as usize, raw.channels as usize);
        let stride = usize::try_from(raw.rowstride).map_err(|_| ImageError::Rowstride)?;
        let row = w * ch;
        if stride < row {
            return Err(ImageError::Rowstride);
        }
        // The last row may stop at the end of its pixels, without padding.
        let need = stride.checked_mul(h - 1).and_then(|v| v.checked_add(row)).ok_or(ImageError::TooLarge)?;
        if raw.data.len() < need {
            return Err(ImageError::Truncated);
        }
        let max_edge = max_edge.max(1) as usize;
        let long = w.max(h);
        let (ow, oh) = if long <= max_edge { (w, h) } else { ((w * max_edge / long).max(1), (h * max_edge / long).max(1)) };
        let mut rgba = Vec::with_capacity(ow * oh * 4);
        for oy in 0..oh {
            let sy = oy * h / oh;
            for ox in 0..ow {
                let sx = ox * w / ow;
                let p = sy * stride + sx * ch;
                rgba.extend_from_slice(&raw.data[p..p + 3]);
                rgba.push(if ch == 4 { raw.data[p + 3] } else { 255 });
            }
        }
        Ok(Image { width: ow as u32, height: oh as u32, rgba })
    }
}

/// The hints we act on, already extracted from the wire dictionary.
#[derive(Clone, Debug, Default)]
pub struct Hints {
    pub urgency: Option<u8>,
    pub transient: bool,
    pub resident: bool,
    pub desktop_entry: Option<String>,
    pub image_data: Option<RawImage>,
    pub image_path: Option<String>,
}

/// The arguments of `Notify`, as received.
#[derive(Clone, Debug, Default)]
pub struct NotifyArgs {
    pub app_name: String,
    pub replaces_id: u32,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<String>,
    pub hints: Hints,
    pub expire_timeout: i32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    /// Plain text, one line.
    pub summary: String,
    /// Canonical markup, see [`markup`].
    pub body: String,
    pub actions: Vec<Action>,
    pub urgency: Urgency,
    pub transient: bool,
    pub resident: bool,
    pub desktop_entry: Option<String>,
    /// `image-path`, else `app_icon`. Resolved to pixels by the toast, step 5.
    pub icon: Option<String>,
    /// Never written to the history file: 128x128 RGBA per entry would bloat it.
    #[serde(skip)]
    pub image: Option<Image>,
    /// As requested: -1 server default, 0 never, otherwise milliseconds.
    pub timeout_ms: i32,
    /// Wall clock, unix milliseconds, for display only.
    pub created_ms: u64,
    pub read: bool,
}

impl Notification {
    pub fn from_args(id: u32, args: NotifyArgs, limits: &Limits, now_ms: u64) -> Notification {
        let hints = args.hints;
        let image = hints.image_data.as_ref().and_then(|raw| match Image::from_raw(raw, limits.max_image_edge) {
            Ok(img) => Some(img),
            Err(e) => {
                tracing::debug!("dropping image from {:?}: {e}", args.app_name);
                None
            }
        });
        let icon = hints.image_path.as_deref().and_then(text::clean_path).or_else(|| text::clean_path(&args.app_icon));
        Notification {
            id,
            app_name: text::clean_line(&args.app_name, 64),
            summary: text::clean_line(&args.summary, limits.max_summary),
            body: markup::sanitize(&args.body, limits.max_body),
            actions: parse_actions(&args.actions, limits.max_actions),
            urgency: hints.urgency.map(Urgency::from_hint).unwrap_or_default(),
            transient: hints.transient,
            resident: hints.resident,
            desktop_entry: hints.desktop_entry.map(|s| text::clean_line(&s, 128)).filter(|s| !s.is_empty()),
            icon,
            image,
            timeout_ms: args.expire_timeout.max(-1),
            created_ms: now_ms,
            read: false,
        }
    }

    /// Re-apply the caps to an entry read back from disk. The body is stored in
    /// canonical form, which is longer than the raw input, hence the headroom.
    pub fn normalize(&mut self, limits: &Limits) {
        self.app_name = text::clean_line(&self.app_name, 64);
        self.summary = text::clean_line(&self.summary, limits.max_summary);
        self.body = markup::sanitize(&self.body, limits.max_body.saturating_mul(4));
        self.actions.retain(|a| !a.key.is_empty() && a.key.len() <= MAX_ACTION_KEY);
        for a in &mut self.actions {
            a.label = text::clean_line(&a.label, MAX_ACTION_LABEL);
        }
        self.icon = self.icon.as_deref().and_then(text::clean_path);
        self.timeout_ms = self.timeout_ms.max(-1);
    }

    /// The action run when the toast itself is clicked.
    pub fn default_action(&self) -> Option<&Action> {
        self.actions.iter().find(|a| a.key == DEFAULT_ACTION)
    }

    /// Actions shown as buttons.
    pub fn buttons(&self) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(|a| a.key != DEFAULT_ACTION)
    }
}

#[cfg(test)]
pub(crate) fn test_notification(id: u32, summary: &str) -> Notification {
    Notification::from_args(id, NotifyArgs { app_name: "app".into(), summary: summary.into(), ..NotifyArgs::default() }, &Limits::default(), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn urgency_mapping() {
        assert_eq!(Urgency::from_hint(0), Urgency::Low);
        assert_eq!(Urgency::from_hint(1), Urgency::Normal);
        assert_eq!(Urgency::from_hint(2), Urgency::Critical);
        assert_eq!(Urgency::from_hint(9), Urgency::Normal);
    }

    #[test]
    fn actions_parse_pairs_and_apply_rules() {
        let a = parse_actions(&strs(&["default", "Open", "yes", "Yes", "yes", "dup", "", "empty", "no", "No", "odd"]), 4);
        let keys: Vec<_> = a.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["default", "yes", "no"]);
        // the default action is free; buttons are capped
        let a = parse_actions(&strs(&["a", "A", "b", "B", "default", "D", "c", "C"]), 2);
        let keys: Vec<_> = a.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["a", "b", "default"]);
        // an over-long key is dropped, not truncated
        let long = "k".repeat(300);
        assert!(parse_actions(&[long, "x".into()], 4).is_empty());
        // labels are plain, one line
        let a = parse_actions(&strs(&["k", " multi\nline  label "]), 4);
        assert_eq!(a[0].label, "multi line label");
    }

    fn rgb(w: i32, h: i32, stride: i32, data: Vec<u8>) -> RawImage {
        RawImage { width: w, height: h, rowstride: stride, has_alpha: false, bits_per_sample: 8, channels: 3, data }
    }

    #[test]
    fn image_converts_rgb_to_rgba() {
        let img = Image::from_raw(&rgb(2, 1, 6, vec![1, 2, 3, 4, 5, 6]), 128).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.rgba, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn image_honours_rowstride_padding_and_short_last_row() {
        // 1x2 RGB, stride 4 (one pad byte), last row without padding
        let img = Image::from_raw(&rgb(1, 2, 4, vec![1, 2, 3, 0, 7, 8, 9]), 128).unwrap();
        assert_eq!(img.rgba, vec![1, 2, 3, 255, 7, 8, 9, 255]);
    }

    #[test]
    fn image_keeps_alpha() {
        let raw = RawImage { width: 1, height: 1, rowstride: 4, has_alpha: true, bits_per_sample: 8, channels: 4, data: vec![9, 8, 7, 6] };
        assert_eq!(Image::from_raw(&raw, 128).unwrap().rgba, vec![9, 8, 7, 6]);
    }

    #[test]
    fn image_rejects_bad_headers() {
        assert_eq!(Image::from_raw(&rgb(0, 1, 3, vec![0; 3]), 128), Err(ImageError::Dimensions(0, 1)));
        assert_eq!(Image::from_raw(&rgb(5000, 1, 15000, vec![]), 128), Err(ImageError::Dimensions(5000, 1)));
        assert_eq!(Image::from_raw(&rgb(2, 1, 5, vec![0; 6]), 128), Err(ImageError::Rowstride));
        assert_eq!(Image::from_raw(&rgb(2, 1, -1, vec![0; 6]), 128), Err(ImageError::Rowstride));
        assert_eq!(Image::from_raw(&rgb(2, 2, 6, vec![0; 11]), 128), Err(ImageError::Truncated));
        let mut raw = rgb(1, 1, 3, vec![0; 3]);
        raw.bits_per_sample = 16;
        assert!(matches!(Image::from_raw(&raw, 128), Err(ImageError::Format(..))));
        let mut raw = rgb(1, 1, 4, vec![0; 4]);
        raw.channels = 4; // 4 channels but has_alpha false
        assert!(matches!(Image::from_raw(&raw, 128), Err(ImageError::Format(..))));
    }

    #[test]
    fn image_scales_down_and_keeps_aspect() {
        let img = Image::from_raw(&rgb(8, 4, 24, vec![7; 8 * 4 * 3]), 4).unwrap();
        assert_eq!((img.width, img.height), (4, 2));
        assert_eq!(img.rgba.len(), 4 * 2 * 4);
        let img = Image::from_raw(&rgb(400, 1, 1200, vec![0; 1200]), 128).unwrap();
        assert_eq!((img.width, img.height), (128, 1), "never collapses to zero");
    }

    #[test]
    fn from_args_sanitises_everything() {
        let args = NotifyArgs {
            app_name: " Mail\n".into(),
            app_icon: "mail-icon".into(),
            summary: "Hi\nthere\u{202E}".into(),
            body: "<b>x</b><a href=\"u\">y</a>".into(),
            actions: strs(&["default", "Open"]),
            hints: Hints { urgency: Some(2), transient: true, image_path: Some("/tmp/a.png".into()), ..Hints::default() },
            expire_timeout: -7,
            ..NotifyArgs::default()
        };
        let n = Notification::from_args(5, args, &Limits::default(), 42);
        assert_eq!(n.id, 5);
        assert_eq!(n.app_name, "Mail");
        assert_eq!(n.summary, "Hi there");
        assert_eq!(n.body, "<b>x</b>y");
        assert_eq!(n.urgency, Urgency::Critical);
        assert!(n.transient && !n.resident);
        assert_eq!(n.icon.as_deref(), Some("/tmp/a.png"), "image-path wins over app_icon");
        assert_eq!(n.timeout_ms, -1);
        assert_eq!(n.created_ms, 42);
        assert!(n.default_action().is_some());
        assert_eq!(n.buttons().count(), 0);
        assert!(!n.read);
    }

    #[test]
    fn from_args_caps_lengths_and_survives_a_bad_image() {
        let args = NotifyArgs {
            summary: "s".repeat(1000),
            body: "b".repeat(10_000),
            hints: Hints { image_data: Some(rgb(0, 0, 0, vec![])), ..Hints::default() },
            ..NotifyArgs::default()
        };
        let n = Notification::from_args(1, args, &Limits::default(), 0);
        assert!(n.summary.len() <= 256 + 3);
        assert!(n.body.len() <= 4096 + 3);
        assert!(n.image.is_none());
    }
}
