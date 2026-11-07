//! Auto-pick which monitor the greeter runs on, and which mode it uses.
//!
//! Slint's linuxkms backend drives exactly ONE connector. Left alone it takes
//! the first *connected* connector of the first DRM node it can open, which on
//! a multi-monitor box is whichever the kernel enumerates first (here: the
//! 1280x720 HDMI screen, not the 1080p one) — and then its mode is the
//! preferred one, i.e. the lowest refresh rate. Slint does honour
//! `SLINT_DRM_OUTPUT` / `SLINT_DRM_MODE`, so we probe KMS read-only before the
//! backend starts, choose, and export those. No config: it's all detection.
//!
//! Choice: the connected output whose preferred resolution has the most
//! pixels (ties: higher refresh, then name), at that resolution's highest
//! refresh rate. An existing `SLINT_DRM_OUTPUT` in the environment wins.

use drm::control::{connector, Device as ControlDevice, Mode, ModeTypeFlags};
use drm::Device as DrmDevice;
use std::os::fd::{AsFd, BorrowedFd};

pub(crate) struct Card(pub(crate) std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl DrmDevice for Card {}
impl ControlDevice for Card {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeInfo {
    pub w: u32,
    pub h: u32,
    pub mhz: u32,
    pub preferred: bool,
}

#[derive(Clone, Debug)]
pub struct Output {
    /// Same format Slint matches `SLINT_DRM_OUTPUT` against: `DP-2`, `HDMI-A-1`.
    pub name: String,
    /// Kernel order — the index is what `SLINT_DRM_MODE` expects.
    pub modes: Vec<ModeInfo>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Pick {
    pub name: String,
    pub mode_index: usize,
    pub mode: ModeInfo,
}

pub(crate) fn mode_infos(modes: &[Mode]) -> Vec<ModeInfo> {
    modes
        .iter()
        .map(|m| ModeInfo {
            w: m.size().0 as u32,
            h: m.size().1 as u32,
            mhz: mode_mhz(m),
            preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
        })
        .collect()
}

fn mode_mhz(m: &Mode) -> u32 {
    let (_, _, htotal) = m.hsync();
    let (_, _, vtotal) = m.vsync();
    let denom = htotal as u64 * vtotal as u64;
    if denom == 0 {
        return m.vrefresh() * 1000;
    }
    (m.clock() as u64 * 1_000_000 / denom) as u32
}

/// Best mode of one output: the preferred resolution, at its top refresh.
pub(crate) fn best_mode(modes: &[ModeInfo]) -> Option<(usize, &ModeInfo)> {
    let base = modes
        .iter()
        .filter(|m| m.w * m.h > 0)
        .max_by_key(|m| (m.preferred, m.w * m.h))?;
    modes
        .iter()
        .enumerate()
        .filter(|(_, m)| m.w == base.w && m.h == base.h)
        .max_by_key(|(_, m)| m.mhz)
}

/// Pure selection over probed outputs (unit-tested; no hardware).
pub fn choose(outputs: &[Output]) -> Option<Pick> {
    outputs
        .iter()
        .filter_map(|o| {
            let (i, m) = best_mode(&o.modes)?;
            Some((o, i, m))
        })
        .max_by(|(oa, _, a), (ob, _, b)| {
            (a.w * a.h, a.mhz)
                .cmp(&(b.w * b.h, b.mhz))
                // On a full tie the alphabetically first name wins.
                .then_with(|| ob.name.cmp(&oa.name))
        })
        .map(|(o, i, m)| Pick { name: o.name.clone(), mode_index: i, mode: m.clone() })
}

fn probe() -> Vec<Output> {
    let mut cards: Vec<(u32, std::path::PathBuf)> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n: u32 = name.strip_prefix("card")?.parse().ok()?;
            Some((n, e.path()))
        })
        .collect();
    cards.sort();

    let mut outputs = Vec::new();
    for (_, path) in cards {
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        let card = Card(file);
        let Ok(res) = card.resource_handles() else { continue };
        for h in res.connectors() {
            // force_probe: don't trust the kernel's cached list. At boot a
            // DP monitor can still be mid-EDID-read and report a stub list
            // (9 modes, 720p preferred) instead of its real 27.
            let Ok(con) = card.get_connector(*h, true) else { continue };
            if con.state() != connector::State::Connected {
                continue;
            }
            outputs.push(Output {
                name: format!("{}-{}", con.interface().as_str(), con.interface_id()),
                modes: mode_infos(con.modes()),
            });
        }
    }
    outputs
}

/// Probe until the connector/mode counts stop changing (monitors that are
/// still waking up report partial mode lists), up to ~2 s.
fn probe_settled() -> Vec<Output> {
    let sig = |o: &[Output]| o.iter().map(|x| (x.name.clone(), x.modes.len())).collect::<Vec<_>>();
    let mut prev = probe();
    for _ in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let cur = probe();
        let stable = sig(&cur) == sig(&prev);
        prev = cur;
        if stable {
            break;
        }
    }
    prev
}

/// Probe, choose, and export `SLINT_DRM_OUTPUT` / `SLINT_DRM_MODE`. Call
/// before the first Slint API use (the backend reads them on creation) and
/// while still single-threaded. Never fatal: on any failure Slint's own
/// default applies.
pub fn configure_slint_output() {
    if std::env::var_os("SLINT_DRM_OUTPUT").is_some() {
        return;
    }
    let outputs = probe_settled();
    for o in &outputs {
        eprintln!("[velogin] display: {} ({} modes)", o.name, o.modes.len());
    }
    match choose(&outputs) {
        Some(p) => {
            eprintln!(
                "[velogin] display: using {} {}x{}@{:.2}Hz (mode #{})",
                p.name,
                p.mode.w,
                p.mode.h,
                p.mode.mhz as f32 / 1000.0,
                p.mode_index
            );
            std::env::set_var("SLINT_DRM_OUTPUT", &p.name);
            std::env::set_var("SLINT_DRM_MODE", p.mode_index.to_string());
        }
        None => eprintln!("[velogin] display: nothing detected, leaving it to Slint"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hardware check: `cargo test -p veil-login live_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_probe() {
        let outs = probe();
        for o in &outs {
            eprintln!("{}: {:?}", o.name, o.modes.iter().map(|m| (m.w, m.h, m.mhz / 1000, m.preferred)).collect::<Vec<_>>());
        }
        eprintln!("pick: {:?}", choose(&outs));
    }

    fn m(w: u32, h: u32, hz: u32, preferred: bool) -> ModeInfo {
        ModeInfo { w, h, mhz: hz * 1000, preferred }
    }

    #[test]
    fn picks_the_big_monitor_at_its_top_refresh() {
        let outs = [
            Output { name: "HDMI-A-1".into(), modes: vec![m(1280, 720, 60, true), m(720, 480, 60, false)] },
            Output {
                name: "DP-2".into(),
                modes: vec![m(1920, 1080, 60, true), m(1920, 1080, 180, false), m(1280, 720, 60, false)],
            },
        ];
        let p = choose(&outs).unwrap();
        assert_eq!((p.name.as_str(), p.mode_index), ("DP-2", 1));
        assert_eq!(p.mode.mhz, 180_000);
    }

    #[test]
    fn resolution_comes_from_the_preferred_mode() {
        // A panel offering a bigger non-preferred mode keeps its native one.
        let outs = [Output {
            name: "eDP-1".into(),
            modes: vec![m(1920, 1080, 60, true), m(3840, 2160, 30, false)],
        }];
        let p = choose(&outs).unwrap();
        assert_eq!((p.mode.w, p.mode_index), (1920, 0));
    }

    #[test]
    fn ties_are_deterministic_and_empty_is_none() {
        let outs = [
            Output { name: "DP-3".into(), modes: vec![m(1920, 1080, 60, true)] },
            Output { name: "DP-1".into(), modes: vec![m(1920, 1080, 60, true)] },
        ];
        assert_eq!(choose(&outs).unwrap().name, "DP-1");
        assert!(choose(&[]).is_none());
    }
}
