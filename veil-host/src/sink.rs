//! Frame output from the host compositor.
//!
//! Sink-agnostic: caller decides what to do with the RGBA. veil-cli
//! encodes to kitty graphics today; the same Frame stream feeds
//! /dev/fb0, sixel, iterm2, DRM dumb buffers tomorrow.

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
    pub rgba:   Arc<Vec<u8>>,
    pub width:  u32,
    pub height: u32,
    /// Monotonic frame counter from the compositor. Useful for skip detection.
    pub serial: u64,
}
