//! Input commands pushed into the host compositor.
//!
//! Mirrors `veil-compositor::InputCmd` shape so the CLI can swap
//! WaylandInput / UInputHandle / Host transparently. Kept independent
//! to avoid a cross-crate dep cycle.

#[derive(Clone, Debug)]
pub enum InputCmd {
    /// Raw evdev keycode + active modifier bitmask + press/release.
    Key { keycode: u32, mods: u32, pressed: bool },

    /// Absolute pointer move in logical-output pixels.
    PointerMotionAbs { x: i32, y: i32, width: u32, height: u32 },

    /// Pointer button: evdev BTN_* code + press/release.
    PointerButton { button: u32, pressed: bool },

    /// Vertical scroll in discrete notches (+ = down).
    Scroll { v120: i32 },

    /// Terminal window resized — new compositor output dimensions in pixels.
    Resize { width: u32, height: u32 },

    /// Real monitor layout from output detection (Phase 1's `monitor_count()`
    /// / `get_size()`), sent once shortly after startup once the output
    /// backend is known. Rebuilds `State.monitors` from scratch — side by
    /// side, left to right, in the given order (the "dumb" arrangement, see
    /// MULTI_MONITOR_SCOPE.md). `sizes.len() == 1` for terminal mode or a
    /// single DRM display; unremarkable in that case, same effect as the
    /// old single-output `Resize`-based retarget it replaces.
    SetMonitors { sizes: Vec<(u32, u32)> },

    /// Display an On-Screen Display (OSD) popup overlay.
    Osd { title: String, body: String, progress: Option<u8> },
}

// TODO: routing layer that takes an InputCmd and calls into
//   smithay::input::Seat::keyboard()/pointer(). Modifier diffing
//   already proven in veil-compositor::uinput_input::dispatch —
//   port that logic here once the seat exists.
