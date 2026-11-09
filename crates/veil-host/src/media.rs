//! Volume, brightness and media-player keys, run off the compositor thread.
//!
//! The helpers (`wpctl`, `amixer`, `pactl`, `brightnessctl`, `light`,
//! `playerctl`) are
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
//!   become a single `5*N%` change (brightness), or N OSD updates and one
//!   mixer write (volume, see `Runner`).
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
    /// Read the current volume without changing it or showing the OSD.
    VolumeQuery,
    BrightnessUp,
    BrightnessDown,
    /// Media-player keys, sent to whichever player speaks MPRIS (Spotify,
    /// browsers, mpv with mpris, ...) through `playerctl`.
    PlayPause,
    PlayerNext,
    PlayerPrevious,
    PlayerStop,
}

impl MediaCmd {
    /// Steps (up/down) can be merged with identical neighbours. Mute, queries
    /// and player controls cannot: two Next presses mean two tracks.
    fn is_step(self) -> bool {
        matches!(
            self,
            MediaCmd::VolumeUp | MediaCmd::VolumeDown | MediaCmd::BrightnessUp | MediaCmd::BrightnessDown
        )
    }
}

/// What the OSD should show once the worker is done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OsdUpdate {
    pub title: &'static str,
    pub body: String,
    pub progress: Option<u8>,
    /// Sink volume and mute state after the command, when it is known. The
    /// compositor broadcasts it as the `volume` event.
    pub volume: Option<(u8, bool)>,
    /// Do not show the OSD (a background read, not something the user did).
    pub quiet: bool,
}

impl OsdUpdate {
    fn with_volume(mut self, v: Option<(u8, bool)>) -> Self {
        self.volume = v;
        self
    }
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
            .spawn(move || {
                let mut runner = Runner::new();
                runner.refresh();
                worker_loop(cmd_rx, osd_tx, |cmd, step, emit| runner.run(cmd, step, emit));
            });
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
    mut runner: impl FnMut(MediaCmd, u32, &mut dyn FnMut(OsdUpdate)),
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
        // The runner may emit several updates (one per coalesced press).
        let mut alive = true;
        runner(first, STEP * n, &mut |u| alive &= tx.send(u).is_ok());
        if !alive {
            break;
        }
    }
}

const VOL_TITLE: &str = "VOLUME";
/// A remembered volume is trusted this long after it was read or changed.
const VOL_FRESH: Duration = Duration::from_secs(3);

/// Owns what the worker remembers between commands. Volume keys answer from
/// that memory: the OSD gets one update per press at once, and the mixer is
/// set in the background to the latest value. (Before, every press ran
/// `wpctl set-volume` then `wpctl get-volume` back to back before anything
/// showed, and queued presses were merged into one jump.)
struct Runner {
    /// Last known sink (volume, muted), and when it was read or changed.
    vol: Option<(u8, bool)>,
    touched: Instant,
    /// Targets for the setter thread, which applies only the newest.
    apply: mpsc::Sender<u8>,
}

impl Runner {
    fn new() -> Self {
        let (apply, rx) = mpsc::channel::<u8>();
        let _ = std::thread::Builder::new().name("veil-volume-set".into()).spawn(move || {
            while let Ok(mut target) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    target = newer;
                }
                let _ = applied("wpctl", &["set-volume", "-l", "1.0", "@DEFAULT_AUDIO_SINK@", &format!("{target}%")]);
            }
        });
        Runner { vol: None, touched: Instant::now(), apply }
    }

    /// Read the sink now (`None` when `wpctl` is not usable).
    fn refresh(&mut self) {
        self.vol = read_wpctl();
        self.touched = Instant::now();
    }

    fn run(&mut self, cmd: MediaCmd, step: u32, emit: &mut dyn FnMut(OsdUpdate)) {
        let up = match cmd {
            MediaCmd::VolumeUp => true,
            MediaCmd::VolumeDown => false,
            _ => {
                let u = run_cmd(cmd, step);
                // A background read can lag a key press the setter has not
                // applied yet; do not let it overwrite the newer answer.
                let stale_read = cmd == MediaCmd::VolumeQuery && self.touched.elapsed() < Duration::from_secs(1);
                if u.volume.is_some() && !stale_read {
                    self.vol = u.volume;
                    self.touched = Instant::now();
                }
                return emit(u);
            }
        };
        if self.vol.is_none() || self.touched.elapsed() > VOL_FRESH {
            self.refresh();
        }
        // No PipeWire: the old path tries wpctl, then ALSA, then Pulse.
        let Some((mut pct, muted)) = self.vol else {
            let u = run_cmd(cmd, step);
            self.vol = u.volume;
            return emit(u);
        };
        for _ in 0..step / STEP {
            let delta = STEP as i32 * if up { 1 } else { -1 };
            pct = (i32::from(pct) + delta).clamp(0, 100) as u8;
            let u = if muted { osd(VOL_TITLE, "MUTED", Some(0)) } else { pct_osd(VOL_TITLE, pct) };
            emit(u.with_volume(Some((pct, muted))));
        }
        self.vol = Some((pct, muted));
        self.touched = Instant::now();
        if self.apply.send(pct).is_err() {
            self.vol = None; // setter thread is gone: use the slow path next time
        }
    }
}

fn run_cmd(cmd: MediaCmd, step: u32) -> OsdUpdate {
    match cmd {
        MediaCmd::VolumeUp => volume_step(true, step),
        MediaCmd::VolumeDown => volume_step(false, step),
        MediaCmd::VolumeMute => volume_mute(),
        MediaCmd::VolumeQuery => volume_query(),
        MediaCmd::BrightnessUp => brightness_step(true, step),
        MediaCmd::BrightnessDown => brightness_step(false, step),
        MediaCmd::PlayPause => player("play-pause"),
        MediaCmd::PlayerNext => player("next"),
        MediaCmd::PlayerPrevious => player("previous"),
        MediaCmd::PlayerStop => player("stop"),
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
        volume: None,
        quiet: false,
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
        let now = read_wpctl();
        return match now {
            Some((_, true)) => osd(T, "MUTED", Some(0)),
            Some((pct, false)) => pct_osd(T, pct),
            None => osd(T, format!("{}{step}%", sign(up)), None),
        }
        .with_volume(now);
    }

    // ALSA.
    let am = format!("{step}%{}", sign(up));
    if let Some(out) = applied("amixer", &["sset", "Master", &am]) {
        let pct = parse_bracket_percent(&out);
        return match pct {
            Some(pct) => pct_osd(T, pct),
            None => osd(T, format!("{}{step}%", sign(up)), None),
        }
        .with_volume(pct.map(|p| (p, false)));
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
        }
        .with_volume(pct.map(|p| (p, false)));
    }

    osd(T, "NO AUDIO CONTROL", None)
}

/// Read the sink volume for the bar. Same helper order as stepping.
fn volume_query() -> OsdUpdate {
    let v = read_wpctl().or_else(|| read_pactl().map(|p| (p.min(100), false)));
    let mut u = osd("VOLUME", "", None).with_volume(v);
    u.quiet = true;
    u
}

fn volume_mute() -> OsdUpdate {
    const T: &str = "VOLUME";

    if applied("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]).is_some() {
        let now = read_wpctl();
        return match now {
            Some((_, true)) => osd(T, "MUTED", Some(0)),
            Some((pct, false)) => osd(T, format!("UNMUTED ({pct}%)"), Some(pct)),
            None => osd(T, "TOGGLED", None),
        }
        .with_volume(now);
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

// ----------------------------------------------------------------- player

/// Send a transport command to the active MPRIS player. Silent on success (a
/// track change is its own feedback); when nothing took it, say so, so a key
/// that does nothing is never a mystery.
fn player(action: &str) -> OsdUpdate {
    if applied("playerctl", &[action]).is_some() {
        return OsdUpdate { quiet: true, ..osd("MEDIA", "", None) };
    }
    osd("MEDIA", "NO PLAYER", None)
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
        worker_loop(cmd_rx, osd_tx, |cmd, step, emit| emit(osd("T", format!("{cmd:?} {step}"), None)));
        let got: Vec<String> = osd_rx.try_iter().map(|u| u.body).collect();
        assert_eq!(got, ["VolumeUp 20", "VolumeMute 5", "BrightnessDown 10"]);
    }

    #[test]
    fn player_keys_are_never_merged() {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (osd_tx, osd_rx) = mpsc::channel();
        for _ in 0..3 {
            cmd_tx.send(MediaCmd::PlayerNext).unwrap();
        }
        cmd_tx.send(MediaCmd::PlayPause).unwrap();
        cmd_tx.send(MediaCmd::PlayPause).unwrap();
        drop(cmd_tx);
        worker_loop(cmd_rx, osd_tx, |cmd, step, emit| emit(osd("T", format!("{cmd:?} {step}"), None)));
        let got: Vec<String> = osd_rx.try_iter().map(|u| u.body).collect();
        assert_eq!(got, ["PlayerNext 5", "PlayerNext 5", "PlayerNext 5", "PlayPause 5", "PlayPause 5"]);
    }

    #[test]
    fn coalescing_is_capped() {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (osd_tx, osd_rx) = mpsc::channel();
        for _ in 0..(MAX_COALESCE + 3) {
            cmd_tx.send(MediaCmd::VolumeDown).unwrap();
        }
        drop(cmd_tx);
        worker_loop(cmd_rx, osd_tx, |_, step, emit| emit(osd("T", step.to_string(), None)));
        let got: Vec<String> = osd_rx.try_iter().map(|u| u.body).collect();
        assert_eq!(got, ["100", "15"]);
    }

    #[test]
    fn volume_keys_answer_from_memory_one_update_per_press() {
        let (apply, setter) = mpsc::channel();
        let mut r = Runner { vol: Some((50, false)), touched: Instant::now(), apply };
        let mut got = Vec::new();
        r.run(MediaCmd::VolumeUp, STEP * 2, &mut |u| got.push((u.progress, u.volume)));
        assert_eq!(got, [(Some(55), Some((55, false))), (Some(60), Some((60, false)))]);
        assert_eq!(setter.try_iter().collect::<Vec<_>>(), [60], "the mixer is set once, to the final value");

        // Clamped at both ends, and a muted sink keeps reading MUTED.
        r.vol = Some((98, false));
        let mut top = Vec::new();
        r.run(MediaCmd::VolumeUp, STEP, &mut |u| top.push(u.progress));
        assert_eq!(top, [Some(100)]);
        r.vol = Some((3, true));
        let mut low = Vec::new();
        r.run(MediaCmd::VolumeDown, STEP, &mut |u| low.push((u.body, u.progress)));
        assert_eq!(low, [("MUTED".to_string(), Some(0))]);
        assert_eq!(r.vol, Some((0, true)));
    }
}
