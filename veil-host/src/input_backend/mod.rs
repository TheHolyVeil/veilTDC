//! Input abstraction — Combo 2.
//!
//! veil-host runs in two worlds:
//!   * under a window manager / over SSH, reading the host terminal's
//!     escape-sequence input via crossterm;
//!   * on a bare TTY, reading raw `/dev/input/event*` devices via evdev.
//!
//! Both produce [`InputCmd`]s pushed into the same host channel, so the
//! compositor side never knows which backend is driving it. A backend owns
//! its own blocking loop on a dedicated thread — call [`detect`] to pick one,
//! then [`InputBackend::run`] it with an [`InputCtx`].

use crate::input::InputCmd;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32};
use std::sync::mpsc::Sender;
use std::sync::Arc;

mod crossterm_input;
mod evdev_input;

pub use crossterm_input::CrosstermInput;
pub use evdev_input::EvdevInput;

/// Shared, mutable view geometry used to map pointer coordinates.
///
/// Terminal mode tracks both cell grid and pixel size (cells → pixels);
/// evdev mode only cares about the pixel extent for cursor clamping.
pub struct InputGeometry {
    pub cols: AtomicU16,
    pub rows: AtomicU16,
    pub comp_w: AtomicU32,
    pub comp_h: AtomicU32,
}

impl InputGeometry {
    pub fn new(cols: u16, rows: u16, comp_w: u32, comp_h: u32) -> Arc<Self> {
        Arc::new(Self {
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            comp_w: AtomicU32::new(comp_w),
            comp_h: AtomicU32::new(comp_h),
        })
    }
}

/// Everything a backend needs to run its loop.
pub struct InputCtx {
    /// Push translated events into the host compositor.
    pub tx: Sender<InputCmd>,
    /// Cleared to request CLI shutdown (ctrl-c, device gone).
    pub running: Arc<AtomicBool>,
    /// Mirror of `running` that also stops the compositor thread.
    pub host_stop: Arc<AtomicBool>,
    /// Live geometry for pointer mapping.
    pub geom: Arc<InputGeometry>,
}

/// A pluggable source of [`InputCmd`]s.
pub trait InputBackend: Send {
    /// Blocking event loop; returns when `ctx.running` is cleared. Consumes
    /// `self` so backends can move their device handles into the loop.
    fn run(self: Box<Self>, ctx: InputCtx);

    /// Human-readable backend name for logging.
    fn name(&self) -> &'static str;
}

/// Pick an input backend for the current environment.
///
/// Bare TTY (no `WAYLAND_DISPLAY`/`DISPLAY`, not SSH, and `/dev/input`
/// readable) → evdev. Anything else → crossterm. evdev construction can
/// fail (permissions, no devices); we fall back to crossterm in that case.
pub fn detect() -> Box<dyn InputBackend> {
    let ssh = std::env::var("SSH_CLIENT").is_ok() || std::env::var("SSH_TTY").is_ok();
    let wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
    let x11 = std::env::var("DISPLAY").is_ok();
    let bare_tty = !ssh && !wayland && !x11;

    if bare_tty {
        match EvdevInput::open() {
            Ok(ev) => {
                eprintln!("[veil-host] input: evdev (bare TTY)");
                return Box::new(ev);
            }
            Err(e) => {
                // On a bare TTY crossterm reads stdin, which gets nothing once
                // we're in graphics mode — so this fallback means NO input.
                // Make the cause and fix loud rather than silently dead.
                eprintln!("[veil-host] !! input: evdev unavailable: {e}");
                eprintln!("[veil-host] !! keyboard/mouse will NOT work on this TTY.");
                eprintln!("[veil-host] !! fix: add yourself to the 'input' group:");
                eprintln!("[veil-host] !!      sudo usermod -aG input $USER   (then re-login)");
            }
        }
    }

    eprintln!("[veil-host] input: crossterm (terminal)");
    Box::new(CrosstermInput::new())
}

/// Compositor pixel extent from the controlling terminal, via TIOCGWINSZ.
pub(crate) fn term_pixel_size() -> Option<(u32, u32)> {
    let mut winsz: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut winsz) };
    if ret == 0 && winsz.ws_xpixel > 0 && winsz.ws_ypixel > 0 {
        Some((winsz.ws_xpixel as u32, winsz.ws_ypixel as u32))
    } else {
        None
    }
}
