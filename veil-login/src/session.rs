//! Session detection: what can we log the user into?
//!
//! Dynamic detection: Veil session, shell, and .desktop files in standard
//! wayland/x11 session directories.

use std::path::Path;

#[derive(Clone, Debug)]
pub struct SessionEntry {
    pub name: String,
    /// Shell command line, run as `<user shell> -lc "exec <exec>"`.
    /// Empty exec means "just a login shell".
    pub exec: String,
}

/// Dynamically locate the veil-host executable or allow override via VELOGIN_DEFAULT_EXEC.
pub fn default_veil_exec() -> String {
    if let Ok(env_exec) = std::env::var("VELOGIN_DEFAULT_EXEC") {
        if !env_exec.trim().is_empty() {
            return env_exec;
        }
    }
    // Check if veil-host exists in the same directory as this binary
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join("veil-host");
            if candidate.is_file() {
                return format!("{} start", candidate.display());
            }
        }
    }
    "veil-host start".to_string()
}

pub fn detect() -> Vec<SessionEntry> {
    let mut sessions = vec![
        SessionEntry { name: "Veil".into(), exec: default_veil_exec() },
        SessionEntry { name: "Shell".into(), exec: String::new() },
    ];

    for dir in ["/usr/share/wayland-sessions", "/usr/share/xsessions"] {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "desktop") {
                if let Some(s) = from_desktop_file(&path) {
                    // skip duplicates by name (same DE in both dirs)
                    if !sessions.iter().any(|e| e.name == s.name) {
                        sessions.push(s);
                    }
                }
            }
        }
    }

    sessions
}

fn from_desktop_file(path: &Path) -> Option<SessionEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let raw_exec = ini_get(&content, "Desktop Entry", "Exec")?;
    let exec = clean_exec(&raw_exec);
    if exec.is_empty() { return None; }
    let name = ini_get(&content, "Desktop Entry", "Name").unwrap_or_else(|| {
        path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
    });
    Some(SessionEntry { name, exec })
}

/// Strip desktop-entry field codes (`%f`, `%F`, `%u`, `%U`, `%i`, `%c`, `%k`, `%%`).
fn clean_exec(exec: &str) -> String {
    exec.split_whitespace()
        .filter(|tok| !(tok.len() == 2 && tok.starts_with('%')))
        .collect::<Vec<_>>()
        .join(" ")
}

fn ini_get(content: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == format!("[{section}]");
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}
