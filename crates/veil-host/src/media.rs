//! Volume / brightness keys, run off the compositor thread.
//!
//! The helpers (`wpctl`, `amixer`, `pactl`, `brightnessctl`, `light`) are
//! external processes. Running them inline froze the whole compositor for as
//! long as they took, and forever if one hung. Now a single worker thread owns
//! them: the compositor sends a `MediaCmd`, keeps drawing, and later picks the
//! finished `OsdUpdate` up with `try_recv` and shows it.
//!
//! Rules the worker follows:
//! - Backends are tried in order, but only while the previous one *failed to
//!   apply* (not found, non-zero exit, timeout). If a change applied and only
//!   reading the level back failed, no second backend runs, so one key press
//!   can never change the level twice.
//! - Volume is capped at 100% on every backend.
//! - Key auto-repeat is coalesced: N queued presses in the same direction
//!   become a single `5*N%` change.
//! - Every helper has a 2 s timeout and is killed if it overruns.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const STEP: u32 = 5;
const MAX_COALESCE: u32 = 20;
const HELPER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaCmd {
    VolumeUp,
    VolumeDown,
    VolumeMute,
    BrightnessUp,
    BrightnessDown,
}

impl MediaCmd {
    /// Steps (up/down) can be merged with identical neighbours; mute cannot.
    fn is_step(self) -> bool {
        !matches!(self, MediaCmd::VolumeMute)
    }
}

/// What the OSD should show once the worker is done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OsdUpdate {
    pub title: &'static str,
    pub body: String,
    pub progress: Option<u8>,
}

pub struct MediaWorker {
    tx: Option<mpsc::Sender<MediaCmd>>,
    rx: mpsc::Receiver<OsdUpdate>,
}

impl MediaWorker {
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<MediaCmd>();
        let (osd_tx, osd_rx) = mpsc::channel::<OsdUpdate>();
        let spawned = std::thread::Builder::new()
            .name("veil-media".into())
            .spawn(move || worker_loop(cmd_rx, osd_tx, run_cmd));
        match spawned {
            Ok(_) => Self {
                tx: Some(cmd_tx),
                rx: osd_rx,
            },
            Err(e) => {
                eprintln!("[veil-media] could not start worker: {e}; media keys disabled");
                Self { tx: None, rx: osd_rx }
            }
        }
    }

    /// Queue a command. Never blocks.
    pub fn send(&self, cmd: MediaCmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    /// A finished result, if there is one.
    pub fn try_recv(&self) -> Option<OsdUpdate> {
        self.rx.try_recv().ok()
    }
}

fn worker_loop(
    rx: mpsc::Receiver<MediaCmd>,
    tx: mpsc::Sender<OsdUpdate>,
    runner: impl Fn(MediaCmd, u32) -> OsdUpdate,
) {
    let mut pending: Option<MediaCmd> = None;
    loop {
        let first = match pending.take() {
            Some(c) => c,
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => break, // compositor gone
            },
        };
        let mut n = 1;
        if first.is_step() {
            while n < MAX_COALESCE {
                match rx.try_recv() {
                    Ok(c) if c == first => n += 1,
                    Ok(c) => {
                        pending = Some(c);
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        if tx.send(runner(first, STEP * n)).is_err() {
            break;
        }
    }
}

fn run_cmd(cmd: MediaCmd, step: u32) -> OsdUpdate {
    match cmd {
        MediaCmd::VolumeUp => volume_step(true, step),
        MediaCmd::VolumeDown => volume_step(false, step),
        MediaCmd::VolumeMute => volume_mute(),
        MediaCmd::BrightnessUp => brightness_step(true, step),
        MediaCmd::BrightnessDown => brightness_step(false, step),
    }
}

// ---------------------------------------------------------------- helpers

struct Ran {
    ok: bool,
    out: String,
}

/// Run a helper with a timeout. `None` means it could not be started or had
/// to be killed; `Some` carries its exit status and stdout.
fn run(prog: &str, args: &[&str]) -> Option<Ran> {
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + HELPER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut buf = Vec::new();
                if let Some(mut so) = child.stdout.take() {
                    let _ = so.read_to_end(&mut buf);
                }
                return Some(Ran {
                    ok: status.success(),
                    out: String::from_utf8_lossy(&buf).into_owned(),
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(3)),
            Err(_) => return None,
        }
    }
}

/// `Some(stdout)` if the helper ran and exited 0.
fn applied(prog: &str, args: &[&str]) -> Option<String> {
    run(prog, args).filter(|r| r.ok).map(|r| r.out)
}

fn osd(title: &'static str, body: impl Into<String>, progress: Option<u8>) -> OsdUpdate {
    OsdUpdate {
        title,
        body: body.into(),
        progress,
    }
}

fn pct_osd(title: &'static str, pct: u8) -> OsdUpdate {
    osd(title, format!("{pct}%"), Some(pct))
}

fn sign(up: bool) -> char {
    if up {
        '+'
    } else {
        '-'
    }
}

// ----------------------------------------------------------------- volume

fn volume_step(up: bool, step: u32) -> OsdUpdate {
    const T: &str = "VOLUME";

    // PipeWire. `-l 1.0` stops it going past 100%.
    let wp = format!("{step}%{}", sign(up));
    if applied("wpctl", &["set-volume", "-l", "1.0", "@DEFAULT_AUDIO_SINK@", &wp]).is_some() {
        return match read_wpctl() {
            Some((_, true)) => osd(T, "MUTED", Some(0)),
            Some((pct, false)) => pct_osd(T, pct),
            None => osd(T, format!("{}{step}%", sign(up)), None),
        };
    }

    // ALSA.
    let am = format!("{step}%{}", sign(up));
    if let Some(out) = applied("amixer", &["sset", "Master", &am]) {
        return match parse_bracket_percent(&out) {
            Some(pct) => pct_osd(T, pct),
            None => osd(T, format!("{}{step}%", sign(up)), None),
        };
    }

    // PulseAudio. No built-in limit, so read back and clamp.
    let pa = format!("{}{step}%", sign(up));
    if applied("pactl", &["set-sink-volume", "@DEFAULT_SINK@", &pa]).is_some() {
        let mut pct = read_pactl();
        if pct.is_some_and(|p| p > 100) {
            let _ = applied("pactl", &["set-sink-volume", "@DEFAULT_SINK@", "100%"]);
            pct = Some(100);
        }
        return match pct {
            Some(p) => pct_osd(T, p),
            None => osd(T, format!("{}{step}%", sign(up)), None),
        };
    }

    osd(T, "NO AUDIO CONTROL", None)
}

fn volume_mute() -> OsdUpdate {
    const T: &str = "VOLUME";

    if applied("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]).is_some() {
        return match read_wpctl() {
            Some((_, true)) => osd(T, "MUTED", Some(0)),
            Some((pct, false)) => osd(T, format!("UNMUTED ({pct}%)"), Some(pct)),
            None => osd(T, "TOGGLED", None),
        };
    }
    if let Some(out) = applied("amixer", &["sset", "Master", "toggle"]) {
        return if out.contains("[off]") {
            osd(T, "MUTED", Some(0))
        } else {
            osd(T, "UNMUTED", None)
        };
    }
    if applied("pactl", &["set-sink-mute", "@DEFAULT_SINK@", "toggle"]).is_some() {
        return osd(T, "TOGGLED", None);
    }
    osd(T, "NO AUDIO CONTROL", None)
}

fn read_wpctl() -> Option<(u8, bool)> {
    applied("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]).and_then(|s| parse_wpctl(&s))
}

fn read_pactl() -> Option<u8> {
    applied("pactl", &["get-sink-volume", "@DEFAULT_SINK@"]).and_then(|s| parse_pactl(&s))
}

// ------------------------------------------------------------- brightness

fn brightness_step(up: bool, step: u32) -> OsdUpdate {
    const T: &str = "BRIGHTNESS";

    let arg = format!("{step}%{}", sign(up));
    if let Some(out) = applied("brightnessctl", &["set", &arg]) {
        return match parse_paren_percent(&out) {
            Some(pct) => pct_osd(T, pct),
            None => osd(T, format!("{}{step}%", sign(up)), None),
        };
    }

    let flag = if up { "-A" } else { "-U" };
    let step_s = step.to_string();
    if applied("light", &[flag, &step_s]).is_some() {
        return match applied("light", &[]).and_then(|s| s.trim().parse::<f32>().ok()) {
            Some(v) => pct_osd(T, v.round().clamp(0.0, 100.0) as u8),
            None => osd(T, format!("{}{step}%", sign(up)), None),
        };
    }

    osd(T, "NO BRIGHTNESS CONTROL", None)
}

// ---------------------------------------------------------------- parsing

/// `wpctl get-volume`: `Volume: 0.52` or `Volume: 0.52 [MUTED]`.
/// Returns `(percent, muted)`.
pub fn parse_wpctl(s: &str) -> Option<(u8, bool)> {
    let muted = s.contains("[MUTED]");
    let v: f32 = s.split_whitespace().nth(1)?.parse().ok()?;
    Some(((v * 100.0).round().clamp(0.0, 255.0) as u8, muted))
}

/// First `[NN%]` in `amixer` output.
pub fn parse_bracket_percent(s: &str) -> Option<u8> {
    s.split('[')
        .skip(1)
        .filter_map(|seg| seg.split(']').next())
        .find_map(|seg| seg.strip_suffix('%')?.parse().ok())
}

/// First `(NN%)` in `brightnessctl set` output.
pub fn parse_paren_percent(s: &str) -> Option<u8> {
    s.split('(')
        .skip(1)
        .filter_map(|seg| seg.split(')').next())
        .find_map(|seg| seg.strip_suffix('%')?.parse().ok())
}

/// First `NN%` token in `pactl get-sink-volume` output.
pub fn parse_pactl(s: &str) -> Option<u8> {
    s.split_whitespace()
        .find_map(|tok| tok.strip_suffix('%')?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wpctl_output() {
        assert_eq!(parse_wpctl("Volume: 0.52\n"), Some((52, false)));
        assert_eq!(parse_wpctl("Volume: 1.00\n"), Some((100, false)));
        assert_eq!(parse_wpctl("Volume: 0.30 [MUTED]\n"), Some((30, true)));
        assert_eq!(parse_wpctl(""), None);
        assert_eq!(parse_wpctl("Volume: loud"), None);
    }

    #[test]
    fn amixer_output() {
        let s = "Simple mixer control 'Master',0\n  Mono: Playback 42 [65%] [-17.00dB] [on]\n";
        assert_eq!(parse_bracket_percent(s), Some(65));
        assert_eq!(parse_bracket_percent("[on] [off]"), None);
    }

    #[test]
    fn brightnessctl_output() {
        let s = "Updated device 'amdgpu_bl1'\nCurrent brightness: 96000 (50%)\nMax brightness: 192000\n";
        assert_eq!(parse_paren_percent(s), Some(50));
        assert_eq!(parse_paren_percent("no percent here"), None);
    }

    #[test]
    fn pactl_output() {
        let s = "Volume: front-left: 32768 /  50% / -18.06 dB,   front-right: 32768 /  50% / -18.06 dB\n";
        assert_eq!(parse_pactl(s), Some(50));
        assert_eq!(parse_pactl("Volume: 65536 / 100% / 0.00 dB"), Some(100));
        assert_eq!(parse_pactl("nothing"), None);
    }

    #[test]
    fn missing_helpers_are_not_applied() {
        assert!(run("definitely-not-a-real-binary-veil", &[]).is_none());
        assert!(applied("false", &[]).is_none());
        assert_eq!(applied("true", &[]).as_deref(), Some(""));
    }

    #[test]
    fn hung_helper_is_killed() {
        let t = Instant::now();
        assert!(run("sleep", &["30"]).is_none());
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn repeats_coalesce_into_one_step() {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (osd_tx, osd_rx) = mpsc::channel();
        for _ in 0..4 {
            cmd_tx.send(MediaCmd::VolumeUp).unwrap();
        }
        cmd_tx.send(MediaCmd::VolumeMute).unwrap();
        cmd_tx.send(MediaCmd::BrightnessDown).unwrap();
        cmd_tx.send(MediaCmd::BrightnessDown).unwrap();
        drop(cmd_tx);
        // A fake runner: the test must never touch the real mixer.
        worker_loop(cmd_rx, osd_tx, |cmd, step| osd("T", format!("{cmd:?} {step}"), None));
        let got: Vec<String> = osd_rx.try_iter().map(|u| u.body).collect();
        assert_eq!(got, ["VolumeUp 20", "VolumeMute 5", "BrightnessDown 10"]);
    }

    #[test]
    fn coalescing_is_capped() {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (osd_tx, osd_rx) = mpsc::channel();
        for _ in 0..(MAX_COALESCE + 3) {
            cmd_tx.send(MediaCmd::VolumeDown).unwrap();
        }
        drop(cmd_tx);
        worker_loop(cmd_rx, osd_tx, |_, step| osd("T", step.to_string(), None));
        let got: Vec<String> = osd_rx.try_iter().map(|u| u.body).collect();
        assert_eq!(got, ["100", "15"]);
    }
}
