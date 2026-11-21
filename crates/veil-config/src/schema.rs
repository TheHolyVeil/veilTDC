//! Every setting the settings app can edit, in one table.
//!
//! The parsers in `lib.rs` stay the source of truth for what a key *means*
//! and what it defaults to (`VeilConfig::default()`); this table adds what a
//! UI and a writer need on top: the value's type, its allowed range or
//! choices, which settings tab it lives on, and a line of help. A path may
//! contain `*` for one user-chosen segment (`bar.widgets.custom.*.command`).
//!
//! `config.set` validates against this before anything touches the file, so
//! a caller can only write keys listed here, with values of the right shape.

use crate::{parse_hex_color, VeilConfig};

/// What a setting holds, with its limits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Bool,
    Int { min: i64, max: i64 },
    Float { min: f64, max: f64 },
    /// One of a fixed set of strings (compared case-insensitively, stored
    /// in the spelling listed here).
    Choice(&'static [&'static str]),
    /// Free text, no control characters.
    Text,
    /// A list of free-text strings.
    TextList,
    /// `#rrggbb`.
    Color,
}

/// One editable setting.
#[derive(Debug, Clone, Copy)]
pub struct SettingDef {
    /// Dotted path in config.toml; `*` stands for one user-chosen segment.
    pub path: &'static str,
    pub kind: Kind,
    /// Settings-app tab this belongs on.
    pub tab: &'static str,
    pub help: &'static str,
    /// The value is run as a shell command. Callers that expose `config.set`
    /// more widely than the settings app should refuse these.
    pub exec: bool,
}

/// A value on its way into config.toml. Deliberately only literals: there
/// is no way to express anything the TOML parser would treat as code.
#[derive(Debug, Clone, PartialEq)]
pub enum EditValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<String>),
}

const MAX_TEXT: usize = 1024;
const MAX_LIST: usize = 64;
/// Longest user-chosen path segment (a custom widget or blur namespace name).
const MAX_SEGMENT: usize = 64;

macro_rules! def {
    ($path:expr, $kind:expr, $tab:expr, $help:expr) => {
        SettingDef { path: $path, kind: $kind, tab: $tab, help: $help, exec: false }
    };
}

pub const THEMES: &[&str] = &[
    "default", "nord", "dracula", "catppuccin", "gruvbox", "everforest", "tokyonight",
    "solarized", "rosepine", "monokai", "onedark", "deepsage", "wallpaper",
];

const TAB_THEME: &str = "Wallpaper and theme";
const TAB_FONTS: &str = "Fonts";
const TAB_INPUT: &str = "Input";
const TAB_BAR: &str = "Bar";
const TAB_NOTIFY: &str = "Notifications";
const TAB_POWER: &str = "Power and lock";
const TAB_GENERAL: &str = "General";

pub static SETTINGS: &[SettingDef] = &[
    // ── Wallpaper and theme ────────────────────────────────────────────────
    def!("theme", Kind::Choice(THEMES), TAB_THEME, "Colour preset for the shell. `wallpaper` derives colours from the wallpaper."),
    def!("theme_mode", Kind::Choice(&["dark", "light", "auto"]), TAB_THEME, "Only used by the `wallpaper` theme."),
    def!("background", Kind::Color, TAB_THEME, "Colour shown where no window or wallpaper covers the screen."),
    def!("wallpaper.path", Kind::Text, TAB_THEME, "A single image. `~` is expanded."),
    def!("wallpaper.paths", Kind::TextList, TAB_THEME, "Several images; combine with interval and shuffle."),
    def!("wallpaper.interval", Kind::Int { min: 0, max: 86_400 }, TAB_THEME, "Seconds between wallpapers; 0 keeps the first."),
    def!("wallpaper.shuffle", Kind::Bool, TAB_THEME, "Pick wallpapers in random order."),
    def!("wallpaper.fit", Kind::Choice(&["fill", "fit", "stretch"]), TAB_THEME, "How the image is scaled to the screen."),
    // ── Fonts ───────────────────────────────────────────────────────────────
    def!("font.family", Kind::Text, TAB_FONTS, "UI font: a family name or an absolute path."),
    def!("font.mono", Kind::Text, TAB_FONTS, "Monospace font."),
    def!("font.icons", Kind::Text, TAB_FONTS, "Icon font."),
    def!("font.fallback", Kind::TextList, TAB_FONTS, "Tried in order for characters the main font lacks."),
    def!("font.size", Kind::Float { min: 8.0, max: 48.0 }, TAB_FONTS, "Base size in pixels; surfaces scale their text from it."),
    def!("font.weight", Kind::Choice(&["regular", "bold"]), TAB_FONTS, "Weight of the UI font."),
    // ── Input ───────────────────────────────────────────────────────────────
    def!("keyboard.layout", Kind::Text, TAB_INPUT, "XKB layout, e.g. `us` or `us,de`. Empty uses the system default."),
    def!("keyboard.variant", Kind::Text, TAB_INPUT, "XKB variant, e.g. `dvorak`."),
    def!("keyboard.options", Kind::Text, TAB_INPUT, "XKB options, e.g. `caps:escape`."),
    def!("keyboard.repeat_delay", Kind::Int { min: 100, max: 2000 }, TAB_INPUT, "Milliseconds before a held key starts repeating."),
    def!("keyboard.repeat_rate", Kind::Int { min: 0, max: 100 }, TAB_INPUT, "Repeats per second; 0 turns repeat off."),
    def!("touchpad.dwt", Kind::Bool, TAB_INPUT, "Ignore the touchpad briefly after typing. `VEIL_DWT` overrides."),
    def!("touchpad.dwt_timeout", Kind::Int { min: 50, max: 5000 }, TAB_INPUT, "Milliseconds the pad stays off after the last typed key."),
    // ── Bar ─────────────────────────────────────────────────────────────────
    def!("bar.enabled", Kind::Bool, TAB_BAR, "Show the bar."),
    def!("bar.position", Kind::Choice(&["top", "bottom"]), TAB_BAR, "Which screen edge the bar sits on."),
    def!("bar.shell", Kind::Bool, TAB_BAR, "Use the Veil shell bar rather than an external one."),
    def!("bar.height", Kind::Int { min: 24, max: 64 }, TAB_BAR, "Bar height in pixels."),
    def!("bar.left", Kind::TextList, TAB_BAR, "Widgets on the left, in order."),
    def!("bar.center", Kind::TextList, TAB_BAR, "Widgets in the middle, in order."),
    def!("bar.right", Kind::TextList, TAB_BAR, "Widgets on the right, in order."),
    def!("bar.widgets.clock.format", Kind::Text, TAB_BAR, "strftime-style clock format."),
    def!("bar.widgets.disk.mount", Kind::Text, TAB_BAR, "Mount point the disk widget reports."),
    // Weather needs both coordinates; write lat and lon in one batch.
    def!("bar.widgets.weather.lat", Kind::Float { min: -90.0, max: 90.0 }, TAB_BAR, "Latitude. The weather widget needs lat and lon together."),
    def!("bar.widgets.weather.lon", Kind::Float { min: -180.0, max: 180.0 }, TAB_BAR, "Longitude."),
    def!("bar.widgets.weather.units", Kind::Text, TAB_BAR, "Units for the weather widget; `metric` by default."),
    SettingDef {
        path: "bar.widgets.custom.*.command",
        kind: Kind::Text,
        tab: TAB_BAR,
        help: "Shell command whose output the widget shows.",
        exec: true,
    },
    def!("bar.widgets.custom.*.interval", Kind::Int { min: 1, max: 86_400 }, TAB_BAR, "Seconds between runs of the command."),
    // ── Notifications ───────────────────────────────────────────────────────
    def!("notify.shell", Kind::Bool, TAB_NOTIFY, "Use the Veil notification daemon."),
    def!("notify.timeout", Kind::Int { min: 0, max: 3_600_000 }, TAB_NOTIFY, "Default on-screen time in milliseconds; 0 never expires."),
    def!("notify.max_visible", Kind::Int { min: 1, max: 10 }, TAB_NOTIFY, "How many notifications show at once."),
    def!("notify.dnd_critical", Kind::Bool, TAB_NOTIFY, "Show critical notifications even in do-not-disturb."),
    def!("osd.shell", Kind::Bool, TAB_NOTIFY, "Use the Veil on-screen display for volume and brightness."),
    // ── Power and lock ──────────────────────────────────────────────────────
    def!("idle.enabled", Kind::Bool, TAB_POWER, "Master switch for the idle timers."),
    def!("idle.lock_timeout", Kind::Int { min: 0, max: 86_400 }, TAB_POWER, "Seconds idle before locking; 0 disables."),
    def!("idle.dpms_timeout", Kind::Int { min: 0, max: 86_400 }, TAB_POWER, "Seconds idle before the display blanks; 0 disables."),
    def!("idle.suspend_timeout", Kind::Int { min: 0, max: 86_400 }, TAB_POWER, "Seconds idle before suspending; 0 disables."),
    def!("idle.lid_action", Kind::Choice(&["nothing", "lock", "suspend", "lock+suspend"]), TAB_POWER, "What closing the lid does."),
    // ── General ─────────────────────────────────────────────────────────────
    def!("default_layout", Kind::Choice(&["dwindle", "scroller"]), TAB_GENERAL, "Tiling layout new workspaces start in."),
    def!("fps", Kind::Int { min: 0, max: 1000 }, TAB_GENERAL, "Frame-rate cap; 0 matches the display."),
    def!("refresh", Kind::Text, TAB_GENERAL, "Display refresh: `preferred`, `max`, or a number of Hz."),
    def!("clipboard_sync", Kind::Bool, TAB_GENERAL, "Share the host clipboard with clients. Turn off for privacy."),
    def!("gpu_render", Kind::Bool, TAB_GENERAL, "Use GPU compute shaders for frame encoding."),
    def!("gpu_power", Kind::Choice(&["high", "low"]), TAB_GENERAL, "Which GPU to prefer; `low` picks the integrated one."),
    def!("blur.enabled", Kind::Bool, TAB_GENERAL, "Blur behind translucent surfaces."),
    def!("blur.radius", Kind::Int { min: 1, max: 32 }, TAB_GENERAL, "Blur strength."),
    def!("blur.windows", Kind::Bool, TAB_GENERAL, "Blur behind ordinary windows too."),
    def!("blur.namespaces.*", Kind::Bool, TAB_GENERAL, "Per-layer-surface blur switch."),
];

/// What `cfg` currently holds for a setting, in the shape the writer takes.
/// A setting that is unset shows what Veil then uses: `""` for text that
/// falls back to a default (`font.family`), the default size for
/// `font.size`. `None` means there is nothing to show: a `*` path (those are
/// per-name tables, not single settings), `wallpaper.path` (merged into
/// `wallpaper.paths` when the file is read), or weather coordinates when no
/// location is configured.
pub fn current(cfg: &VeilConfig, path: &str) -> Option<EditValue> {
    use EditValue::{Bool, Float, Int, List, Str};
    let s = |t: &str| Str(t.to_string());
    let list = |v: &[String]| List(v.to_vec());
    let wp = cfg.wallpaper.as_ref();
    let weather = cfg.bar.weather.as_ref();
    Some(match path {
        "theme" => s(cfg.theme_name.label()),
        "theme_mode" => s(cfg.theme_mode.label()),
        "background" => {
            let [r, g, b] = cfg.background;
            Str(format!("#{r:02x}{g:02x}{b:02x}"))
        }
        "wallpaper.paths" => List(wp.map(|w| w.paths.iter().map(|p| p.display().to_string()).collect()).unwrap_or_default()),
        "wallpaper.interval" => Int(wp.map_or(0, |w| w.interval_secs).min(i64::MAX as u64) as i64),
        "wallpaper.shuffle" => Bool(wp.is_some_and(|w| w.shuffle)),
        "wallpaper.fit" => s(wp.map_or("fill", |w| w.fit.label())),
        "font.family" => Str(cfg.font.family.clone().unwrap_or_default()),
        "font.mono" => Str(cfg.font.mono.clone().unwrap_or_default()),
        "font.icons" => Str(cfg.font.icons.clone().unwrap_or_default()),
        "font.fallback" => list(&cfg.font.fallback),
        // 14 px is `veil_text::BASE_SIZE`; veil-config does not depend on veil-text.
        "font.size" => Float(f64::from(cfg.font.size.unwrap_or(14.0))),
        "font.weight" => s(cfg.font.weight.label()),
        "keyboard.layout" => Str(cfg.keyboard.layout.clone()),
        "keyboard.variant" => Str(cfg.keyboard.variant.clone()),
        "keyboard.options" => Str(cfg.keyboard.options.clone()),
        "keyboard.repeat_delay" => Int(i64::from(cfg.keyboard.repeat_delay_ms)),
        "keyboard.repeat_rate" => Int(i64::from(cfg.keyboard.repeat_rate)),
        "touchpad.dwt" => Bool(cfg.touchpad.dwt),
        "touchpad.dwt_timeout" => Int(i64::from(cfg.touchpad.dwt_timeout_ms)),
        "bar.enabled" => Bool(cfg.bar.enabled),
        "bar.position" => s(cfg.bar.position.label()),
        "bar.shell" => Bool(cfg.bar.shell),
        "bar.height" => Int(i64::from(cfg.bar.height)),
        "bar.left" => list(&cfg.bar.left),
        "bar.center" => list(&cfg.bar.center),
        "bar.right" => list(&cfg.bar.right),
        "bar.widgets.clock.format" => Str(cfg.bar.clock_format.clone()),
        "bar.widgets.disk.mount" => Str(cfg.bar.disk_mount.clone()),
        "bar.widgets.weather.lat" => Float(weather?.lat),
        "bar.widgets.weather.lon" => Float(weather?.lon),
        "bar.widgets.weather.units" => Str(weather?.units.clone()),
        "notify.shell" => Bool(cfg.notify.shell),
        "notify.timeout" => Int(i64::from(cfg.notify.timeout_ms)),
        "notify.max_visible" => Int(i64::from(cfg.notify.max_visible)),
        "notify.dnd_critical" => Bool(cfg.notify.dnd_critical),
        "osd.shell" => Bool(cfg.osd.shell),
        "idle.enabled" => Bool(cfg.idle.enabled),
        "idle.lock_timeout" => Int(i64::from(cfg.idle.lock_timeout)),
        "idle.dpms_timeout" => Int(i64::from(cfg.idle.dpms_timeout)),
        "idle.suspend_timeout" => Int(i64::from(cfg.idle.suspend_timeout)),
        "idle.lid_action" => s(cfg.idle.lid_action.as_str()),
        "default_layout" => s(cfg.default_layout.as_str()),
        "fps" => Int(i64::from(cfg.fps)),
        "refresh" => Str(cfg.refresh.as_text()),
        "clipboard_sync" => Bool(cfg.clipboard_sync),
        "gpu_render" => Bool(cfg.gpu_render),
        "gpu_power" => s(cfg.gpu_power.as_str()),
        "blur.enabled" => Bool(cfg.blur.enabled),
        "blur.radius" => Int(i64::from(cfg.blur.radius)),
        "blur.windows" => Bool(cfg.blur.windows),
        _ => return None,
    })
}

fn segment_ok(seg: &str) -> bool {
    !seg.is_empty()
        && seg.len() <= MAX_SEGMENT
        && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The definition for a dotted `path`, matching `*` segments.
pub fn find(path: &str) -> Option<&'static SettingDef> {
    let want: Vec<&str> = path.split('.').collect();
    if !want.iter().all(|s| segment_ok(s)) {
        return None;
    }
    SETTINGS.iter().find(|d| {
        let have: Vec<&str> = d.path.split('.').collect();
        have.len() == want.len() && have.iter().zip(&want).all(|(h, w)| *h == "*" || h == w)
    })
}

/// Validate `value` for `path`, returning it in the form to store (a
/// `Choice` in its listed spelling, a `Color` as lowercase `#rrggbb`, an
/// integer given for a float setting as a float).
pub fn check(path: &str, value: &EditValue) -> Result<EditValue, String> {
    find(path)
        .ok_or_else(|| format!("unknown setting `{path}`"))?
        .check(value)
}

fn text_ok(path: &str, s: &str) -> Result<(), String> {
    if s.len() > MAX_TEXT {
        return Err(format!("`{path}` is longer than {MAX_TEXT} bytes"));
    }
    if s.chars().any(char::is_control) {
        return Err(format!("`{path}` must not contain control characters"));
    }
    Ok(())
}

impl SettingDef {
    pub fn check(&self, value: &EditValue) -> Result<EditValue, String> {
        let path = self.path;
        match (self.kind, value) {
            (Kind::Bool, EditValue::Bool(_)) => Ok(value.clone()),
            (Kind::Int { min, max }, EditValue::Int(n)) => {
                if (min..=max).contains(n) {
                    Ok(value.clone())
                } else {
                    Err(format!("`{path}` must be {min} to {max}"))
                }
            }
            (Kind::Float { min, max }, EditValue::Float(f)) => {
                if f.is_finite() && (min..=max).contains(f) {
                    Ok(value.clone())
                } else {
                    Err(format!("`{path}` must be {min} to {max}"))
                }
            }
            (Kind::Float { .. }, EditValue::Int(n)) => self.check(&EditValue::Float(*n as f64)),
            (Kind::Choice(options), EditValue::Str(s)) => options
                .iter()
                .find(|o| o.eq_ignore_ascii_case(s.trim()))
                .map(|o| EditValue::Str((*o).to_string()))
                .ok_or_else(|| format!("`{path}` must be one of: {}", options.join(", "))),
            (Kind::Text, EditValue::Str(s)) => text_ok(path, s).map(|()| value.clone()),
            (Kind::TextList, EditValue::List(items)) => {
                if items.len() > MAX_LIST {
                    return Err(format!("`{path}` takes at most {MAX_LIST} entries"));
                }
                items.iter().try_for_each(|s| text_ok(path, s))?;
                Ok(value.clone())
            }
            (Kind::Color, EditValue::Str(s)) => parse_hex_color(s)
                .map(|[r, g, b]| EditValue::Str(format!("#{r:02x}{g:02x}{b:02x}")))
                .ok_or_else(|| format!("`{path}` must be a colour like #1a2b3c")),
            _ => Err(format!("`{path}` was given the wrong kind of value")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_unique_and_known_to_the_linter() {
        for (i, a) in SETTINGS.iter().enumerate() {
            for b in &SETTINGS[i + 1..] {
                assert_ne!(a.path, b.path, "duplicate setting");
            }
            let top = a.path.split('.').next().unwrap();
            assert!(crate::TOP_KEYS.contains(&top), "`{}` is not a known top-level key", a.path);
        }
    }

    #[test]
    fn every_path_resolves_to_itself() {
        for d in SETTINGS {
            let concrete = d.path.replace('*', "name");
            assert_eq!(find(&concrete).map(|f| f.path), Some(d.path));
        }
    }

    #[test]
    fn rejects_unknown_and_malformed_paths() {
        assert!(find("nope").is_none());
        assert!(find("bar.height.extra").is_none());
        assert!(find("bar..height").is_none());
        assert!(find("bar.widgets.custom..command").is_none());
        assert!(find("bar.widgets.custom.a b.command").is_none());
        assert!(check("nope", &EditValue::Bool(true)).is_err());
    }

    #[test]
    fn checks_ranges_and_normalises() {
        assert!(check("bar.height", &EditValue::Int(40)).is_ok());
        assert!(check("bar.height", &EditValue::Int(5)).is_err());
        assert!(check("bar.height", &EditValue::Bool(true)).is_err());
        assert_eq!(
            check("font.size", &EditValue::Int(16)).unwrap(),
            EditValue::Float(16.0)
        );
        assert!(check("font.size", &EditValue::Float(f64::NAN)).is_err());
        assert_eq!(
            check("theme", &EditValue::Str(" Nord ".into())).unwrap(),
            EditValue::Str("nord".into())
        );
        assert!(check("theme", &EditValue::Str("neon".into())).is_err());
        assert_eq!(
            check("background", &EditValue::Str("8C8C8C".into())).unwrap(),
            EditValue::Str("#8c8c8c".into())
        );
        assert!(check("background", &EditValue::Str("grey".into())).is_err());
    }

    #[test]
    fn text_rejects_control_characters_and_overlong_lists() {
        assert!(check("keyboard.layout", &EditValue::Str("us".into())).is_ok());
        assert!(check("keyboard.layout", &EditValue::Str("us\nbar".into())).is_err());
        let long = EditValue::List(vec!["a".into(); MAX_LIST + 1]);
        assert!(check("bar.left", &long).is_err());
    }

    #[test]
    fn custom_widget_command_is_flagged_as_exec() {
        assert!(find("bar.widgets.custom.uptime.command").unwrap().exec);
        assert!(!find("bar.widgets.custom.uptime.interval").unwrap().exec);
        assert_eq!(SETTINGS.iter().filter(|d| d.exec).count(), 1);
    }

    /// Settings with no single current value; see `current`.
    fn has_no_current(path: &str) -> bool {
        path.contains('*') || path == "wallpaper.path" || path.starts_with("bar.widgets.weather.")
    }

    #[test]
    fn every_setting_shows_a_current_value_the_schema_accepts() {
        // If a label in lib.rs and a choice here ever drift apart, the app
        // would show a value it then refuses to write back. Catch it here.
        let cfg = VeilConfig::default();
        for d in SETTINGS.iter().filter(|d| !has_no_current(d.path)) {
            let cur = current(&cfg, d.path).unwrap_or_else(|| panic!("no current value for `{}`", d.path));
            assert!(d.check(&cur).is_ok(), "`{}` shows {cur:?}, which the schema rejects: {:?}", d.path, d.check(&cur));
        }
    }

    #[test]
    fn current_follows_a_parsed_config() {
        let cfg = crate::parse_config(
            "theme = \"nord\"\nbackground = \"#102030\"\nrefresh = 144\n\
             [font]\nfamily = \"Inter\"\nsize = 16\nweight = \"bold\"\n\
             [keyboard]\nlayout = \"de\"\nrepeat_rate = 30\n\
             [idle]\nlid_action = \"lock\"\n\
             [wallpaper]\npaths = [\"/a.png\"]\ninterval = 60\nfit = \"fit\"\n\
             [bar]\nposition = \"top\"\nleft = [\"clock\"]\n\
             [bar.widgets.weather]\nlat = 52.5\nlon = 13.4\n",
        )
        .unwrap();
        let get = |p: &str| current(&cfg, p).unwrap_or_else(|| panic!("{p}"));
        assert_eq!(get("theme"), EditValue::Str("nord".into()));
        assert_eq!(get("background"), EditValue::Str("#102030".into()));
        assert_eq!(get("refresh"), EditValue::Str("144".into()));
        assert_eq!(get("font.family"), EditValue::Str("Inter".into()));
        assert_eq!(get("font.size"), EditValue::Float(16.0));
        assert_eq!(get("font.weight"), EditValue::Str("bold".into()));
        assert_eq!(get("keyboard.layout"), EditValue::Str("de".into()));
        assert_eq!(get("keyboard.repeat_rate"), EditValue::Int(30));
        assert_eq!(get("idle.lid_action"), EditValue::Str("lock".into()));
        assert_eq!(get("wallpaper.paths"), EditValue::List(vec!["/a.png".into()]));
        assert_eq!(get("wallpaper.interval"), EditValue::Int(60));
        assert_eq!(get("wallpaper.fit"), EditValue::Str("fit".into()));
        assert_eq!(get("bar.position"), EditValue::Str("top".into()));
        assert_eq!(get("bar.left"), EditValue::List(vec!["clock".into()]));
        assert_eq!(get("bar.widgets.weather.lat"), EditValue::Float(52.5));
        assert_eq!(get("bar.widgets.weather.lon"), EditValue::Float(13.4));
        // No location configured: nothing to show for the coordinates.
        assert_eq!(current(&VeilConfig::default(), "bar.widgets.weather.lat"), None);
        assert_eq!(current(&VeilConfig::default(), "bar.widgets.custom.x.command"), None);
    }
}
