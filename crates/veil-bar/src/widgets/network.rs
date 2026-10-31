//! Network status: Wi-Fi (with SSID and signal), wired, or offline.
//!
//! Link state comes from `/sys/class/net` and signal from
//! `/proc/net/wireless`, both cheap. The SSID needs `iw`, a subprocess, so
//! the whole probe runs on a sampler thread. NetworkManager over D-Bus is
//! left out on purpose: sysfs works with any network stack.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use veil_ui::{Metrics, Theme, Widget};

use crate::poll::{Poller, Seen};
use crate::util::{label, run_capture, truncate_chars};

#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Wifi { iface: String, signal: Option<u8> },
    Wired,
    Down,
}

pub struct NetworkWidget {
    poll: Poller<(Link, Option<String>)>,
    seen: Seen,
}

impl Default for NetworkWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkWidget {
    pub fn new() -> Self {
        let poll = Poller::every("network", Duration::from_secs(5), || {
            let link = probe(Path::new("/sys/class/net"), Path::new("/proc/net/wireless"));
            let ssid = match &link {
                Link::Wifi { iface, .. } => iw_ssid(iface),
                _ => None,
            };
            (link, ssid)
        });
        Self { poll, seen: Seen::default() }
    }
}

impl Widget for NetworkWidget {
    fn name(&self) -> &str {
        "network"
    }

    fn update(&mut self, _now: Instant) -> bool {
        self.seen.changed(&self.poll)
    }

    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &mut TextSystem,
        theme: &Theme,
        m: &Metrics,
        x: f32,
        y: f32,
        h: f32,
    ) -> f32 {
        let Some((link, ssid)) = self.poll.latest() else {
            return 0.0;
        };
        let (s, color) = describe(&link, ssid.as_deref(), theme);
        label(pixmap, text, m, x, y, h, &s, color)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn describe(link: &Link, ssid: Option<&str>, theme: &Theme) -> (String, tiny_skia::Color) {
    match link {
        Link::Wifi { signal, .. } => {
            let name = match (ssid, signal) {
                (Some(s), _) => truncate_chars(s, 18),
                (None, Some(p)) => format!("{p}%"),
                (None, None) => "wifi".into(),
            };
            (format!("󰖩 {name}"), theme.text_dim)
        }
        Link::Wired => ("󰈀 wired".into(), theme.text_dim),
        Link::Down => ("󰖪 offline".into(), theme.warn),
    }
}

/// Is `iface` a physical NIC (has a `device` link, unlike lo, veth, docker, tun...)?
fn is_physical(net: &Path, iface: &str) -> bool {
    net.join(iface).join("device").exists()
}

fn is_up(net: &Path, iface: &str) -> bool {
    let read = |f: &str| fs::read_to_string(net.join(iface).join(f)).unwrap_or_default();
    read("operstate").trim() == "up" || read("carrier").trim() == "1"
}

/// Signal quality in percent for `iface` from `/proc/net/wireless`.
fn wireless_signal(proc_wireless: &Path, iface: &str) -> Option<u8> {
    let content = fs::read_to_string(proc_wireless).ok()?;
    content.lines().skip(2).find_map(|l| {
        let (name, rest) = l.split_once(':')?;
        if name.trim() != iface {
            return None;
        }
        // columns: status, link quality (0..=70), level, noise, ...
        let link: f32 = rest.split_whitespace().nth(1)?.trim_end_matches('.').parse().ok()?;
        Some((link * 100.0 / 70.0).round().clamp(0.0, 100.0) as u8)
    })
}

/// The connection state, from sysfs only. Wi-Fi wins over wired when both are up.
pub fn probe(net: &Path, proc_wireless: &Path) -> Link {
    let Ok(rd) = fs::read_dir(net) else {
        return Link::Down;
    };
    let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();

    let mut wired = false;
    for iface in &names {
        if iface == "lo" || !is_physical(net, iface) || !is_up(net, iface) {
            continue;
        }
        if net.join(iface).join("wireless").exists() {
            return Link::Wifi { iface: iface.clone(), signal: wireless_signal(proc_wireless, iface) };
        }
        wired = true;
    }
    if wired {
        Link::Wired
    } else {
        Link::Down
    }
}

/// `iw dev <iface> link` → SSID. `None` when `iw` is missing or not associated.
fn iw_ssid(iface: &str) -> Option<String> {
    let out = run_capture("iw", &["dev", iface, "link"], Duration::from_secs(2))?;
    parse_iw_ssid(&out)
}

pub fn parse_iw_ssid(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("SSID: "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(net: &Path, name: &str, physical: bool, wireless: bool, operstate: &str) {
        let d = net.join(name);
        fs::create_dir_all(&d).unwrap();
        if physical {
            fs::create_dir_all(d.join("device")).unwrap();
        }
        if wireless {
            fs::create_dir_all(d.join("wireless")).unwrap();
        }
        fs::write(d.join("operstate"), format!("{operstate}\n")).unwrap();
    }

    const PROC: &str = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\nwlan0: 0000   49.  -61.  -256        0      0      0      0      0        0\n";

    #[test]
    fn wifi_up_reports_signal_percent() {
        let t = tempfile::tempdir().unwrap();
        let net = t.path().join("net");
        nic(&net, "lo", false, false, "unknown");
        nic(&net, "wlan0", true, true, "up");
        let proc = t.path().join("wireless");
        fs::write(&proc, PROC).unwrap();
        assert_eq!(probe(&net, &proc), Link::Wifi { iface: "wlan0".into(), signal: Some(70) });
    }

    #[test]
    fn wired_up_and_virtual_interfaces_ignored() {
        let t = tempfile::tempdir().unwrap();
        let net = t.path().join("net");
        nic(&net, "docker0", false, false, "up");
        nic(&net, "veth12", false, false, "up");
        nic(&net, "enp5s0", true, false, "up");
        assert_eq!(probe(&net, &t.path().join("none")), Link::Wired);
    }

    #[test]
    fn everything_down_is_offline() {
        let t = tempfile::tempdir().unwrap();
        let net = t.path().join("net");
        nic(&net, "enp5s0", true, false, "down");
        nic(&net, "wlan0", true, true, "down");
        assert_eq!(probe(&net, &t.path().join("none")), Link::Down);
        assert_eq!(probe(&t.path().join("missing"), &t.path().join("none")), Link::Down);
    }

    #[test]
    fn wifi_beats_wired() {
        let t = tempfile::tempdir().unwrap();
        let net = t.path().join("net");
        nic(&net, "enp5s0", true, false, "up");
        nic(&net, "wlan0", true, true, "up");
        assert!(matches!(probe(&net, &t.path().join("none")), Link::Wifi { signal: None, .. }));
    }

    #[test]
    fn parses_iw_output() {
        let out = "Connected to aa:bb:cc:dd:ee:ff (on wlan0)\n\tSSID: Home Net\n\tfreq: 5180\n";
        assert_eq!(parse_iw_ssid(out).as_deref(), Some("Home Net"));
        assert_eq!(parse_iw_ssid("Not connected.\n"), None);
    }

    #[test]
    fn signal_parsing_handles_a_missing_interface() {
        let t = tempfile::tempdir().unwrap();
        let proc = t.path().join("w");
        fs::write(&proc, PROC).unwrap();
        assert_eq!(wireless_signal(&proc, "wlan9"), None);
        assert_eq!(wireless_signal(&t.path().join("nope"), "wlan0"), None);
    }
}
