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
use std::path::PathBuf;

fn lock_path() -> Option<PathBuf> {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(|d| PathBuf::from(d).join("veil-host.lock"))
}

pub struct LockInfo {
    pub pid: i32,
    pub socket_name: String,
}

/// Read the lock file and check the PID inside it is still alive. `None` if
/// there's no lock file, it's malformed, OR it's stale (dead PID) — callers
/// should treat all three as "no default instance running."
pub fn read_live() -> Option<LockInfo> {
    let path = lock_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
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
    let info = LockInfo {
        pid: pid?,
        socket_name: socket_name?,
    };
    pid_alive(info.pid).then_some(info)
}

/// `kill(pid, 0)` checks existence/permission without sending a signal —
/// standard liveness probe.
fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn write(socket_name: &str) -> io::Result<()> {
    let path = lock_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR unset"))?;
    std::fs::write(
        &path,
        format!("pid={}\nsocket={socket_name}\n", std::process::id()),
    )
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
    if let Some(info) = read_live() {
        eprintln!(
            "[veil-host] already running (pid {}, socket {:?}) — use -O to start another instance without the lock file",
            info.pid, info.socket_name
        );
        std::process::exit(1);
    }
    if let Err(e) = write(socket_name) {
        eprintln!("[veil-host] warning: couldn't write lock file: {e}");
        return;
    }
    if let Some(path) = lock_path() {
        crate::vt::set_lock_path(&path.to_string_lossy());
    }
}
