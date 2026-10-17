//! Persistent state + asset lookup: remembered username, avatar, wallpaper.

use std::path::PathBuf;

const STATE_DIR: &str = "/var/lib/velogin";

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
    let mut users = std::fs::read_to_string("/etc/passwd")
        .map(|c| parse_users(&c))
        .unwrap_or_default();
    if users.is_empty() {
        if let Some(last) = last_user() {
            users.push(last);
        }
    }
    users
}

/// Pure passwd parser behind [`detect_users`]. A login shell of `nologin` or
/// `false` is matched on the shell's *file name*, not as a substring, so a
/// shell like `/opt/falsetto/bin/zsh` isn't mistaken for `/bin/false`.
fn parse_users(passwd: &str) -> Vec<String> {
    passwd
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            let uid: u32 = f[2].parse().ok()?;
            if !(1000..65534).contains(&uid) {
                return None;
            }
            let shell = f[6].trim();
            let base = shell.rsplit('/').next().unwrap_or(shell);
            if base == "nologin" || base == "false" {
                return None;
            }
            Some(f[0].to_string())
        })
        .collect()
}

/// Session *name* (not index — the detected list can reorder between boots).
pub fn last_session() -> Option<String> {
    read_state("last-session")
}

pub fn save_last_session(name: &str) {
    write_state("last-session", name);
}

fn read_state(file: &str) -> Option<String> {
    let s = std::fs::read_to_string(format!("{STATE_DIR}/{file}")).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn write_state(file: &str, value: &str) {
    // Best-effort: a read-only /var/lib must never break login.
    let _ = std::fs::create_dir_all(STATE_DIR);
    let _ = std::fs::write(format!("{STATE_DIR}/{file}"), value);
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1::/:/usr/bin/nologin
abyss:x:1000:1000:Abyss:/home/abyss:/usr/bin/zsh
svc:x:1001:1001::/srv/svc:/usr/bin/nologin
locked:x:1002:1002::/home/locked:/bin/false
odd:x:1003:1003::/home/odd:/opt/falsetto/bin/zsh
noshell:x:1004:1004::/home/noshell:
nobody:x:65534:65534:Nobody:/:/usr/bin/nologin
short:x:1005
garbage:x:notanumber:1:::/bin/sh
";

    #[test]
    fn parse_users_filters_system_and_nologin() {
        assert_eq!(parse_users(PASSWD), ["abyss", "odd", "noshell"]);
    }

    #[test]
    fn avatar_path_rejects_traversal() {
        assert!(avatar_path("").is_none());
        assert!(avatar_path("../etc/passwd").is_none());
        assert!(avatar_path("a/b").is_none());
    }
}
