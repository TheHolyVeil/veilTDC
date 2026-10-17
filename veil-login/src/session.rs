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
                return format!("{} start", shell_quote(&candidate.to_string_lossy()));
            }
        }
    }
    "veil-host start".to_string()
}

pub fn detect() -> Vec<SessionEntry> {
    let mut sessions = vec![
        SessionEntry {
            name: "Veil".into(),
            exec: default_veil_exec(),
        },
        SessionEntry {
            name: "Shell".into(),
            exec: String::new(),
        },
    ];

    for dir in ["/usr/share/wayland-sessions", "/usr/share/xsessions"] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
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
    if exec.is_empty() {
        return None;
    }
    let name = ini_get(&content, "Desktop Entry", "Name").unwrap_or_else(|| {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    Some(SessionEntry { name, exec })
}

/// Strip desktop-entry field codes (`%f`, `%F`, `%u`, `%U`, `%i`, `%c`, `%k`, etc.)
/// and decode `%%` → literal `%`.  Operates character-by-character so that
/// mid-token codes (`--file=%f`) are caught and `%%` is not mistakenly dropped.
fn clean_exec(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('%') => out.push('%'), // %% → literal %
                Some(_) => {}               // any other %x field code → strip
                None => {}                  // trailing lone % → strip
            }
        } else {
            out.push(c);
        }
    }
    // Re-join tokens: stripping mid-token codes can leave multiple spaces.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// Quote `s` for a POSIX shell. Bare when it only has safe characters, else
/// single-quoted with embedded quotes escaped. The exec line is run as
/// `<shell> -c "exec <exec>"`, so an unquoted path with a space or `;` in it
/// (e.g. a binary installed under an odd directory) would split or inject.
fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%".contains(&b));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_exec_strips_field_codes() {
        assert_eq!(clean_exec("app %U"), "app");
        assert_eq!(clean_exec("app --file=%f --x"), "app --file= --x");
        assert_eq!(clean_exec("app 100%% done %"), "app 100% done");
        assert_eq!(clean_exec("  a   b  "), "a b");
        assert_eq!(clean_exec("%F"), "");
    }

    #[test]
    fn ini_get_reads_only_the_named_section() {
        let ini = "[Other]\nExec=wrong\n[Desktop Entry]\nName = Foo\nExec=right --x\n";
        assert_eq!(ini_get(ini, "Desktop Entry", "Exec").as_deref(), Some("right --x"));
        assert_eq!(ini_get(ini, "Desktop Entry", "Name").as_deref(), Some("Foo"));
        assert_eq!(ini_get(ini, "Desktop Entry", "Missing"), None);
        assert_eq!(ini_get(ini, "Nope", "Exec"), None);
    }

    #[test]
    fn desktop_file_without_exec_is_skipped() {
        let dir = std::env::temp_dir().join(format!("velogin-sess-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.desktop");
        let bad = dir.join("bad.desktop");
        std::fs::write(&good, "[Desktop Entry]\nName=Good\nExec=good-wm %U\n").unwrap();
        std::fs::write(&bad, "[Desktop Entry]\nName=Bad\nExec=%U\n").unwrap();
        let s = from_desktop_file(&good).unwrap();
        assert_eq!((s.name.as_str(), s.exec.as_str()), ("Good", "good-wm"));
        assert!(from_desktop_file(&bad).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shell_quote_leaves_safe_paths_and_quotes_the_rest() {
        assert_eq!(shell_quote("/usr/bin/veil-host"), "/usr/bin/veil-host");
        assert_eq!(shell_quote("/opt/my dir/veil-host"), "'/opt/my dir/veil-host'");
        assert_eq!(shell_quote("/x;rm -rf ~"), "'/x;rm -rf ~'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
