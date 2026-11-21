//! `config.set` wire format: JSON in, [`crate::writer::Edit`]s out. Shared by the
//! compositor (which receives the command) and the settings app (which saves
//! directly when there is no compositor to ask).
//!
//! ```json
//! {"set":   {"bar.height": 40, "theme": "nord", "bar.left": ["launcher"]},
//!  "unset": ["font.size"]}
//! ```
//!
//! Either key may be left out, but not both. Paths are dotted config.toml
//! paths; what a path may hold is decided by `crate::schema`, not
//! here. This module only turns JSON into typed literals, so nothing a
//! client sends can be anything but a bool, number, string or list of strings.

use serde_json::Value;
use crate::schema::EditValue;
use crate::writer::Edit;

/// More than this in one call is a bug in the caller, not a settings change.
const MAX_EDITS: usize = 64;

/// Parse the arguments of a `config.set` command. Unsets come first, then
/// sets, so `{"unset": ["a"], "set": {"a": 1}}` ends with `a = 1`.
pub fn edits_from_args(args: &Value) -> Result<Vec<Edit>, String> {
    const USAGE: &str = "config.set needs {\"set\": {\"path\": value, ...}} and/or {\"unset\": [\"path\", ...]}";

    let unset: &[Value] = match args.get("unset") {
        None | Some(Value::Null) => &[],
        Some(Value::Array(items)) => items,
        Some(_) => return Err(format!("`unset` must be a list of paths. {USAGE}")),
    };
    let set = match args.get("set") {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) => Some(map),
        Some(_) => return Err(format!("`set` must be an object of path: value. {USAGE}")),
    };

    let mut edits = Vec::with_capacity(unset.len() + set.map_or(0, |m| m.len()));
    for path in unset {
        let path = path.as_str().ok_or_else(|| format!("`unset` entries must be strings. {USAGE}"))?;
        edits.push(Edit::Unset(path.to_string()));
    }
    if let Some(set) = set {
        for (path, value) in set {
            edits.push(Edit::Set(path.clone(), to_edit_value(path, value)?));
        }
    }

    if edits.is_empty() {
        return Err(USAGE.to_string());
    }
    if edits.len() > MAX_EDITS {
        return Err(format!("config.set takes at most {MAX_EDITS} changes at once"));
    }
    Ok(edits)
}

fn to_edit_value(path: &str, value: &Value) -> Result<EditValue, String> {
    match value {
        Value::Bool(b) => Ok(EditValue::Bool(*b)),
        Value::Number(n) => n
            .as_i64()
            .map(EditValue::Int)
            .or_else(|| n.as_f64().map(EditValue::Float))
            .ok_or_else(|| format!("`{path}`: unusable number")),
        Value::String(s) => Ok(EditValue::Str(s.clone())),
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(EditValue::List)
            .ok_or_else(|| format!("`{path}`: lists may only hold strings")),
        Value::Null | Value::Object(_) => Err(format!("`{path}`: expected a bool, number, string or list of strings")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_every_value_kind() {
        let edits = edits_from_args(&json!({
            "set": { "fps": 90, "font.size": 14.5, "touchpad.dwt": false, "theme": "nord", "bar.left": ["a", "b"] }
        }))
        .unwrap();
        assert_eq!(edits.len(), 5);
        let get = |p: &str| {
            edits.iter().find_map(|e| match e {
                Edit::Set(path, v) if path == p => Some(v.clone()),
                _ => None,
            })
        };
        assert_eq!(get("fps"), Some(EditValue::Int(90)));
        assert_eq!(get("font.size"), Some(EditValue::Float(14.5)));
        assert_eq!(get("touchpad.dwt"), Some(EditValue::Bool(false)));
        assert_eq!(get("theme"), Some(EditValue::Str("nord".into())));
        assert_eq!(get("bar.left"), Some(EditValue::List(vec!["a".into(), "b".into()])));
    }

    #[test]
    fn unsets_come_before_sets() {
        let edits = edits_from_args(&json!({ "set": { "fps": 1 }, "unset": ["bar.height", "theme"] })).unwrap();
        assert_eq!(
            edits,
            vec![
                Edit::Unset("bar.height".into()),
                Edit::Unset("theme".into()),
                Edit::Set("fps".into(), EditValue::Int(1)),
            ]
        );
    }

    #[test]
    fn rejects_empty_and_malformed_calls() {
        assert!(edits_from_args(&json!({})).is_err());
        assert!(edits_from_args(&json!({ "set": {}, "unset": [] })).is_err());
        assert!(edits_from_args(&json!({ "set": [1] })).is_err());
        assert!(edits_from_args(&json!({ "unset": "bar.height" })).is_err());
        assert!(edits_from_args(&json!({ "unset": [3] })).is_err());
    }

    #[test]
    fn rejects_values_that_are_not_plain_literals() {
        assert!(edits_from_args(&json!({ "set": { "fps": null } })).is_err());
        assert!(edits_from_args(&json!({ "set": { "bar": { "height": 3 } } })).is_err());
        assert!(edits_from_args(&json!({ "set": { "bar.left": [1, 2] } })).is_err());
        assert!(edits_from_args(&json!({ "set": { "bar.left": [["nested"]] } })).is_err());
    }

    #[test]
    fn caps_the_batch_size() {
        let many: Vec<String> = (0..=MAX_EDITS).map(|i| format!("p{i}")).collect();
        assert!(edits_from_args(&json!({ "unset": many })).is_err());
    }
}
