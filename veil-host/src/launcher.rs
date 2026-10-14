//! Alt+D-style app launcher (Combo 4 follow-up).
//!
//! Closing your only window used to strand you: veil-host's `run` spawns one
//! client and ties its own process lifetime to that child's exit (see the
//! reaper thread in `server.rs`), and there was no way to summon another
//! client short of `run -a` from a second terminal, which doesn't exist if
//! veil is the only thing on the TTY. This gives the compositor itself a
//! modal that can spawn a new client at any time, even with zero live
//! toplevels.
//!
//! Two match sources: a typed string is run verbatim via `sh -c` if it
//! doesn't match anything, or `.desktop` entries under the standard XDG
//! application dirs, substring-filtered by name.

use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct DesktopEntry {
    pub name: String,
    pub exec: String,
}

#[derive(Clone)]
pub struct Launcher {
    pub query: String,
    pub selected: usize,
    entries: Vec<DesktopEntry>,
}

impl Default for Launcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Launcher {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            selected: 0,
            entries: scan_desktop_entries(),
        }
    }

    /// Entries whose name contains the query (case-insensitive). Empty query
    /// returns everything, alphabetical.
    pub fn matches(&self) -> Vec<&DesktopEntry> {
        if self.query.is_empty() {
            return self.entries.iter().collect();
        }
        let q = self.query.to_ascii_lowercase();
        self.entries
            .iter()
            .filter(|e| e.name.to_ascii_lowercase().contains(&q))
            .collect()
    }
}

/// H7: directories to scan, in precedence order. Used to be three hardcoded
/// paths, so Flatpak and Nix apps (and anyone with a custom `XDG_DATA_DIRS`)
/// were invisible. Now: `XDG_DATA_HOME` (default `~/.local/share`), then
/// `XDG_DATA_DIRS` (default `/usr/local/share:/usr/share`), then the usual
/// Flatpak and Nix export dirs, which a bare-TTY session often doesn't have in
/// its environment. User dirs come first so a user's own entry beats a system
/// one of the same name in the dedup below. Nonexistent dirs are harmless —
/// `read_dir` just fails and the scan moves on.
fn application_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut bases: Vec<PathBuf> = Vec::new();

    match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => bases.push(PathBuf::from(d)),
        None => {
            if let Some(h) = &home {
                bases.push(h.join(".local/share"));
            }
        }
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    bases.extend(
        data_dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    if let Some(h) = &home {
        bases.push(h.join(".local/share/flatpak/exports/share"));
        bases.push(h.join(".nix-profile/share"));
    }
    for extra in [
        "/var/lib/flatpak/exports/share",
        "/nix/var/nix/profiles/default/share",
        "/run/current-system/sw/share",
    ] {
        bases.push(PathBuf::from(extra));
    }

    let mut seen = std::collections::HashSet::new();
    let mut dirs = Vec::new();
    for b in bases {
        let d = b.join("applications");
        if seen.insert(d.clone()) {
            dirs.push(d);
        }
    }
    dirs
}

/// True if an executable named `bin` is on `PATH`.
fn in_path(bin: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| {
        std::fs::metadata(d.join(bin))
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// H7: how to run a `Terminal=true` entry. `$TERMINAL` if set, else the first
/// common emulator on `PATH`; all of these take `-e <command>`. `None` means
/// there's nothing to run a terminal app in, so those entries are skipped
/// rather than listed and then dying instantly with no window to show why.
fn terminal_prefix() -> Option<String> {
    if let Ok(t) = std::env::var("TERMINAL") {
        let t = t.trim();
        if !t.is_empty() {
            return Some(format!("{t} -e"));
        }
    }
    ["foot", "alacritty", "kitty", "konsole", "xterm"]
        .iter()
        .find(|t| in_path(t))
        .map(|t| format!("{t} -e"))
}

/// Scan the XDG application dirs (see `application_dirs`) for `.desktop`
/// files. Not full XDG spec compliance — just enough to list launchable
/// apps: `Name=`, `Exec=` (strips `%f`/`%u`/etc field codes, including
/// mid-token ones like `--file=%u`), skips `NoDisplay=true` / `Hidden=true`,
/// and wraps `Terminal=true` entries in a terminal emulator.
fn scan_desktop_entries() -> Vec<DesktopEntry> {
    let term = terminal_prefix();
    let mut out = Vec::new();
    for dir in &application_dirs() {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(de) = parse_desktop_file(&path, term.as_deref()) {
                out.push(de);
            }
        }
    }
    // Stable sort + dedup keeps the FIRST entry per name, i.e. the one from
    // the highest-precedence directory.
    out.sort_by_key(|a| a.name.to_ascii_lowercase());
    out.dedup_by(|a, b| a.name == b.name);
    out
}

fn parse_desktop_file(path: &Path, term: Option<&str>) -> Option<DesktopEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut skip = false;
    let mut terminal = false;
    let mut in_main_section = false;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main_section = line == "[Desktop Entry]";
            continue;
        }
        if !in_main_section {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            if name.is_none() {
                name = Some(v.to_string());
            }
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec = Some(v.to_string());
        } else if line == "NoDisplay=true" || line == "Hidden=true" {
            skip = true;
        } else if line == "Terminal=true" {
            terminal = true;
        }
    }

    if skip {
        return None;
    }
    let name = name?;
    let mut exec = clean_exec(&exec?);
    if exec.is_empty() {
        return None;
    }
    if terminal {
        // H7: Terminal=true apps (htop, ncdu, ...) need a terminal to run in.
        exec = format!("{} {exec}", term?);
    }
    Some(DesktopEntry { name, exec })
}

/// Strip desktop-entry field codes (`%f`, `%F`, `%u`, `%U`, `%i`, `%c`, `%k`,
/// etc.) and decode `%%` to a literal `%`. Character-by-character so mid-token
/// codes (`--file=%u`) are caught — the old token filter only dropped tokens
/// that were exactly `%x`, leaving `--file=%u` to reach the shell verbatim.
/// Same logic as `veil-login`'s copy; unifying them needs a shared crate.
fn clean_exec(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some('%') = chars.next() {
                out.push('%'); // %% -> literal %
            } // any other %x field code, or a trailing lone %, is stripped
        } else {
            out.push(c);
        }
    }
    // Stripping mid-token codes can leave runs of spaces; normalize.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::clean_exec;

    #[test]
    fn strips_standalone_and_mid_token_codes() {
        assert_eq!(clean_exec("firefox %u"), "firefox");
        assert_eq!(clean_exec("app --file=%f --x"), "app --file= --x");
        assert_eq!(clean_exec("app %F %i %c %k"), "app");
    }

    #[test]
    fn decodes_percent_percent() {
        assert_eq!(clean_exec("echo 100%%"), "echo 100%");
        assert_eq!(clean_exec("app 50%"), "app 50");
    }
}
