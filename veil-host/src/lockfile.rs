//! Single "default instance" lock file at `$XDG_RUNTIME_DIR/veil-host.lock`.
//!
//! Distinct from `wayland-server`'s own per-socket `.lock` flock (that one's
//! a bind-time mutex tied to a specific socket path, not ours to repurpose).
//! This one exists so a plain `veil-host run <cmd>` refuses to start a second
//! time by accident, and so `veil-host run -a <cmd>` (no `-s`) can find
//! whatever's currently running without the caller having to know its socket
//! name. `-O` opts an instance out of both — it's deliberately unregistered,
//! so it won't be found by `-a` auto-discovery either; pass `-s` explicitly
//! to reach it (and to `run` it in the first place, since it can't bind the
//! same socket the registered instance is already holding).

use std::io;
use std::path::{Path, PathBuf};

fn lock_path() -> Option<PathBuf> {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(|d| PathBuf::from(d).join("veil-host.lock"))
}

pub struct LockInfo {
    pub pid: i32,
    pub socket_name: String,
}

fn parse(content: &str) -> Option<LockInfo> {
    let mut pid = None;
    let mut socket_name = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("pid=") {
            pid = v.trim().parse::<i32>().ok();
        }
        if let Some(v) = line.strip_prefix("socket=") {
            socket_name = Some(v.trim().to_string());
        }
    }
    Some(LockInfo {
        pid: pid?,
        socket_name: socket_name?,
    })
}

/// Read the lock file and check the PID inside it is still alive. `None` if
/// there's no lock file, it's malformed, OR it's stale (dead PID) — callers
/// should treat all three as "no default instance running."
pub fn read_live() -> Option<LockInfo> {
    let info = parse(&std::fs::read_to_string(lock_path()?).ok()?)?;
    pid_alive(info.pid).then_some(info)
}

/// Liveness probe. `kill(pid, 0)` says the pid exists (EPERM still means it
/// does); then, where /proc is readable, the process must actually be a
/// `veil-host`, so a stale lock whose pid was recycled by an unrelated process
/// no longer blocks startup forever.
fn pid_alive(pid: i32) -> bool {
    let exists = unsafe { libc::kill(pid, 0) } == 0
        || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !exists {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(comm) => comm.trim() == "veil-host",
        Err(_) => true,
    }
}

enum Acquire {
    Won,
    Held(LockInfo),
    Unavailable(io::Error),
}

/// Atomically claim `path`. The content is written to a private temp file and
/// then `hard_link`ed into place, which fails with EEXIST if someone else got
/// there first and never exposes a half-written lock. Two simultaneous starts
/// can't both win (the old check-then-write could). A stale or malformed
/// existing lock is removed and the claim retried.
fn acquire_at(path: &Path, socket_name: &str, is_live: &dyn Fn(i32) -> bool) -> Acquire {
    let tmp = path.with_extension(format!("lock.{}", std::process::id()));
    if let Err(e) = std::fs::write(
        &tmp,
        format!("pid={}\nsocket={socket_name}\n", std::process::id()),
    ) {
        return Acquire::Unavailable(e);
    }
    let mut result = Acquire::Unavailable(io::Error::other("lock file kept reappearing"));
    for _ in 0..3 {
        match std::fs::hard_link(&tmp, path) {
            Ok(()) => {
                result = Acquire::Won;
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let held = std::fs::read_to_string(path).ok().and_then(|c| parse(&c));
                if let Some(info) = held {
                    if is_live(info.pid) {
                        result = Acquire::Held(info);
                        break;
                    }
                }
                let _ = std::fs::remove_file(path); // stale or malformed
            }
            Err(e) => {
                result = Acquire::Unavailable(e);
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Enforce the single-default-instance rule and write our lock file.
/// No-op (and no lock file written) if `override_lock` (`-O`) is set — exits
/// the process if a live default instance is already running and this run
/// didn't ask to bypass that.
pub fn acquire_or_exit(socket_name: &str, override_lock: bool) {
    if override_lock {
        eprintln!("[veil-host] -O: running unregistered — `-a` auto-discovery won't find this instance, use -s explicitly");
        return;
    }
    let Some(path) = lock_path() else {
        eprintln!("[veil-host] warning: couldn't write lock file: XDG_RUNTIME_DIR unset");
        return;
    };
    match acquire_at(&path, socket_name, &pid_alive) {
        Acquire::Won => crate::vt::set_lock_path(&path.to_string_lossy()),
        Acquire::Held(info) => {
            eprintln!(
                "[veil-host] already running (pid {}, socket {:?}) — use -O to start another instance without the lock file",
                info.pid, info.socket_name
            );
            std::process::exit(1);
        }
        Acquire::Unavailable(e) => eprintln!("[veil-host] warning: couldn't write lock file: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("veil-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("veil-host.lock")
    }

    #[test]
    fn parse_requires_both_fields() {
        let i = parse("pid=42\nsocket=wayland-veil-0\n").unwrap();
        assert_eq!((i.pid, i.socket_name.as_str()), (42, "wayland-veil-0"));
        assert!(parse("pid=42\n").is_none());
        assert!(parse("socket=x\n").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn first_claim_wins_second_sees_it_held() {
        let p = scratch("held");
        assert!(matches!(acquire_at(&p, "a", &|_| true), Acquire::Won));
        match acquire_at(&p, "b", &|_| true) {
            Acquire::Held(i) => assert_eq!(
                (i.pid, i.socket_name.as_str()),
                (std::process::id() as i32, "a")
            ),
            _ => panic!("second claim must lose"),
        }
        // the winner's content is intact and no temp file is left behind
        assert!(std::fs::read_to_string(&p).unwrap().contains("socket=a"));
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
        std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    #[test]
    fn stale_or_malformed_lock_is_replaced() {
        let p = scratch("stale");
        std::fs::write(&p, "pid=999999\nsocket=old\n").unwrap();
        assert!(matches!(acquire_at(&p, "new", &|_| false), Acquire::Won));
        assert!(std::fs::read_to_string(&p).unwrap().contains("socket=new"));
        std::fs::write(&p, "garbage").unwrap();
        assert!(matches!(acquire_at(&p, "newer", &|_| true), Acquire::Won));
        assert!(std::fs::read_to_string(&p).unwrap().contains("socket=newer"));
        std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    #[test]
    fn dead_pid_is_not_alive() {
        assert!(!pid_alive(i32::MAX));
    }
}
