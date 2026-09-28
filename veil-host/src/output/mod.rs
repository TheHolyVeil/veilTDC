//! Output abstraction: render RGBA frames to terminal or framebuffer.
//!
//! Two implementations:
//! - TerminalOutput: Kitty graphics protocol, halfblock, ASCII (current terminal rendering)
//! - DrmOutput: Direct framebuffer via DRM/KMS on bare TTY — one independent
//!   pipeline per connected display, see `monitor_count()`

pub mod drm;
pub mod terminal;

use crate::layout::Rect;
use std::io;

pub use drm::DrmOutput;
pub use terminal::TerminalOutput;

/// Trait for output backends.
///
/// Not `Send`: `DrmOutput` owns a libseat handle (a raw pointer) and lives on
/// the main thread for its whole lifetime. The frame loop never moves it.
pub trait OutputBackend {
    /// How many independent physical displays this backend is driving.
    /// `1` for everything except a multi-connector `DrmOutput` — default
    /// covers `TerminalOutput` (a terminal is inherently one viewport) with
    /// no override needed.
    fn monitor_count(&self) -> usize {
        1
    }

    /// Render an RGBA frame to one monitor. Blocks until complete or error.
    ///
    /// `monitor`: which display this frame is for, `0..monitor_count()`.
    /// Backends with only one display (`TerminalOutput`) ignore it.
    ///
    /// `damage`: bounding box of what changed since the last frame *on this
    /// monitor* (always present, may equal the full frame). Backends that
    /// can cheaply skip unchanged regions (DRM's dumb-buffer copy) should
    /// use it; backends that can't (terminal cell encoding, today) are free
    /// to ignore it and redraw everything, same as before this parameter
    /// existed.
    fn render_frame(
        &mut self,
        monitor: usize,
        rgba: &[u8],
        width: u32,
        height: u32,
        damage: Rect,
    ) -> io::Result<()>;

    /// Get one monitor's current output dimensions in pixels.
    fn get_size(&self, monitor: usize) -> (u32, u32);

    /// Called when VT is being switched away (suspend output, release resources).
    /// Only relevant for DrmOutput; TerminalOutput can no-op this. Whole-seat,
    /// not per-monitor — a VT switch suspends/resumes every display at once.
    fn on_vt_switch(&mut self, switch_in: bool) -> io::Result<()>;

    /// Called when the terminal is resized (new columns and rows). Allows the
    /// backend to update cached dimensions without querying the OS on every frame.
    /// Default: no-op (DrmOutput ignores terminal resize events).
    fn on_resize(&mut self, _cols: u16, _rows: u16) {}

    /// Whether any output device has a page flip in progress.
    fn has_flip_pending(&self) -> bool {
        false
    }

    /// Poll for output backend events (e.g. DRM VBLANK completion).
    /// Returns Ok(true) if an event was processed.
    fn poll_events(&mut self, _timeout: std::time::Duration) -> io::Result<bool> {
        Ok(false)
    }
}

#[derive(PartialEq)]
enum Force {
    Drm,
    Terminal,
}

/// Auto-detect best output backend for current environment.
///
/// `VEIL_OUTPUT=drm` forces DRM/KMS unconditionally — even nested under a
/// real compositor — and errors loudly if it can't init, instead of
/// silently falling back; `VEIL_OUTPUT=terminal` forces terminal the same
/// way. Both are an explicit one-off override and win over everything else.
///
/// `pref` is config.lua's `output` field. Unlike the env var, `pref ==
/// OutputPref::Drm` does NOT override the nested-compositor check below —
/// it only replaces the SSH-session check with an unconditional DRM/KMS
/// attempt on what still looks like a bare TTY. It's a persistent "trust
/// bare-TTY sessions to have real GPU hardware" setting, not a sledgehammer:
/// running `veil-host run` under Niri with `output = "drm"` in config still
fn is_bare_tty() -> bool {
    if std::env::var("TERM").map(|t| t == "linux").unwrap_or(false) {
        return true;
    }
    let tty_name = unsafe {
        let name = libc::ttyname(libc::STDIN_FILENO);
        if name.is_null() {
            None
        } else {
            std::ffi::CStr::from_ptr(name)
                .to_str()
                .ok()
                .map(|s| s.to_string())
        }
    };
    if let Some(name) = tty_name {
        if name.starts_with("/dev/tty") && !name.starts_with("/dev/pts/") {
            return true;
        }
    }
    false
}

/// Auto-detect best output backend for current environment.
///
/// `gpu_render` is config.lua's `gpu_render` flag, forwarded straight to
/// `TerminalOutput::new` — it has no effect on `DrmOutput`, which doesn't
/// use `veil-gpu` at all.
pub fn detect(
    pref: veil_config::OutputPref,
    gpu_render: bool,
) -> io::Result<Box<dyn OutputBackend>> {
    let force = match std::env::var("VEIL_OUTPUT").ok().as_deref() {
        Some("drm") | Some("kms") => Some(Force::Drm),
        Some("terminal") | Some("term") => Some(Force::Terminal),
        _ => None,
    };

    if force == Some(Force::Terminal) {
        eprintln!("[veil-host] VEIL_OUTPUT=terminal → terminal output");
        return Ok(Box::new(TerminalOutput::new(gpu_render)?));
    }

    if force == Some(Force::Drm) {
        eprintln!("[veil-host] VEIL_OUTPUT=drm → forcing DRM/KMS");
        return Ok(Box::new(DrmOutput::new()?));
    }

    let bare_tty = is_bare_tty();
    let has_dri = !drm::list_cards().is_empty();

    // On a bare TTY console with GPU hardware available, try DRM/KMS directly
    // even if stale WAYLAND_DISPLAY / DISPLAY / SSH_TTY env vars exist from a prior session.
    if bare_tty && has_dri && pref != veil_config::OutputPref::Terminal {
        eprintln!("[veil-host] bare TTY + GPU detected → attempting DRM/KMS");
        match DrmOutput::new() {
            Ok(drm) => {
                eprintln!("[veil-host] DRM/KMS initialized on bare metal framebuffer");
                return Ok(Box::new(drm));
            }
            Err(e) => {
                eprintln!("[veil-host] DRM/KMS failed on bare TTY: {e}");
            }
        }
    }

    // If running under a Wayland/X11 compositor, use terminal output.
    if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok() {
        eprintln!("[veil-host] detected compositor via env, using terminal output");
        return Ok(Box::new(TerminalOutput::new(gpu_render)?));
    }

    if pref == veil_config::OutputPref::Terminal {
        eprintln!("[veil-host] config.lua output=terminal → terminal output");
        return Ok(Box::new(TerminalOutput::new(gpu_render)?));
    }

    if pref == veil_config::OutputPref::Drm {
        eprintln!("[veil-host] config.lua output=drm → forcing DRM/KMS (bare TTY assumed)");
        return Ok(Box::new(DrmOutput::new()?));
    }

    // If SSH session (and not on a bare TTY console), use terminal output
    if std::env::var("SSH_CLIENT").is_ok() || std::env::var("SSH_TTY").is_ok() {
        eprintln!("[veil-host] detected SSH session, using terminal output");
        return Ok(Box::new(TerminalOutput::new(gpu_render)?));
    }

    // Fallback: try DRM/KMS
    match DrmOutput::new() {
        Ok(drm) => {
            eprintln!("[veil-host] DRM/KMS available, using bare-metal framebuffer output");
            Ok(Box::new(drm))
        }
        Err(e) => {
            eprintln!("[veil-host] DRM/KMS unavailable: {e}");
            if force == Some(Force::Drm) {
                return Err(e);
            }
            eprintln!("[veil-host] falling back to terminal output");
            Ok(Box::new(TerminalOutput::new(gpu_render)?))
        }
    }
}
