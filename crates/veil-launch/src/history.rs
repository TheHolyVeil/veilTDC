//! What gets launched often, so it comes first: a small count-and-recency
//! history ("frecency") kept in one text file under the state directory.
//!
//! The file is `count<TAB>last_unix_secs<TAB>desktop_id` per line. A damaged
//! line is skipped, a missing or unreadable file is an empty history, and a
//! failed save never gets in the way of launching.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Entries kept; the least recently launched are dropped past this.
const MAX_ENTRIES: usize = 400;
const DAY: u64 = 86_400;

#[derive(Debug, Default)]
pub struct History {
    entries: HashMap<String, (u32, u64)>,
    path: Option<PathBuf>,
}

impl History {
    /// A history that is never saved (tests, and when there is no state dir).
    pub fn in_memory() -> Self {
        History::default()
    }

    pub fn load(path: &Path) -> Self {
        let mut entries = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let mut f = line.splitn(3, '\t');
                let (Some(count), Some(last), Some(id)) = (f.next(), f.next(), f.next()) else { continue };
                let (Ok(count), Ok(last)) = (count.parse::<u32>(), last.parse::<u64>()) else { continue };
                if !id.is_empty() {
                    entries.insert(id.to_string(), (count, last));
                }
            }
        }
        History { entries, path: Some(path.to_path_buf()) }
    }

    /// `$XDG_STATE_HOME/veil/launcher-history`, falling back to
    /// `~/.local/state/veil/launcher-history`.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| Path::new(&h).join(".local/state")))?;
        Some(base.join("veil").join("launcher-history"))
    }

    /// Ranking bonus for `id` at `now` (unix seconds): up to 120 for how often
    /// and up to 60 for how recently. Zero for something never launched.
    pub fn boost(&self, id: &str, now: u64) -> i32 {
        let Some(&(count, last)) = self.entries.get(id) else { return 0 };
        let often = count.min(10) as i32 * 12;
        let age = now.saturating_sub(last);
        let recent = if age < DAY {
            60
        } else if age < 7 * DAY {
            30
        } else if age < 30 * DAY {
            10
        } else {
            0
        };
        often + recent
    }

    /// Count a launch and save. Ids that could not survive the file format are
    /// ignored.
    pub fn record(&mut self, id: &str, now: u64) {
        if id.is_empty() || id.contains(['\t', '\n', '\r']) {
            return;
        }
        let e = self.entries.entry(id.to_string()).or_insert((0, now));
        e.0 = e.0.saturating_add(1);
        e.1 = now;
        self.prune();
        self.save();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn prune(&mut self) {
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }
        let mut by_age: Vec<(u64, String)> = self.entries.iter().map(|(k, v)| (v.1, k.clone())).collect();
        by_age.sort();
        for (_, id) in by_age.into_iter().take(self.entries.len() - MAX_ENTRIES) {
            self.entries.remove(&id);
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let mut lines: Vec<(&String, &(u32, u64))> = self.entries.iter().collect();
        lines.sort_by(|a, b| b.1 .1.cmp(&a.1 .1).then_with(|| a.0.cmp(b.0)));
        let body: String = lines.iter().map(|(id, (c, t))| format!("{c}\t{t}\t{id}\n")).collect();
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
        }
        // Write beside it and rename, so a crash never leaves half a file.
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_800_000_000;

    #[test]
    fn unknown_apps_have_no_boost() {
        assert_eq!(History::in_memory().boost("a.desktop", T), 0);
    }

    #[test]
    fn more_launches_and_more_recent_launches_rank_higher() {
        let mut h = History::in_memory();
        h.record("once.desktop", T);
        for _ in 0..5 {
            h.record("often.desktop", T);
        }
        assert!(h.boost("often.desktop", T) > h.boost("once.desktop", T));
        // same count, older launch ranks lower, in steps
        let mut h = History::in_memory();
        h.record("a.desktop", T);
        let at = |dt: u64| h.boost("a.desktop", T + dt);
        assert!(at(0) > at(2 * DAY) && at(2 * DAY) > at(10 * DAY) && at(10 * DAY) > at(40 * DAY));
        assert_eq!(at(40 * DAY), 12, "still remembered, just no recency bonus");
    }

    #[test]
    fn the_boost_is_bounded() {
        let mut h = History::in_memory();
        for _ in 0..1000 {
            h.record("a.desktop", T);
        }
        assert_eq!(h.boost("a.desktop", T), 180);
    }

    #[test]
    fn a_clock_that_went_backwards_counts_as_just_now() {
        let mut h = History::in_memory();
        h.record("a.desktop", T);
        assert_eq!(h.boost("a.desktop", T - 5 * DAY), 12 + 60);
    }

    #[test]
    fn it_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/dir/history");
        let mut h = History::load(&path);
        assert!(h.is_empty());
        h.record("foot.desktop", T);
        h.record("foot.desktop", T + 5);
        h.record("zen.desktop", T + 9);
        let again = History::load(&path);
        assert_eq!(again.len(), 2);
        assert_eq!(again.boost("foot.desktop", T + 9), 24 + 60);
        assert!(!path.with_extension("tmp").exists(), "no leftover temp file");
    }

    #[test]
    fn damaged_lines_are_skipped_and_the_rest_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        std::fs::write(&path, "3\t100\tgood.desktop\nnot a line\n\tx\t\n-1\t5\tneg.desktop\n2\tabc\tbad-time.desktop\n1\t50\t\n7\t200\twith space.desktop\n").unwrap();
        let h = History::load(&path);
        assert_eq!(h.len(), 2);
        assert!(h.boost("good.desktop", 100) > 0);
        assert!(h.boost("with space.desktop", 200) > 0);
    }

    #[test]
    fn a_missing_or_unwritable_location_is_harmless() {
        assert!(History::load(Path::new("/definitely/not/here/history")).is_empty());
        let mut h = History::load(Path::new("/proc/definitely/not/writable/history"));
        h.record("a.desktop", T); // must not panic
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn ids_that_break_the_format_are_ignored() {
        let mut h = History::in_memory();
        for bad in ["", "a\tb", "a\nb", "a\rb"] {
            h.record(bad, T);
        }
        assert!(h.is_empty());
    }

    #[test]
    fn the_oldest_entries_are_dropped_past_the_cap() {
        let mut h = History::in_memory();
        for i in 0..(MAX_ENTRIES as u64 + 25) {
            h.record(&format!("app{i}.desktop"), T + i);
        }
        assert_eq!(h.len(), MAX_ENTRIES);
        assert_eq!(h.boost("app0.desktop", T), 0, "oldest gone");
        assert!(h.boost(&format!("app{}.desktop", MAX_ENTRIES + 24), T + 1000) > 0, "newest kept");
    }
}
