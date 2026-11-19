//! Battery state for the lock screen's status pill, read from sysfs.
//!
//! `/sys/class/power_supply/BAT*`: the first one that has a `capacity`. No
//! battery (a desktop) gives no reading and the screen draws nothing. The time
//! estimate is smoothed, because `power_now` moves with every burst of load and
//! a raw estimate would jump around.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SYSFS: &str = "/sys/class/power_supply";
/// How often sysfs is read; the clock redraws every second regardless.
const POLL: Duration = Duration::from_secs(15);
/// Weight of a new rate sample in the smoothed rate.
const SMOOTHING: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Discharging,
    Charging,
    Full,
    /// Plugged in but not charging (a charge limit is holding the battery).
    Held,
    /// The kernel does not say.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub pct: u8,
    pub state: State,
    /// Minutes until empty (discharging) or full (charging).
    pub minutes: Option<u32>,
}

impl Reading {
    pub fn plugged_in(&self) -> bool {
        matches!(self.state, State::Charging | State::Full | State::Held)
    }

    /// Material Design Icons glyph (Nerd Font) for the level, in steps of ten.
    pub fn icon(&self) -> char {
        if self.state == State::Charging {
            return '\u{f0084}'; // battery-charging
        }
        match (usize::from(self.pct) + 5) / 10 {
            0 => '\u{f0083}',  // battery-alert
            10 => '\u{f0079}', // battery
            n => char::from_u32(0xF0079 + n as u32).unwrap_or('\u{f0079}'), // battery-10 .. battery-90
        }
    }

    pub fn label(&self) -> String {
        let pct = self.pct;
        match (self.state, self.minutes) {
            (State::Discharging, Some(m)) => format!("{pct}% · {} left", duration(m)),
            (State::Charging, Some(m)) => format!("{pct}% · {} to full", duration(m)),
            (State::Charging, None) => format!("{pct}% · charging"),
            (State::Full, _) => format!("{pct}% · full"),
            (State::Held, _) => format!("{pct}% · plugged in"),
            _ => format!("{pct}%"),
        }
    }
}

fn duration(minutes: u32) -> String {
    if minutes >= 60 {
        format!("{}h {:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

pub struct Battery {
    base: PathBuf,
    next_poll: Instant,
    /// (was charging, rate): the smoothed draw or charge rate.
    smoothed: Option<(bool, f64)>,
    reading: Option<Reading>,
}

impl Battery {
    pub fn new(now: Instant) -> Self {
        Self::at(SYSFS, now)
    }

    fn at(base: impl Into<PathBuf>, now: Instant) -> Self {
        let mut b = Battery { base: base.into(), next_poll: now, smoothed: None, reading: None };
        b.poll(now);
        b
    }

    pub fn reading(&self) -> Option<Reading> {
        self.reading
    }

    /// Read sysfs if it is due. True when what is shown changed.
    pub fn poll(&mut self, now: Instant) -> bool {
        if now < self.next_poll {
            return false;
        }
        self.next_poll = now + POLL;
        let next = sample(&self.base).map(|s| self.reading_from(&s));
        if next.is_none() {
            self.smoothed = None;
        }
        let changed = next != self.reading;
        self.reading = next;
        changed
    }

    fn reading_from(&mut self, s: &Sample) -> Reading {
        let mut minutes = None;
        let charging = s.state == State::Charging;
        match s.units {
            Some(u) if charging || s.state == State::Discharging => {
                if u.rate > 0.0 {
                    let rate = match self.smoothed {
                        Some((was, old)) if was == charging => old + SMOOTHING * (u.rate - old),
                        _ => u.rate,
                    };
                    self.smoothed = Some((charging, rate));
                    let hours = if charging { (u.full - u.now).max(0.0) / rate } else { u.now / rate };
                    if hours > 0.0 && hours < 100.0 {
                        minutes = Some((hours * 60.0).round() as u32);
                    }
                }
            }
            _ => self.smoothed = None,
        }
        Reading { pct: s.pct, state: s.state, minutes }
    }
}

#[derive(Clone, Copy)]
struct Units {
    now: f64,
    full: f64,
    rate: f64,
}

struct Sample {
    pct: u8,
    state: State,
    units: Option<Units>,
}

fn sample(base: &Path) -> Option<Sample> {
    let mut bats: Vec<PathBuf> = fs::read_dir(base)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("BAT"))
        .map(|e| e.path())
        .collect();
    bats.sort();
    let (dir, pct) = bats.into_iter().find_map(|d| {
        let p = num(&d, "capacity")?;
        Some((d, p))
    })?;
    let state = match text(&dir, "status").as_deref() {
        Some("Charging") => State::Charging,
        Some("Discharging") => State::Discharging,
        Some("Full") => State::Full,
        Some("Not charging") => State::Held,
        _ => State::Unknown,
    };
    // Energy (µWh, µW) or charge (µAh, µA): only the ratios are used, so the
    // unit does not matter. Some drivers report the current as negative.
    let units = [("energy_now", "energy_full", "power_now"), ("charge_now", "charge_full", "current_now")]
        .into_iter()
        .find_map(|(n, f, r)| Some(Units { now: num(&dir, n)?, full: num(&dir, f)?, rate: num(&dir, r)?.abs() }));
    Some(Sample { pct: pct.clamp(0.0, 100.0).round() as u8, state, units })
}

fn text(dir: &Path, name: &str) -> Option<String> {
    Some(fs::read_to_string(dir.join(name)).ok()?.trim().to_string())
}

fn num(dir: &Path, name: &str) -> Option<f64> {
    text(dir, name)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `power_supply` directory with one `BAT1`, in the shape of a
    /// Framework 12 (charge and current, not energy and power).
    fn fake(name: &str, status: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("veil-lock-battery-{}-{name}", std::process::id()));
        let bat = base.join("BAT1");
        fs::create_dir_all(&bat).unwrap();
        for (f, v) in [
            ("capacity", "88"),
            ("status", status),
            ("charge_now", "3679000"),
            ("charge_full", "4193000"),
            ("current_now", "486000"),
        ] {
            fs::write(bat.join(f), format!("{v}\n")).unwrap();
        }
        base
    }

    fn read(base: &Path) -> Reading {
        Battery::at(base, Instant::now()).reading().expect("a battery")
    }

    #[test]
    fn discharging_gives_time_to_empty() {
        let r = read(&fake("dis", "Discharging"));
        assert_eq!((r.pct, r.state, r.minutes), (88, State::Discharging, Some(454)));
        assert_eq!(r.label(), "88% · 7h 34m left");
        assert!(!r.plugged_in());
    }

    #[test]
    fn charging_gives_time_to_full() {
        let r = read(&fake("chg", "Charging"));
        assert_eq!(r.minutes, Some(63));
        assert_eq!(r.label(), "88% · 1h 03m to full");
        assert!(r.plugged_in());
    }

    #[test]
    fn a_charge_limit_hold_is_plugged_in_with_no_estimate() {
        let r = read(&fake("held", "Not charging"));
        assert_eq!((r.state, r.minutes), (State::Held, None));
        assert_eq!(r.label(), "88% · plugged in");
    }

    #[test]
    fn full_and_unknown_status_have_no_estimate() {
        assert_eq!(read(&fake("full", "Full")).label(), "88% · full");
        assert_eq!(read(&fake("unk", "Mystery")).label(), "88%");
    }

    #[test]
    fn no_battery_gives_no_reading() {
        let base = std::env::temp_dir().join(format!("veil-lock-battery-{}-none", std::process::id()));
        fs::create_dir_all(base.join("ACAD")).unwrap();
        assert!(Battery::at(&base, Instant::now()).reading().is_none());
        assert!(Battery::at("/definitely/not/a/dir", Instant::now()).reading().is_none());
    }

    #[test]
    fn a_sudden_load_spike_moves_the_estimate_only_part_way() {
        let base = fake("smooth", "Discharging");
        let t0 = Instant::now();
        let mut b = Battery::at(&base, t0);
        assert_eq!(b.reading().unwrap().minutes, Some(454));
        // Not due yet: nothing is re-read.
        fs::write(base.join("BAT1/current_now"), "972000\n").unwrap();
        assert!(!b.poll(t0 + Duration::from_secs(1)));
        // Due: the rate doubled, the estimate does not halve at once.
        assert!(b.poll(t0 + POLL));
        assert_eq!(b.reading().unwrap().minutes, Some(363));
    }

    #[test]
    fn a_negative_current_is_still_a_rate() {
        let base = fake("neg", "Discharging");
        fs::write(base.join("BAT1/current_now"), "-486000\n").unwrap();
        assert_eq!(read(&base).minutes, Some(454));
    }

    #[test]
    fn the_icon_follows_the_level_in_steps_of_ten() {
        let at = |pct, state| Reading { pct, state, minutes: None }.icon();
        assert_eq!(at(100, State::Discharging), '\u{f0079}');
        assert_eq!(at(88, State::Discharging), '\u{f0082}'); // 90
        assert_eq!(at(12, State::Discharging), '\u{f007a}'); // 10
        assert_eq!(at(3, State::Discharging), '\u{f0083}'); // alert
        assert_eq!(at(3, State::Charging), '\u{f0084}');
    }
}
