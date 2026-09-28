//! Virtual-terminal takeover for bare-TTY DRM mode.
//!
//! When we mode-set a CRTC on a bare TTY we MUST take the VT out of text mode
//! first (`KD_GRAPHICS`). Otherwise the kernel framebuffer console (fbcon) is
//! still bound to the same CRTC and fights our page-flips — on amdgpu this
//! reliably ends in a GPU ring timeout → reset → hard lock / panic.
//!
//! [`VtGuard`] flips the active VT to graphics mode and restores text mode on
//! drop. Because a crash that skips `Drop` would leave the console black and
//! unusable, we also install a panic hook and fatal-signal handlers that
//! restore text mode from a stashed fd — a last-resort so a bug never leaves
//! the machine wedged.
//!
//! The same last-resort path also unlinks the Wayland socket
//! (`$XDG_RUNTIME_DIR/wayland-veil-0`) and `crate::lockfile`'s
//! `veil-host.lock`, so veil never leaves either behind no matter how it
//! exits — clean shutdown, HOME, Ctrl-C, or a crash. [`install_handlers`]
//! must run unconditionally (both output modes), not just from
//! [`VtGuard::acquire`] — a terminal-mode (crossterm) session has no VT to
//! take over but still has a socket + lock file to clean up.

use std::ffi::CString;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Once, OnceLock};

// linux/kd.h
const KDSETMODE: libc::c_ulong = 0x4B3A;
const KD_TEXT: libc::c_int = 0x00;
const KD_GRAPHICS: libc::c_int = 0x01;

/// Raw fd of the controlling VT, published for the emergency restore path.
/// -1 means "not in graphics mode / nothing to restore".
static VT_FD: AtomicI32 = AtomicI32::new(-1);

/// Absolute Wayland socket path, set once at startup via [`set_socket_path`]
/// so [`emergency_restore`] can unlink it from a signal handler (no
/// allocation there — `CString` is prebuilt, `unlink` is a raw syscall).
static SOCKET_PATH: OnceLock<CString> = OnceLock::new();

/// Record the bound Wayland socket's absolute path. Call once, early in
/// `main`, before anything that could crash — first call wins.
pub fn set_socket_path(absolute_path: &str) {
    if let Ok(c) = CString::new(absolute_path) {
        let _ = SOCKET_PATH.set(c);
    }
}

/// Same idea as [`SOCKET_PATH`], for `crate::lockfile`'s
/// `$XDG_RUNTIME_DIR/veil-host.lock` — set by `lockfile::acquire_or_exit`,
/// never set at all for a `-O` (unregistered) run.
static LOCK_PATH: OnceLock<CString> = OnceLock::new();

pub fn set_lock_path(absolute_path: &str) {
    if let Ok(c) = CString::new(absolute_path) {
        let _ = LOCK_PATH.set(c);
    }
}

pub struct VtGuard {
    fd: OwnedFd,
}

impl VtGuard {
    /// Put the controlling terminal into graphics mode. Fails on a pty
    /// (e.g. SSH) where `KDSETMODE` returns ENOTTY — caller should treat that
    /// as "not a real VT" and avoid DRM.
    pub fn acquire() -> io::Result<Self> {
        let raw = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        if unsafe { libc::ioctl(raw, KDSETMODE, KD_GRAPHICS) } < 0 {
            return Err(io::Error::last_os_error());
        }

        VT_FD.store(raw, Ordering::SeqCst);
        install_handlers();
        eprintln!("[veil-host] VT → graphics mode (fbcon suspended)");
        Ok(VtGuard { fd })
    }
}

impl Drop for VtGuard {
    fn drop(&mut self) {
        let raw = self.fd.as_raw_fd();
        unsafe {
            libc::ioctl(raw, KDSETMODE, KD_TEXT);
            // Discard anything typed while we owned the screen so it can't run
            // in the shell once text mode returns.
            libc::tcflush(raw, libc::TCIFLUSH);
        }
        VT_FD.store(-1, Ordering::SeqCst);
        eprintln!("[veil-host] VT → text mode");
    }
}

/// Restore the VT to text mode and unlink the Wayland socket, from anywhere
/// (signal handler, panic hook, explicit shutdown). Idempotent and
/// async-signal-safe enough for our purposes (an ioctl + tcflush on a
/// stashed fd, plus an unlink on a preallocated path — no locks, no
/// allocation). Every intentional shutdown path (HOME, Ctrl-C/SIGTERM)
/// already calls this directly; [`install_handlers`] wires it into crashes
/// too, so this is the single place "veil is going away" funnels through.
pub fn emergency_restore() {
    let raw = VT_FD.load(Ordering::SeqCst);
    if raw >= 0 {
        unsafe {
            libc::ioctl(raw, KDSETMODE, KD_TEXT);
            libc::tcflush(raw, libc::TCIFLUSH);
        }
    }
    if let Some(path) = SOCKET_PATH.get() {
        unsafe {
            libc::unlink(path.as_ptr());
        }
    }
    if let Some(path) = LOCK_PATH.get() {
        unsafe {
            libc::unlink(path.as_ptr());
        }
    }
}

/// Install the panic hook + fatal-signal handlers. Call unconditionally at
/// startup (both terminal and DRM output modes) — NOT just from
/// `VtGuard::acquire`, which only runs in DRM mode but the socket still
/// needs cleaning up in terminal mode too. Idempotent (`Once`).
pub fn install_handlers() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Panic: restore the console, then run the normal hook so the backtrace
        // still prints (into the log).
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            emergency_restore();
            prev(info);
        }));

        // Hard crashes that bypass Drop: restore, reset to default disposition,
        // re-raise so we still get the real fault/coredump.
        for sig in [
            libc::SIGSEGV,
            libc::SIGABRT,
            libc::SIGBUS,
            libc::SIGILL,
            libc::SIGFPE,
        ] {
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = fatal_handler as *const () as usize;
                libc::sigemptyset(&mut sa.sa_mask);
                sa.sa_flags = libc::SA_RESETHAND;
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        }
    });
}

extern "C" fn fatal_handler(sig: libc::c_int) {
    emergency_restore();
    // SA_RESETHAND restored the default handler; re-raise to get the real crash.
    unsafe {
        libc::raise(sig);
    }
}
