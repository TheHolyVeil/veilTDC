//! Output abstraction: render RGBA frames to terminal or framebuffer.
//!
//! Two implementations:
//! - TerminalOutput: Kitty graphics protocol, halfblock, ASCII (current terminal rendering)
//! - DrmOutput: Direct framebuffer via DRM/KMS on bare TTY

pub mod terminal;
pub mod drm;

use std::io;
use crate::layout::Rect;

pub use terminal::TerminalOutput;
pub use drm::DrmOutput;

/// Trait for output backends.
///
/// Not `Send`: `DrmOutput` owns a libseat handle (a raw pointer) and lives on
/// the main thread for its whole lifetime. The frame loop never moves it.
pub trait OutputBackend {
    /// Render an RGBA frame. Blocks until complete or error.
    ///
    /// `damage`: bounding box of what changed since the last frame (always
    /// present, may equal the full frame). Backends that can cheaply skip
    /// unchanged regions (DRM's dumb-buffer copy) should use it; backends
    /// that can't (terminal cell encoding, today) are free to ignore it and
    /// redraw everything, same as before this parameter existed.
    fn render_frame(&mut self, rgba: &[u8], width: u32, height: u32, damage: Rect) -> io::Result<()>;

    /// Get current output dimensions in pixels.
    fn get_size(&self) -> (u32, u32);

    /// Called when VT is being switched away (suspend output, release resources).
    /// Only relevant for DrmOutput; TerminalOutput can no-op this.
    fn on_vt_switch(&mut self, switch_in: bool) -> io::Result<()>;

    /// Called when the terminal is resized (new columns and rows). Allows the
    /// backend to update cached dimensions without querying the OS on every frame.
    /// Default: no-op (DrmOutput ignores terminal resize events).
    fn on_resize(&mut self, _cols: u16, _rows: u16) {}
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
            std::ffi::CStr::from_ptr(name).to_str().ok().map(|s| s.to_string())
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
pub fn detect(pref: veil_config::OutputPref) -> io::Result<Box<dyn OutputBackend>> {
    let force = match std::env::var("VEIL_OUTPUT").ok().as_deref() {
        Some("drm") | Some("kms")  => Some(Force::Drm),
        Some("terminal") | Some("term") => Some(Force::Terminal),
        _ => None,
    };

    if force == Some(Force::Terminal) {
        eprintln!("[veil-host] VEIL_OUTPUT=terminal → terminal output");
        return Ok(Box::new(TerminalOutput::new()?));
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
        return Ok(Box::new(TerminalOutput::new()?));
    }

    if pref == veil_config::OutputPref::Terminal {
        eprintln!("[veil-host] config.lua output=terminal → terminal output");
        return Ok(Box::new(TerminalOutput::new()?));
    }

    if pref == veil_config::OutputPref::Drm {
        eprintln!("[veil-host] config.lua output=drm → forcing DRM/KMS (bare TTY assumed)");
        return Ok(Box::new(DrmOutput::new()?));
    }

    // If SSH session (and not on a bare TTY console), use terminal output
    if std::env::var("SSH_CLIENT").is_ok() || std::env::var("SSH_TTY").is_ok() {
        eprintln!("[veil-host] detected SSH session, using terminal output");
        return Ok(Box::new(TerminalOutput::new()?));
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
            Ok(Box::new(TerminalOutput::new()?))
        }
    }
}
