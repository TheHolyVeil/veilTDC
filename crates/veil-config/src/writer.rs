//! Comment-preserving edits to the user's `config.toml`.
//!
//! The settings app never touches the file itself: it sends changes to the
//! compositor (`config.set`), which validates them against [`crate::schema`]
//! and calls [`apply`]. The running compositor polls the file's mtime, so a
//! write here is picked up on its own.
//!
//! What this guarantees, because a hand-written file is the whole point:
//! - comments, key order, blank lines and inline-table style survive;
//! - only the keys named in the batch change (resetting a key deletes it, so
//!   the default applies again; tables are never deleted, because an empty
//!   `[screenshot]` still means something);
//! - a file with a syntax error is refused, never overwritten;
//! - the whole batch is validated before the file is touched;
//! - the write is atomic (temp file, fsync, rename), and a symlinked dotfile
//!   stays a symlink: the real file behind it is what gets replaced;
//! - if the file changes while we work, the batch is redone against it.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::schema::{self, EditValue};

/// One change to the config file.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// Set a dotted path (`"bar.height"`) to a value.
    Set(String, EditValue),
    /// Remove a dotted path so its default applies again.
    Unset(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// An edit names an unknown setting or carries a bad value. Nothing was written.
    Invalid(String),
    /// The existing file is not valid TOML. Nothing was written.
    Syntax(String),
    /// The file holds a value where the edit needs a table, or the reverse.
    Shape(String),
    Io(String),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "{m}"),
            Self::Syntax(m) => write!(f, "config.toml has a syntax error, so it was left alone: {m}"),
            Self::Shape(m) => write!(f, "{m}"),
            Self::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for WriteError {}

/// What a successful [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The file that was written (symlinks resolved).
    pub path: PathBuf,
    /// Set when Veil is reading a *different* config file than the one just
    /// written (a `./config.toml` in the compositor's working directory takes
    /// precedence over the user's), so the change will not take effect.
    pub shadowed_by: Option<PathBuf>,
}

/// `$XDG_CONFIG_HOME/veil/config.toml`: the file settings are written to.
pub fn user_config_path() -> PathBuf {
    crate::dirs_config().join("config.toml")
}

/// Apply `edits` to the user's config file, creating it if needed.
///
/// If no user file exists yet but a system-wide one does, the user file is
/// seeded from it: Veil reads exactly one file, so a new user file would
/// otherwise silently replace every system default.
pub fn apply(edits: &[Edit]) -> Result<Report, WriteError> {
    let target = resolve_symlink(&user_config_path())?;
    let seed = crate::config_path().filter(|p| p.is_absolute() && *p != target);
    let mut report = apply_at(&target, seed.as_deref(), edits)?;
    report.shadowed_by = crate::config_path().filter(|p| !same_file(p, &target));
    Ok(report)
}

/// [`apply`] against an explicit `target`, seeding from `seed` when `target`
/// does not exist. Split out so tests can point it at a temp directory.
pub fn apply_at(target: &Path, seed: Option<&Path>, edits: &[Edit]) -> Result<Report, WriteError> {
    let report = Report { path: target.to_path_buf(), shadowed_by: None };
    for _ in 0..3 {
        let exists = target.exists();
        let (src, mtime) = if exists {
            (read(target)?, mtime(target))
        } else {
            (seed.map(read).transpose()?.unwrap_or_default(), None)
        };
        let out = apply_edits(&src, edits)?;
        if exists && out == src {
            return Ok(report);
        }
        // Someone (an editor, a dotfile sync) wrote while we were working:
        // redo the batch against what is there now rather than clobber it.
        if exists && mtime != self::mtime(target) {
            continue;
        }
        write_atomic(target, &out).map_err(|e| WriteError::Io(format!("writing {}: {e}", target.display())))?;
        return Ok(report);
    }
    Err(WriteError::Io(format!("{} kept changing while it was being edited; try again", target.display())))
}

/// Apply `edits` to config text and return the new text. Pure: no I/O.
pub fn apply_edits(src: &str, edits: &[Edit]) -> Result<String, WriteError> {
    // Validate the whole batch first, so a bad edit never leaves a half-applied file.
    let mut checked = Vec::with_capacity(edits.len());
    for edit in edits {
        checked.push(match edit {
            Edit::Set(path, value) => Edit::Set(path.clone(), schema::check(path, value).map_err(WriteError::Invalid)?),
            Edit::Unset(path) => {
                schema::find(path).ok_or_else(|| WriteError::Invalid(format!("unknown setting `{path}`")))?;
                edit.clone()
            }
        });
    }

    let mut doc: DocumentMut = src.parse().map_err(|e| WriteError::Syntax(format!("{e}")))?;
    for edit in &checked {
        match edit {
            Edit::Set(path, value) => {
                let segs: Vec<&str> = path.split('.').collect();
                set_in(doc.as_table_mut(), &segs, value, path)?;
            }
            Edit::Unset(path) => {
                let segs: Vec<&str> = path.split('.').collect();
                unset_in(doc.as_table_mut(), &segs);
            }
        }
    }

    let out = doc.to_string();
    out.parse::<toml::Table>()
        .map_err(|e| WriteError::Shape(format!("the edit would have produced invalid TOML: {e}")))?;
    Ok(out)
}

fn to_value(v: &EditValue) -> Value {
    match v {
        EditValue::Bool(b) => Value::from(*b),
        EditValue::Int(n) => Value::from(*n),
        EditValue::Float(f) => Value::from(*f),
        EditValue::Str(s) => Value::from(s.as_str()),
        EditValue::List(items) => {
            let mut a = Array::new();
            for s in items {
                a.push(s.as_str());
            }
            Value::Array(a)
        }
    }
}

fn set_in(tl: &mut dyn TableLike, path: &[&str], value: &EditValue, full: &str) -> Result<(), WriteError> {
    let Some((head, rest)) = path.split_first() else {
        return Ok(());
    };

    if rest.is_empty() {
        let mut new = to_value(value);
        if let Some(item) = tl.get_mut(head) {
            return match item {
                // Keep the old value's surrounding whitespace and trailing comment.
                Item::Value(old) if !old.is_inline_table() => {
                    *new.decor_mut() = old.decor().clone();
                    *old = new;
                    Ok(())
                }
                _ => Err(WriteError::Shape(format!("`{full}` is a table in config.toml, not a single value"))),
            };
        }
        tl.insert(head, Item::Value(new));
        return Ok(());
    }

    if tl.get(head).is_none() {
        // Implicit: the header only prints if the table ends up with keys of
        // its own, so `bar.widgets.weather.lat` gives `[bar.widgets.weather]`
        // and no stray `[bar]` or `[bar.widgets]`.
        let mut t = Table::new();
        t.set_implicit(true);
        tl.insert(head, Item::Table(t));
    } else if matches!(tl.get(head), Some(Item::Value(v)) if !v.is_inline_table()) {
        promote_shorthand(tl, head, full)?;
    }

    let child = tl
        .get_mut(head)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| WriteError::Shape(format!("`{head}` in config.toml is not a table, so `{full}` cannot be set")))?;
    set_in(child, rest, value, full)
}

/// `font = "Inter"` and `wallpaper = "x.png"` are shorthand for a table with
/// one key. To set anything else under them, expand the shorthand in place
/// (as an inline table, so the line stays where it is).
fn promote_shorthand(tl: &mut dyn TableLike, head: &str, full: &str) -> Result<(), WriteError> {
    let key = match head {
        "font" => "family",
        "wallpaper" => "path",
        _ => return Err(WriteError::Shape(format!("`{head}` in config.toml is a plain value, so `{full}` cannot be set"))),
    };
    let Some(Item::Value(old)) = tl.get_mut(head) else {
        return Err(WriteError::Shape(format!("`{head}` in config.toml is not a plain value")));
    };
    let Some(text) = old.as_str().map(str::to_owned) else {
        return Err(WriteError::Shape(format!("`{head}` in config.toml is not text, so `{full}` cannot be set")));
    };
    let decor = old.decor().clone();
    let mut inline = InlineTable::new();
    inline.insert(key, Value::from(text));
    let mut promoted = Value::InlineTable(inline);
    *promoted.decor_mut() = decor;
    *old = promoted;
    Ok(())
}

fn unset_in(tl: &mut dyn TableLike, path: &[&str]) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        if matches!(tl.get(head), Some(Item::Value(v)) if !v.is_inline_table()) {
            tl.remove(head);
        }
        return;
    }
    // `font = "Inter"`: resetting `font.family` is removing the whole line.
    let shorthand = matches!((*head, rest), ("font", ["family"]) | ("wallpaper", ["path"]));
    if shorthand && matches!(tl.get(head), Some(Item::Value(v)) if v.is_str()) {
        tl.remove(head);
        return;
    }
    if let Some(child) = tl.get_mut(head).and_then(Item::as_table_like_mut) {
        unset_in(child, rest);
    }
}

// ── file handling ───────────────────────────────────────────────────────────

fn read(path: &Path) -> Result<String, WriteError> {
    fs::read_to_string(path).map_err(|e| WriteError::Io(format!("reading {}: {e}", path.display())))
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Follow a symlinked config file to the real file, so the atomic rename
/// replaces that file and leaves the link (a dotfile checkout) alone.
fn resolve_symlink(path: &Path) -> Result<PathBuf, WriteError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(path)
            .map_err(|e| WriteError::Io(format!("{} is a symlink to something that does not exist: {e}", path.display()))),
        _ => Ok(path.to_path_buf()),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn write_atomic(target: &Path, data: &str) -> std::io::Result<()> {
    let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "config.toml".into());
    let tmp = dir.join(format!(".{name}.veil-tmp-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);

    let result = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(data.as_bytes())?;
        f.sync_all()?;
        drop(f);
        if let Ok(meta) = fs::metadata(target) {
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }
        fs::rename(&tmp, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }
    // Best effort: make the rename itself durable.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(path: &str, v: EditValue) -> Edit {
        Edit::Set(path.into(), v)
    }
    fn int(n: i64) -> EditValue {
        EditValue::Int(n)
    }
    fn text(s: &str) -> EditValue {
        EditValue::Str(s.into())
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("veil-writer-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn keeps_comments_order_and_spacing() {
        let src = "# my veil\nfps = 60 # cap\n\n[bar]\n# tall please\nheight = 40 # px\nposition = \"top\"\n";
        let out = apply_edits(src, &[set("bar.height", int(36))]).unwrap();
        assert_eq!(out, src.replace("height = 40 # px", "height = 36 # px"));
    }

    #[test]
    fn creates_missing_tables_with_a_real_header() {
        let out = apply_edits("", &[set("bar.height", int(30))]).unwrap();
        assert_eq!(out, "[bar]\nheight = 30\n");
    }

    #[test]
    fn nested_creation_prints_only_the_leaf_header() {
        let edits = [
            set("bar.widgets.weather.lat", EditValue::Float(52.5)),
            set("bar.widgets.weather.lon", EditValue::Float(13.4)),
        ];
        let out = apply_edits("fps = 0\n", &edits).unwrap();
        assert!(out.contains("[bar.widgets.weather]"), "{out}");
        assert!(!out.contains("[bar]"), "{out}");
        assert!(!out.contains("[bar.widgets]"), "{out}");
        let cfg = crate::parse_config(&out).unwrap();
        let w = cfg.bar.weather.expect("weather needs both coordinates");
        assert_eq!((w.lat, w.lon), (52.5, 13.4));
    }

    #[test]
    fn font_shorthand_is_expanded_not_clobbered() {
        let out = apply_edits("font = \"Inter\" # ui\n", &[set("font.size", EditValue::Float(16.0))]).unwrap();
        assert!(out.contains("# ui"), "{out}");
        let cfg = crate::parse_config(&out).unwrap();
        assert_eq!(cfg.font.family.as_deref(), Some("Inter"));
        assert_eq!(cfg.font.size, Some(16.0));
    }

    #[test]
    fn wallpaper_shorthand_is_expanded() {
        let out = apply_edits("wallpaper = \"/a.png\"\n", &[set("wallpaper.shuffle", EditValue::Bool(true))]).unwrap();
        let cfg = crate::parse_config(&out).unwrap();
        let wp = cfg.wallpaper.expect("wallpaper survives");
        assert_eq!(wp.paths, vec![PathBuf::from("/a.png")]);
        assert!(wp.shuffle);
    }

    #[test]
    fn inline_tables_are_edited_in_place() {
        let src = "notify = { timeout = 4000 } # toasts\n";
        let out = apply_edits(src, &[set("notify.max_visible", int(3)), set("notify.timeout", int(6000))]).unwrap();
        assert!(out.contains("# toasts"), "{out}");
        let cfg = crate::parse_config(&out).unwrap();
        assert_eq!(cfg.notify.max_visible, 3);
        assert_eq!(cfg.notify.timeout_ms, 6000);
    }

    #[test]
    fn new_table_inside_an_inline_table() {
        let out = apply_edits("bar = { height = 30 }\n", &[set("bar.widgets.clock.format", text("%H:%M"))]).unwrap();
        let cfg = crate::parse_config(&out).unwrap();
        assert_eq!(cfg.bar.height, 30);
        assert_eq!(cfg.bar.clock_format, "%H:%M");
    }

    #[test]
    fn unset_removes_the_key_and_keeps_the_table() {
        let out = apply_edits("[bar]\nheight = 40\nposition = \"top\"\n", &[Edit::Unset("bar.height".into())]).unwrap();
        assert_eq!(out, "[bar]\nposition = \"top\"\n");
        let out = apply_edits("[bar]\nheight = 40\n", &[Edit::Unset("bar.height".into())]).unwrap();
        assert!(out.contains("[bar]"), "an empty table can still mean something: {out}");
        let out = apply_edits("font = \"Inter\"\n", &[Edit::Unset("font.family".into())]).unwrap();
        assert!(!out.contains("font"), "{out}");
        // Unsetting something that is not there is fine.
        assert_eq!(apply_edits("fps = 1\n", &[Edit::Unset("bar.height".into())]).unwrap(), "fps = 1\n");
    }

    #[test]
    fn syntax_errors_are_refused() {
        let err = apply_edits("fps = = 3\n", &[set("fps", int(60))]).unwrap_err();
        assert!(matches!(err, WriteError::Syntax(_)), "{err:?}");
    }

    #[test]
    fn a_bad_edit_rejects_the_whole_batch() {
        let src = "fps = 30\n";
        let edits = [set("fps", int(60)), set("bar.height", int(9999))];
        assert!(matches!(apply_edits(src, &edits), Err(WriteError::Invalid(_))));
        assert!(matches!(apply_edits(src, &[set("nope", int(1))]), Err(WriteError::Invalid(_))));
        assert!(matches!(apply_edits(src, &[Edit::Unset("nope".into())]), Err(WriteError::Invalid(_))));
    }

    #[test]
    fn a_value_where_a_table_is_needed_is_an_error() {
        let err = apply_edits("bar = 3\n", &[set("bar.height", int(30))]).unwrap_err();
        assert!(matches!(err, WriteError::Shape(_)), "{err:?}");
        let err = apply_edits("[font]\nfamily = \"x\"\n", &[set("font", text("y"))]).unwrap_err();
        assert!(matches!(err, WriteError::Invalid(_)), "`font` itself is not a setting: {err:?}");
    }

    #[test]
    fn lists_and_choices_round_trip() {
        let edits = [
            set("bar.left", EditValue::List(vec!["launcher".into(), "workspaces".into()])),
            set("theme", text("Nord")),
            set("background", text("1A2B3C")),
            set("keyboard.layout", text("us,de")),
            set("keyboard.repeat_rate", int(30)),
            set("touchpad.dwt", EditValue::Bool(false)),
        ];
        let out = apply_edits("", &edits).unwrap();
        let cfg = crate::parse_config(&out).unwrap();
        assert_eq!(cfg.bar.left, vec!["launcher", "workspaces"]);
        assert_eq!(cfg.theme_name, crate::ThemeName::Nord);
        assert_eq!(cfg.background, [0x1a, 0x2b, 0x3c]);
        assert_eq!(cfg.keyboard.layout, "us,de");
        assert_eq!(cfg.keyboard.repeat_rate, 30);
        assert!(!cfg.touchpad.dwt);
    }

    #[test]
    fn writes_a_file_atomically_and_creates_directories() {
        let d = tmpdir("create");
        let target = d.join("nested/veil/config.toml");
        let report = apply_at(&target, None, &[set("fps", int(90))]).unwrap();
        assert_eq!(report.path, target);
        assert_eq!(fs::read_to_string(&target).unwrap(), "fps = 90\n");
        let leftovers: Vec<_> = fs::read_dir(target.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "no temp file left behind: {leftovers:?}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_new_user_file_is_seeded_from_the_system_one() {
        let d = tmpdir("seed");
        let system = d.join("system.toml");
        fs::write(&system, "# distro defaults\ntheme = \"nord\"\n").unwrap();
        let target = d.join("user/config.toml");
        apply_at(&target, Some(&system), &[set("fps", int(90))]).unwrap();
        let out = fs::read_to_string(&target).unwrap();
        assert!(out.contains("# distro defaults") && out.contains("theme = \"nord\"") && out.contains("fps = 90"), "{out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_symlinked_config_stays_a_symlink() {
        let d = tmpdir("link");
        let real = d.join("dotfiles/veil.toml");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "fps = 30 # keep\n").unwrap();
        let link = d.join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let target = resolve_symlink(&link).unwrap();
        assert_eq!(target, fs::canonicalize(&real).unwrap());
        apply_at(&target, None, &[set("fps", int(60))]).unwrap();

        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link must survive");
        assert_eq!(fs::read_to_string(&real).unwrap(), "fps = 60 # keep\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_dangling_symlink_is_an_error_not_a_clobber() {
        let d = tmpdir("dangling");
        let link = d.join("config.toml");
        std::os::unix::fs::symlink(d.join("missing.toml"), &link).unwrap();
        assert!(matches!(resolve_symlink(&link), Err(WriteError::Io(_))));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_noop_batch_does_not_rewrite_the_file() {
        let d = tmpdir("noop");
        let target = d.join("config.toml");
        fs::write(&target, "fps = 60\n").unwrap();
        let before = mtime(&target);
        std::thread::sleep(std::time::Duration::from_millis(20));
        apply_at(&target, None, &[set("fps", int(60))]).unwrap();
        assert_eq!(mtime(&target), before, "an unchanged file must not trigger a reload");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_syntax_error_on_disk_is_left_alone() {
        let d = tmpdir("syntax");
        let target = d.join("config.toml");
        fs::write(&target, "fps = = 3\n").unwrap();
        assert!(matches!(apply_at(&target, None, &[set("fps", int(60))]), Err(WriteError::Syntax(_))));
        assert_eq!(fs::read_to_string(&target).unwrap(), "fps = = 3\n");
        let _ = fs::remove_dir_all(&d);
    }
}
