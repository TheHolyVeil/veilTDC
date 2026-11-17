//! `veil shell screenshot [full|area|window] [--no-save] [--no-copy] [--no-notify]`
//!
//! A one-shot wrapper: pick what to capture, let `grim` take it, then save
//! the PNG and/or put it on the clipboard with `wl-copy`, and announce the
//! result with a desktop notification. The compositor side is
//! `wlr-screencopy` (see `veil-host::screencopy`); this crate only
//! orchestrates, so any capture tool that speaks that protocol keeps working
//! next to it.
//!
//! - `full`: every output, as `grim` sees them.
//! - `area`: the rectangle `slurp` returns (default). Cancelling exits 0
//!   and writes nothing.
//! - `window`: the focused window, geometry from the compositor's
//!   `window.focused` IPC command.
//!
//! What each kind saves/copies, where files go and whether a notification is
//! shown come from the `screenshot` table in config.lua; the flags above can
//! only turn things off for one run. Files land in `$VEIL_SCREENSHOT_DIR`,
//! else `screenshot.dir`, else `~/Pictures/Screenshots`. The saved path is
//! printed on stdout. The notification goes to whatever owns
//! `org.freedesktop.Notifications`, so it works with other daemons too.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub use veil_config::ScreenshotKind;
use veil_config::ScreenshotConfig;

const USAGE: &str = "usage: veil shell screenshot [full|area|window] [--no-save] [--no-copy] [--no-notify]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub kind: ScreenshotKind,
    pub save: bool,
    pub copy: bool,
    pub notify: bool,
}

/// What a finished capture did, for the notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub saved: Option<PathBuf>,
    pub copied: bool,
}

/// Parse what follows `veil shell screenshot`. Defaults come from `cfg`
/// (per kind); the flags only switch things off.
pub fn parse_args(args: &[String], cfg: &ScreenshotConfig) -> Result<Options, String> {
    let mut kind = None;
    let (mut no_save, mut no_copy, mut no_notify) = (false, false, false);
    for a in args {
        match a.as_str() {
            "--no-save" => no_save = true,
            "--no-copy" => no_copy = true,
            "--no-notify" => no_notify = true,
            other => match ScreenshotKind::parse(other) {
                Some(_) if kind.is_some() => return Err(format!("only one capture kind allowed\n{USAGE}")),
                Some(k) => kind = Some(k),
                None => return Err(format!("unknown argument `{other}`\n{USAGE}")),
            },
        }
    }
    let kind = kind.unwrap_or(ScreenshotKind::Area);
    let mode = cfg.mode(kind);
    let (save, copy) = (mode.save && !no_save, mode.copy && !no_copy);
    if !save && !copy {
        return Err(format!(
            "nothing to do for `{}`: saving and copying are both off (config or flags)\n{USAGE}",
            kind.as_str()
        ));
    }
    Ok(Options { kind, save, copy, notify: cfg.notify && !no_notify })
}

/// `grim -g` geometry from a `window.focused` reply (`x`, `y`, `w`, `h` in
/// global logical pixels).
pub fn window_geometry(data: &serde_json::Value) -> Result<String, String> {
    let num = |k: &str| data.get(k).and_then(|v| v.as_i64());
    match (num("x"), num("y"), num("w"), num("h")) {
        (Some(x), Some(y), Some(w), Some(h)) if w > 0 && h > 0 => Ok(format!("{x},{y} {w}x{h}")),
        _ => Err("the compositor sent an unusable window rectangle".into()),
    }
}

/// `Screenshot_2026-11-16_14-03-09.png`
pub fn file_name(now: chrono::NaiveDateTime) -> String {
    now.format("Screenshot_%Y-%m-%d_%H-%M-%S.png").to_string()
}

/// `~` and `~/…` become `home`; anything else is left alone.
pub fn expand_home(path: &str, home: Option<&Path>) -> PathBuf {
    match (path, home) {
        ("~", Some(h)) => h.to_path_buf(),
        (p, Some(h)) if p.starts_with("~/") => h.join(&p[2..]),
        (p, _) => PathBuf::from(p),
    }
}

/// `$VEIL_SCREENSHOT_DIR` (when non-empty), else the config `dir`, else
/// `<home>/Pictures/Screenshots`.
pub fn screenshots_dir(env_dir: Option<&str>, cfg_dir: Option<&str>, home: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(d) = env_dir.filter(|d| !d.is_empty()).or(cfg_dir.filter(|d| !d.is_empty())) {
        return Ok(expand_home(d, home));
    }
    home.map(|h| h.join("Pictures").join("Screenshots"))
        .ok_or_else(|| "HOME is not set; set screenshot.dir or VEIL_SCREENSHOT_DIR".to_string())
}

/// `dir/name`, or `dir/name-1.png`, `-2`, … if two shots share a second.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let stem = name.strip_suffix(".png").unwrap_or(name);
    (1u32..)
        .map(|n| dir.join(format!("{stem}-{n}.png")))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

/// Body of the "captured" toast: whether it was copied, and where it went.
pub fn toast_body(outcome: &Outcome) -> String {
    let mut lines = Vec::new();
    if outcome.copied {
        lines.push("Copied to clipboard".to_string());
    }
    if let Some(p) = &outcome.saved {
        lines.push(escape_markup(&p.display().to_string()));
    }
    lines.join("\n")
}

/// Notification bodies are markup; a path may contain `&`, `<` or `>`.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn tool_missing(name: &str, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("`{name}` is not installed (veil-screenshot needs it)")
    } else {
        format!("could not run `{name}`: {e}")
    }
}

/// `Ok(None)` when the user cancelled the selection.
fn pick_area() -> Result<Option<String>, String> {
    let out = Command::new("slurp").output().map_err(|e| tool_missing("slurp", e))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !stdout.is_empty() {
        return Ok(Some(stdout));
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.trim().is_empty() || err.contains("cancelled") {
        Ok(None)
    } else {
        Err(format!("slurp failed: {}", err.trim()))
    }
}

fn grim_png(geometry: Option<&str>) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("grim");
    cmd.args(["-t", "png"]);
    if let Some(g) = geometry {
        cmd.args(["-g", g]);
    }
    cmd.arg("-");
    let out = cmd.output().map_err(|e| tool_missing("grim", e))?;
    if !out.status.success() {
        return Err(format!("grim failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    if out.stdout.is_empty() {
        return Err("grim produced no image".into());
    }
    Ok(out.stdout)
}

fn copy_to_clipboard(png: &[u8]) -> Result<(), String> {
    let mut cmd = Command::new("wl-copy");
    cmd.args(["--type", "image/png"]).stdin(Stdio::piped());
    // wl-copy stays alive in the background to serve the selection. Its own
    // process group keeps a supervisor group-kill from taking the clipboard
    // with it when this one-shot exits.
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| tool_missing("wl-copy", e))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(png).map_err(|e| format!("wl-copy: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("wl-copy: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("wl-copy exited with {status}"))
    }
}

/// Ask the compositor for the focused window's rectangle.
fn focused_window() -> Result<serde_json::Value, String> {
    let sock = std::env::var("VEIL_IPC_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| veil_ipc::socket_path());
    let mut stream = UnixStream::connect(&sock).map_err(|e| format!("cannot reach the compositor at {}: {e}", sock.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;

    let hello = veil_ipc::ClientMessage::Hello { version: veil_ipc::PROTOCOL_VERSION, name: "veil-screenshot".into() };
    let cmd = veil_ipc::ClientMessage::Cmd { id: 1, cmd: "window.focused".into(), args: serde_json::Value::Null };
    for msg in [&hello, &cmd] {
        stream.write_all(&veil_ipc::encode(msg).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    }

    let mut frames = veil_ipc::FrameBuffer::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).map_err(|e| format!("no answer from the compositor: {e}"))?;
        if n == 0 {
            return Err("the compositor closed the connection".into());
        }
        frames.push(&buf[..n]).map_err(|e| e.to_string())?;
        while let Ok(Some(m)) = frames.next_frame::<veil_ipc::ServerMessage>() {
            if let veil_ipc::ServerMessage::Reply { ok, error, data, .. } = m {
                return if ok {
                    data.ok_or_else(|| "empty window.focused reply".to_string())
                } else {
                    Err(error.unwrap_or_else(|| "window.focused failed".into()))
                };
            }
        }
    }
}

/// Post a notification to whatever owns `org.freedesktop.Notifications`.
fn notify(summary: &str, body: &str) -> Result<(), String> {
    use std::collections::HashMap;

    futures_lite::future::block_on(async {
        let conn = zbus::Connection::session().await.map_err(|e| format!("no session bus: {e}"))?;
        let hints: HashMap<&str, zbus::Variant<'_>> = HashMap::new();
        conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &("Veil", 0u32, "camera-photo", summary, body, Vec::<&str>::new(), hints, -1i32),
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("notification failed: {e}"))
    })
}

/// Take the screenshot described by `opts`. `Ok(None)`: the user cancelled
/// the area selection.
pub fn capture(opts: &Options, cfg: &ScreenshotConfig) -> Result<Option<Outcome>, String> {
    let geometry = match opts.kind {
        ScreenshotKind::Full => None,
        ScreenshotKind::Area => match pick_area()? {
            Some(g) => Some(g),
            None => return Ok(None),
        },
        ScreenshotKind::Window => Some(window_geometry(&focused_window()?)?),
    };
    let png = grim_png(geometry.as_deref())?;

    let mut outcome = Outcome { saved: None, copied: false };
    if opts.save {
        let dir = screenshots_dir(
            std::env::var("VEIL_SCREENSHOT_DIR").ok().as_deref(),
            cfg.dir.as_deref(),
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        )?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = unique_path(&dir, &file_name(chrono::Local::now().naive_local()));
        std::fs::write(&path, &png).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        outcome.saved = Some(path);
    }
    if opts.copy {
        match copy_to_clipboard(&png) {
            Ok(()) => outcome.copied = true,
            // A saved file is still a result; only fail if the clipboard was the point.
            Err(e) if outcome.saved.is_some() => eprintln!("[veil shell screenshot] clipboard: {e}"),
            Err(e) => return Err(e),
        }
    }
    Ok(Some(outcome))
}

/// Entry point for `veil shell screenshot ...`.
pub fn run_screenshot() -> Result<(), String> {
    let cfg = veil_config::load_user_config().screenshot;
    let args: Vec<String> = std::env::args().skip(3).collect();
    let opts = parse_args(&args, &cfg)?;
    // Spawned from a key press there is no terminal: a toast is the only
    // way a failure becomes visible.
    let result = capture(&opts, &cfg);
    if opts.notify {
        let toast = match &result {
            Ok(Some(o)) => Some(("Screenshot captured", toast_body(o))),
            Ok(None) => None,
            Err(e) => Some(("Screenshot failed", escape_markup(e))),
        };
        if let Some((summary, body)) = toast {
            if let Err(e) = notify(summary, &body) {
                eprintln!("[veil shell screenshot] {e}");
            }
        }
    }
    if let Some(path) = result?.and_then(|o| o.saved) {
        println!("{}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn cfg() -> ScreenshotConfig {
        ScreenshotConfig::default()
    }

    #[test]
    fn no_args_means_area_with_config_defaults() {
        let o = parse_args(&[], &cfg()).unwrap();
        assert_eq!(o, Options { kind: ScreenshotKind::Area, save: true, copy: true, notify: true });
    }

    #[test]
    fn kinds_and_flags_parse_in_any_order() {
        let o = parse_args(&args(&["--no-copy", "window"]), &cfg()).unwrap();
        assert_eq!(o, Options { kind: ScreenshotKind::Window, save: true, copy: false, notify: true });
        let o = parse_args(&args(&["full", "--no-save", "--no-notify"]), &cfg()).unwrap();
        assert_eq!(o, Options { kind: ScreenshotKind::Full, save: false, copy: true, notify: false });
    }

    #[test]
    fn config_sets_the_per_kind_defaults_and_flags_only_turn_things_off() {
        let mut c = cfg();
        c.area.copy = false;
        c.notify = false;
        let o = parse_args(&args(&["area"]), &c).unwrap();
        assert_eq!(o, Options { kind: ScreenshotKind::Area, save: true, copy: false, notify: false });
        // Other kinds are unaffected by area's setting.
        assert!(parse_args(&args(&["full"]), &c).unwrap().copy);
    }

    #[test]
    fn bad_arguments_are_rejected() {
        assert!(parse_args(&args(&["fullscreen"]), &cfg()).is_err());
        assert!(parse_args(&args(&["full", "area"]), &cfg()).is_err());
        assert!(parse_args(&args(&["--no-save", "--no-copy"]), &cfg()).is_err());
        let mut c = cfg();
        c.full.save = false;
        c.full.copy = false;
        assert!(parse_args(&args(&["full"]), &c).is_err()); // nothing left to do
    }

    #[test]
    fn window_geometry_formats_for_grim() {
        let v = serde_json::json!({ "x": 10, "y": -4, "w": 640, "h": 480, "title": "t" });
        assert_eq!(window_geometry(&v).unwrap(), "10,-4 640x480");
        assert!(window_geometry(&serde_json::json!({ "x": 0, "y": 0, "w": 0, "h": 5 })).is_err());
        assert!(window_geometry(&serde_json::json!({ "title": "no rect" })).is_err());
    }

    #[test]
    fn file_name_is_sortable_and_png() {
        let t = chrono::NaiveDate::from_ymd_opt(2026, 11, 16).unwrap().and_hms_opt(14, 3, 9).unwrap();
        assert_eq!(file_name(t), "Screenshot_2026-11-16_14-03-09.png");
    }

    #[test]
    fn screenshots_dir_precedence_env_then_config_then_home() {
        let home = Path::new("/home/abyss");
        assert_eq!(screenshots_dir(Some("/tmp/e"), Some("/tmp/c"), Some(home)).unwrap(), PathBuf::from("/tmp/e"));
        assert_eq!(screenshots_dir(Some(""), Some("/tmp/c"), Some(home)).unwrap(), PathBuf::from("/tmp/c"));
        assert_eq!(screenshots_dir(None, Some("~/Shots"), Some(home)).unwrap(), PathBuf::from("/home/abyss/Shots"));
        assert_eq!(screenshots_dir(None, None, Some(home)).unwrap(), PathBuf::from("/home/abyss/Pictures/Screenshots"));
        assert!(screenshots_dir(None, None, None).is_err());
    }

    #[test]
    fn expand_home_only_touches_a_leading_tilde() {
        let home = Path::new("/h");
        assert_eq!(expand_home("~", Some(home)), PathBuf::from("/h"));
        assert_eq!(expand_home("~/a/b", Some(home)), PathBuf::from("/h/a/b"));
        assert_eq!(expand_home("/a/~/b", Some(home)), PathBuf::from("/a/~/b"));
        assert_eq!(expand_home("~x", Some(home)), PathBuf::from("~x"));
        assert_eq!(expand_home("~/a", None), PathBuf::from("~/a"));
    }

    #[test]
    fn toast_body_lists_clipboard_then_path_and_escapes_markup() {
        let o = Outcome { saved: Some(PathBuf::from("/h/Pictures/Screenshots/a.png")), copied: true };
        assert_eq!(toast_body(&o), "Copied to clipboard\n/h/Pictures/Screenshots/a.png");
        let o = Outcome { saved: Some(PathBuf::from("/h/a&b/<x>.png")), copied: false };
        assert_eq!(toast_body(&o), "/h/a&amp;b/&lt;x&gt;.png");
        let o = Outcome { saved: None, copied: true };
        assert_eq!(toast_body(&o), "Copied to clipboard");
    }

    /// Needs a notification daemon on the session bus and shows a real toast:
    /// `cargo test -p veil-screenshot -- --ignored`.
    #[test]
    #[ignore]
    fn notify_reaches_a_running_daemon() {
        notify("Veil screenshot test", "Copied to clipboard\n/tmp/example.png").expect("a daemon answers Notify");
    }

    #[test]
    fn unique_path_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let name = "Screenshot_x.png";
        let first = unique_path(dir.path(), name);
        assert_eq!(first, dir.path().join(name));
        std::fs::write(&first, b"a").unwrap();
        let second = unique_path(dir.path(), name);
        assert_eq!(second, dir.path().join("Screenshot_x-1.png"));
        std::fs::write(&second, b"b").unwrap();
        assert_eq!(unique_path(dir.path(), name), dir.path().join("Screenshot_x-2.png"));
    }
}
