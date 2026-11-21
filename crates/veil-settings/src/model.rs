//! What the settings window shows and how a value changes, with no drawing
//! and no I/O: which tab a setting is on, how it is labelled and displayed,
//! what Left/Right does to it, how typed text becomes a value, and what is
//! sent to the compositor to save it.

use serde_json::{json, Value};
use veil_config::schema::{EditValue, Kind, SettingDef, SETTINGS};

/// Tabs in the order they appear. A tab with no settings left after
/// filtering is dropped, so this can name tabs ahead of their settings.
const TAB_ORDER: &[&str] = &[
    "Wallpaper and theme",
    "Fonts",
    "Input",
    "Bar",
    "Notifications",
    "Power and lock",
    "General",
];

pub struct TabDef {
    pub name: &'static str,
    pub defs: Vec<&'static SettingDef>,
}

/// Settings the window edits, grouped by tab, in schema order within a tab.
///
/// Left out: paths with a `*` (a table per user-chosen name, such as a custom
/// widget; there is no single row for those) and `wallpaper.path` (the
/// `wallpaper.paths` row covers it and clears it on save).
pub fn tabs() -> Vec<TabDef> {
    TAB_ORDER
        .iter()
        .map(|name| TabDef {
            name,
            defs: SETTINGS
                .iter()
                .filter(|d| d.tab == *name && !d.path.contains('*') && d.path != "wallpaper.path")
                .collect(),
        })
        .filter(|t| !t.defs.is_empty())
        .collect()
}

/// `bar.widgets.weather.lat` → `Weather lat`; `keyboard.repeat_rate` →
/// `Keyboard repeat rate`.
pub fn label(path: &str) -> String {
    let path = path.strip_prefix("bar.widgets.").unwrap_or(path);
    let spaced = path.replace(['.', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// How a value reads in the window.
pub fn show(value: &EditValue) -> String {
    match value {
        EditValue::Bool(true) => "on".into(),
        EditValue::Bool(false) => "off".into(),
        EditValue::Int(n) => n.to_string(),
        EditValue::Float(f) => f.to_string(),
        EditValue::Str(s) if s.is_empty() => "(default)".into(),
        EditValue::Str(s) => s.clone(),
        EditValue::List(items) if items.is_empty() => "(none)".into(),
        EditValue::List(items) => items.join(", "),
    }
}

/// The text to start from when editing a value in place (no placeholders).
pub fn edit_text(value: &EditValue) -> String {
    match value {
        EditValue::Str(s) => s.clone(),
        EditValue::List(items) => items.join(", "),
        other => show(other),
    }
}

/// Whether Enter edits this setting as text (as opposed to toggling or cycling it).
pub fn is_text_kind(def: &SettingDef) -> bool {
    !matches!(def.kind, Kind::Bool | Kind::Choice(_))
}

fn int_step(min: i64, max: i64) -> i64 {
    let range = max.saturating_sub(min).max(1) as u64;
    10_i64.saturating_pow(range.ilog10().saturating_sub(2)).max(1)
}

/// What Left (`dir < 0`) or Right (`dir > 0`) does: flip a switch, cycle a
/// choice, nudge a number within its limits. `None` when nothing changes
/// (already at a limit, or the setting is text).
pub fn step(def: &SettingDef, current: &EditValue, dir: i32) -> Option<EditValue> {
    let dir = i64::from(dir.signum());
    if dir == 0 {
        return None;
    }
    match (def.kind, current) {
        (Kind::Bool, EditValue::Bool(b)) => Some(EditValue::Bool(!b)),
        (Kind::Int { min, max }, EditValue::Int(n)) => {
            let next = n.saturating_add(dir * int_step(min, max)).clamp(min, max);
            (next != *n).then_some(EditValue::Int(next))
        }
        (Kind::Float { min, max }, EditValue::Float(f)) => {
            let unit = if max - min <= 100.0 { 0.5 } else { 1.0 };
            // Round to one decimal so repeated nudges never pick up float noise
            // (0.1 + 0.2 ...), without snapping a typed 52.3 onto a grid.
            let next = (((f + dir as f64 * unit) * 10.0).round() / 10.0).clamp(min, max);
            ((next - f).abs() > f64::EPSILON).then_some(EditValue::Float(next))
        }
        (Kind::Choice(options), EditValue::Str(s)) => {
            let at = options.iter().position(|o| o.eq_ignore_ascii_case(s)).map_or(0, |i| i as i64);
            let next = (at + dir).rem_euclid(options.len() as i64) as usize;
            Some(EditValue::Str(options[next].to_string()))
        }
        _ => None,
    }
}

/// Turn what was typed into a value of the right kind. Range and choice
/// checks are the schema's (`SettingDef::check`); this only fixes the type.
pub fn parse_input(def: &SettingDef, text: &str) -> Result<EditValue, String> {
    let t = text.trim();
    match def.kind {
        Kind::Bool => match t.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => Ok(EditValue::Bool(true)),
            "false" | "off" | "no" | "0" => Ok(EditValue::Bool(false)),
            _ => Err("type on or off".into()),
        },
        Kind::Int { .. } => t.parse::<i64>().map(EditValue::Int).map_err(|_| "type a whole number".into()),
        Kind::Float { .. } => t
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(EditValue::Float)
            .ok_or_else(|| "type a number".into()),
        Kind::Choice(_) | Kind::Color => Ok(EditValue::Str(t.to_string())),
        Kind::Text => Ok(EditValue::Str(text.to_string())),
        Kind::TextList => Ok(EditValue::List(
            text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        )),
    }
}

fn to_json(value: &EditValue) -> Value {
    match value {
        EditValue::Bool(b) => json!(b),
        EditValue::Int(n) => json!(n),
        EditValue::Float(f) => json!(f),
        EditValue::Str(s) => json!(s),
        EditValue::List(items) => json!(items),
    }
}

/// The `config.set` arguments that save `value` for `path`, or reset it to
/// its default when `value` is `None`.
///
/// `wallpaper.paths` also clears `wallpaper.path`: the file may hold the
/// single-image spelling, and both together would show the old image too.
pub fn request(path: &str, value: Option<&EditValue>) -> Value {
    let also_clear = path == "wallpaper.paths";
    match value {
        Some(v) => {
            let mut args = json!({ "set": { path: to_json(v) } });
            if also_clear {
                args["unset"] = json!(["wallpaper.path"]);
            }
            args
        }
        None => {
            let mut paths = vec![path];
            if also_clear {
                paths.push("wallpaper.path");
            }
            json!({ "unset": paths })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veil_config::schema::find;

    fn def(path: &str) -> &'static SettingDef {
        find(path).unwrap_or_else(|| panic!("{path}"))
    }

    #[test]
    fn tabs_follow_the_order_and_hide_what_has_no_row() {
        let t = tabs();
        let names: Vec<_> = t.iter().map(|t| t.name).collect();
        assert_eq!(names, TAB_ORDER, "every named tab has settings today");
        let all: Vec<_> = t.iter().flat_map(|t| t.defs.iter().map(|d| d.path)).collect();
        assert!(all.iter().all(|p| !p.contains('*')), "no per-name tables");
        assert!(!all.contains(&"wallpaper.path"));
        assert!(all.contains(&"wallpaper.paths") && all.contains(&"keyboard.layout"));
        // Nothing listed twice, nothing dropped except the two exclusions.
        let eligible = SETTINGS.iter().filter(|d| !d.path.contains('*') && d.path != "wallpaper.path").count();
        assert_eq!(all.len(), eligible);
    }

    #[test]
    fn labels_read_like_words() {
        assert_eq!(label("keyboard.repeat_rate"), "Keyboard repeat rate");
        assert_eq!(label("bar.widgets.weather.lat"), "Weather lat");
        assert_eq!(label("fps"), "Fps");
        assert_eq!(label(""), "");
    }

    #[test]
    fn values_display_plainly() {
        assert_eq!(show(&EditValue::Bool(true)), "on");
        assert_eq!(show(&EditValue::Float(14.0)), "14");
        assert_eq!(show(&EditValue::Float(14.5)), "14.5");
        assert_eq!(show(&EditValue::Str(String::new())), "(default)");
        assert_eq!(show(&EditValue::List(vec![])), "(none)");
        assert_eq!(show(&EditValue::List(vec!["a".into(), "b".into()])), "a, b");
        // Editing starts from the real text, never a placeholder.
        assert_eq!(edit_text(&EditValue::Str(String::new())), "");
        assert_eq!(edit_text(&EditValue::List(vec![])), "");
        assert_eq!(edit_text(&EditValue::Int(5)), "5");
    }

    #[test]
    fn stepping_flips_cycles_and_nudges_within_limits() {
        assert_eq!(step(def("bar.enabled"), &EditValue::Bool(true), 1), Some(EditValue::Bool(false)));

        let theme = def("theme");
        assert_eq!(step(theme, &EditValue::Str("default".into()), 1), Some(EditValue::Str("nord".into())));
        assert_eq!(step(theme, &EditValue::Str("default".into()), -1), Some(EditValue::Str("wallpaper".into())), "wraps");
        assert_eq!(step(theme, &EditValue::Str("wallpaper".into()), 1), Some(EditValue::Str("default".into())));

        let h = def("bar.height"); // 24..=64: step 1
        assert_eq!(step(h, &EditValue::Int(40), 1), Some(EditValue::Int(41)));
        assert_eq!(step(h, &EditValue::Int(64), 1), None, "stops at the limit");
        assert_eq!(step(h, &EditValue::Int(24), -1), None);

        let d = def("keyboard.repeat_delay"); // 100..=2000: step 10
        assert_eq!(step(d, &EditValue::Int(200), 1), Some(EditValue::Int(210)));
        let big = def("idle.lock_timeout"); // 0..=86400: step 100
        assert_eq!(step(big, &EditValue::Int(0), 1), Some(EditValue::Int(100)));
        assert_eq!(step(big, &EditValue::Int(0), -1), None);

        let size = def("font.size"); // 8..=48: step 0.5
        assert_eq!(step(size, &EditValue::Float(14.0), 1), Some(EditValue::Float(14.5)));
        assert_eq!(step(size, &EditValue::Float(8.0), -1), None);
        let lat = def("bar.widgets.weather.lat"); // -90..=90: range 180, step 1
        assert_eq!(step(lat, &EditValue::Float(52.5), 1), Some(EditValue::Float(53.5)));
        assert_eq!(step(lat, &EditValue::Float(52.3), 1), Some(EditValue::Float(53.3)), "typed values keep their decimals");

        assert_eq!(step(def("keyboard.layout"), &EditValue::Str("us".into()), 1), None, "text is edited, not stepped");
        assert_eq!(step(h, &EditValue::Int(40), 0), None);
    }

    #[test]
    fn repeated_float_nudges_do_not_drift() {
        let size = def("font.size");
        let mut v = EditValue::Float(14.0);
        for _ in 0..10 {
            v = step(size, &v, 1).unwrap();
        }
        assert_eq!(v, EditValue::Float(19.0));
    }

    #[test]
    fn typed_text_becomes_the_right_kind() {
        assert_eq!(parse_input(def("fps"), " 90 "), Ok(EditValue::Int(90)));
        assert!(parse_input(def("fps"), "ninety").is_err());
        assert!(parse_input(def("fps"), "9.5").is_err());
        assert_eq!(parse_input(def("font.size"), "13.5"), Ok(EditValue::Float(13.5)));
        assert!(parse_input(def("font.size"), "nan").is_err());
        assert_eq!(parse_input(def("bar.left"), "launcher, ,clock,"), Ok(EditValue::List(vec!["launcher".into(), "clock".into()])));
        assert_eq!(parse_input(def("bar.left"), ""), Ok(EditValue::List(vec![])));
        assert_eq!(parse_input(def("keyboard.layout"), "us,de"), Ok(EditValue::Str("us,de".into())));
        assert_eq!(parse_input(def("bar.enabled"), "Off"), Ok(EditValue::Bool(false)));
        assert!(parse_input(def("bar.enabled"), "maybe").is_err());
    }

    #[test]
    fn save_requests_are_literal_set_and_unset_calls() {
        assert_eq!(request("fps", Some(&EditValue::Int(90))), json!({ "set": { "fps": 90 } }));
        assert_eq!(request("font.size", Some(&EditValue::Float(14.0))), json!({ "set": { "font.size": 14.0 } }));
        assert_eq!(request("fps", None), json!({ "unset": ["fps"] }));
        assert_eq!(
            request("bar.left", Some(&EditValue::List(vec!["a".into()]))),
            json!({ "set": { "bar.left": ["a"] } })
        );
    }

    #[test]
    fn saving_wallpaper_paths_also_clears_the_single_path_spelling() {
        let v = EditValue::List(vec!["/a.png".into()]);
        assert_eq!(
            request("wallpaper.paths", Some(&v)),
            json!({ "set": { "wallpaper.paths": ["/a.png"] }, "unset": ["wallpaper.path"] })
        );
        assert_eq!(request("wallpaper.paths", None), json!({ "unset": ["wallpaper.paths", "wallpaper.path"] }));
    }

    #[test]
    fn what_the_model_sends_the_host_parser_accepts() {
        // The wire format is defined by veil-host's config_set parser; build
        // the same JSON and run it through the schema the host validates with.
        for d in SETTINGS.iter().filter(|d| !d.path.contains('*')) {
            let sample = match d.kind {
                Kind::Bool => EditValue::Bool(true),
                Kind::Int { min, .. } => EditValue::Int(min),
                Kind::Float { min, .. } => EditValue::Float(min),
                Kind::Choice(o) => EditValue::Str(o[0].to_string()),
                Kind::Text => EditValue::Str("x".into()),
                Kind::TextList => EditValue::List(vec!["x".into()]),
                Kind::Color => EditValue::Str("#102030".into()),
            };
            assert!(d.check(&sample).is_ok(), "{}: {:?}", d.path, d.check(&sample));
            let args = request(d.path, Some(&sample));
            assert!(args["set"][d.path].is_boolean() || args["set"][d.path].is_number() || args["set"][d.path].is_string() || args["set"][d.path].is_array());
        }
    }
}
