//! Persistent state + asset lookup: remembered username, avatar, wallpaper.

use std::path::PathBuf;

const STATE_DIR: &str = "/var/lib/velogin";
const LEGACY_STATE_DIR: &str = "/var/lib/abyss";

/// AccountsService avatar convention — populated by every mainstream DE's
/// user-settings panel, so we get avatars for free.
pub fn avatar_path(username: &str) -> Option<PathBuf> {
    // Refuse path-traversal shaped usernames outright.
    if username.is_empty() || username.contains('/') || username.contains("..") {
        return None;
    }
    let p = PathBuf::from("/var/lib/AccountsService/icons").join(username);
    p.is_file().then_some(p)
}

/// Optional background image. First hit wins.
pub fn wallpaper_path() -> Option<PathBuf> {
    // decoded by content, so the extension is just a courtesy
    [
        "/etc/velogin/background.png",
        "/etc/velogin/background.jpg",
        "/etc/velogin/background",
        "/etc/abyss/background.png",
        "/etc/abyss/background.jpg",
        "/etc/abyss/background",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

pub fn last_user() -> Option<String> {
    read_state("last-user")
}

pub fn save_last_user(username: &str) {
    write_state("last-user", username);
}

/// Detect normal non-system user accounts from /etc/passwd (UID >= 1000).
pub fn detect_users() -> Vec<String> {
    let mut users = Vec::new();
    if let Ok(content) = std::fs::read_to_string("/etc/passwd") {
        for line in content.lines() {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 7 {
                let name = parts[0];
                let uid: u32 = parts[2].parse().unwrap_or(0);
                let shell = parts[6];
                if (uid >= 1000 && uid < 65534)
                    && !shell.contains("nologin")
                    && !shell.contains("false")
                {
                    users.push(name.to_string());
                }
            }
        }
    }
    if users.is_empty() {
        if let Some(last) = last_user() {
            users.push(last);
        }
    }
    users
}

/// Session *name* (not index — the detected list can reorder between boots).
pub fn last_session() -> Option<String> {
    read_state("last-session")
}

pub fn save_last_session(name: &str) {
    write_state("last-session", name);
}

fn read_state(file: &str) -> Option<String> {
    let path = format!("{STATE_DIR}/{file}");
    let legacy_path = format!("{LEGACY_STATE_DIR}/{file}");
    let s = std::fs::read_to_string(&path)
        .or_else(|_| std::fs::read_to_string(&legacy_path))
        .ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn write_state(file: &str, value: &str) {
    // Best-effort: a read-only /var/lib must never break login.
    let _ = std::fs::create_dir_all(STATE_DIR);
    let _ = std::fs::write(format!("{STATE_DIR}/{file}"), value);
}
