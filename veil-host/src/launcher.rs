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
    pub query:    String,
    pub selected: usize,
    entries:      Vec<DesktopEntry>,
}

impl Default for Launcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Launcher {
    pub fn new() -> Self {
        Self { query: String::new(), selected: 0, entries: scan_desktop_entries() }
    }

    /// Entries whose name contains the query (case-insensitive). Empty query
    /// returns everything, alphabetical.
    pub fn matches(&self) -> Vec<&DesktopEntry> {
        if self.query.is_empty() {
            return self.entries.iter().collect();
        }
        let q = self.query.to_ascii_lowercase();
        self.entries.iter().filter(|e| e.name.to_ascii_lowercase().contains(&q)).collect()
    }
}

/// Scan `/usr/share/applications`, `/usr/local/share/applications`, and
/// `~/.local/share/applications` for `.desktop` files. Not full XDG spec
/// compliance — just enough to list launchable apps: `Name=`, `Exec=`
/// (strips `%f`/`%u`/etc field codes), skips `NoDisplay=true` / `Hidden=true`.
fn scan_desktop_entries() -> Vec<DesktopEntry> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }

    let mut out = Vec::new();
    for dir in &dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(de) = parse_desktop_file(&path) {
                out.push(de);
            }
        }
    }
    out.sort_by_key(|a| a.name.to_ascii_lowercase());
    out.dedup_by(|a, b| a.name == b.name);
    out
}

fn parse_desktop_file(path: &Path) -> Option<DesktopEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut skip = false;
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
            if name.is_none() { name = Some(v.to_string()); }
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec = Some(v.to_string());
        } else if line == "NoDisplay=true" || line == "Hidden=true" {
            skip = true;
        }
    }

    if skip { return None; }
    let name = name?;
    let exec = clean_exec(&exec?);
    if exec.is_empty() { return None; }
    Some(DesktopEntry { name, exec })
}

/// Strip desktop-entry field codes (`%f`, `%F`, `%u`, `%U`, `%i`, `%c`, `%k`,
/// `%%`) — we're not passing files/icons through, just launching the app.
fn clean_exec(exec: &str) -> String {
    exec.split_whitespace()
        .filter(|tok| !(tok.len() == 2 && tok.starts_with('%')))
        .collect::<Vec<_>>()
        .join(" ")
}
