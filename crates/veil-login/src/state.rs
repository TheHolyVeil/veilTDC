//! Persistent state + asset lookup: remembered username, avatar, wallpaper.

use std::path::{Path, PathBuf};

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

/// Directories scanned for greeter wallpapers (non-recursive).
const WALLPAPER_DIRS: [&str; 2] = ["/etc/velogin", "/etc/velogin/wallpapers"];
/// Hard cap on candidates so a stuffed directory can't stall the greeter.
const MAX_WALLPAPERS: usize = 64;

/// Every image in the wallpaper dirs, sorted. Matched by name (png/jpg/jpeg,
/// or the legacy extensionless `background`); the decoder still judges by
/// content, so a mislabelled or corrupt file is skipped later, not trusted.
pub fn wallpaper_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = WALLPAPER_DIRS
        .iter()
        .flat_map(|d| wallpapers_in(Path::new(d)))
        .collect();
    out.sort();
    out.dedup();
    out.truncate(MAX_WALLPAPERS);
    out
}

fn wallpapers_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_wallpaper_name(p) && p.is_file())
        .collect()
}

fn is_wallpaper_name(p: &Path) -> bool {
    match p.extension().and_then(|e| e.to_str()) {
        Some(e) => matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg"),
        None => p.file_name().and_then(|n| n.to_str()) == Some("background"),
    }
}

/// Order in which to try the candidates: a random starting point, with the
/// previous login's wallpaper pushed to the very end (so it never repeats
/// back-to-back unless it's the only one, or the rest fail to decode).
pub fn wallpaper_order(mut c: Vec<PathBuf>, last: Option<&Path>, rnd: u64) -> Vec<PathBuf> {
    let mut prev = None;
    if c.len() > 1 {
        if let Some(i) = last.and_then(|l| c.iter().position(|p| p == l)) {
            prev = Some(c.remove(i));
        }
    }
    if !c.is_empty() {
        let n = c.len() as u64;
        c.rotate_left((rnd % n) as usize);
    }
    c.extend(prev);
    c
}

/// Random u64 from the kernel; falls back to the clock if /dev/urandom is
/// unavailable. Not security-relevant, it only picks a background.
pub fn random_u64() -> u64 {
    use std::io::Read;
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok()
    {
        return u64::from_le_bytes(b);
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

pub fn last_wallpaper() -> Option<PathBuf> {
    read_state("last-wallpaper").map(PathBuf::from)
}

pub fn save_last_wallpaper(path: &Path) {
    write_state("last-wallpaper", &path.to_string_lossy());
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

#[cfg(test)]
mod wallpaper_tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn name_filter_accepts_images_and_legacy_background_only() {
        for ok in ["/e/a.png", "/e/b.JPG", "/e/c.jpeg", "/e/background"] {
            assert!(is_wallpaper_name(Path::new(ok)), "{ok}");
        }
        for no in ["/e/notes.txt", "/e/config", "/e/a.png.bak", "/e/x.gif"] {
            assert!(!is_wallpaper_name(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn order_never_leads_with_last_when_alternatives_exist() {
        let c = paths(&["/a.png", "/b.png", "/c.png"]);
        for rnd in 0..12 {
            let o = wallpaper_order(c.clone(), Some(Path::new("/b.png")), rnd);
            assert_ne!(o[0], PathBuf::from("/b.png"));
            assert_eq!(o.last().unwrap(), &PathBuf::from("/b.png"));
            assert_eq!(o.len(), 3);
        }
    }

    #[test]
    fn order_handles_single_empty_and_unknown_last() {
        let one = paths(&["/a.png"]);
        assert_eq!(
            wallpaper_order(one.clone(), Some(Path::new("/a.png")), 7),
            one
        );
        assert!(wallpaper_order(Vec::new(), None, 3).is_empty());
        let two = paths(&["/a.png", "/b.png"]);
        let o = wallpaper_order(two, Some(Path::new("/gone.png")), 1);
        assert_eq!(o.len(), 2);
    }

    #[test]
    fn order_covers_every_candidate_across_seeds() {
        let c = paths(&["/a.png", "/b.png", "/c.png", "/d.png"]);
        let firsts: std::collections::HashSet<_> = (0..4)
            .map(|r| wallpaper_order(c.clone(), None, r)[0].clone())
            .collect();
        assert_eq!(firsts.len(), 4);
    }
}
