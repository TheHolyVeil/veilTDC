//! Frame output from the host compositor.
//!
//! Sink-agnostic: caller decides what to do with the RGBA. veil-cli
//! encodes to kitty graphics today; the same Frame stream feeds
//! /dev/fb0, sixel, iterm2, DRM dumb buffers tomorrow.

use crate::layout::Rect;
use std::sync::Arc;

/// One rendered frame of the hosted scene.
///
/// `rgba` is an `Arc`-wrapped buffer — sending a frame to the render thread
/// is a pointer copy, not a full ~8 MB memcpy. The render thread holds the
/// Arc while it encodes; the compositor meanwhile reuses its own
/// `composite_buf` for the next frame independently.
///
/// Buffer is tightly packed `width * height * 4` bytes, R-G-B-A order.
#[derive(Clone)]
pub struct Frame {
    pub rgba: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    /// Which monitor this frame is for — index into whatever the output
    /// backend's `monitor_count()` reports. `0` for terminal mode (always
    /// exactly one) and for the common single-DRM-display case.
    pub output_id: usize,
    /// Monotonic frame counter from the compositor. Useful for skip detection.
    pub serial: u64,
    /// Bounding box of what actually changed since the last frame — always
    /// present and always `rgba`-full-size-or-smaller, never a precise
    /// region list. Composite itself still fully recomputes `rgba` every
    /// tick (correctness first); this is purely so an output backend that
    /// *can* skip unchanged rows (DRM's dumb-buffer copy) does. Backends
    /// that can't easily do partial redraw (terminal cell encoding) are
    /// free to ignore it and redraw everything, same as before this existed.
    pub damage: Rect,
}
