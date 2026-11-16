//! `wlr-screencopy-unstable-v1`: lets `grim`, `wf-recorder`, OBS and the
//! wlr portal copy an output (or a region of one) into a client shm buffer.
//!
//! Source of pixels: the last frame `ship_frame` handed to the render
//! thread (`Monitor::prev_frame`, a full-size RGBA `Arc<Vec<u8>>`), so a
//! capture never re-renders and never touches the compositor's work buffer.
//!
//! Scope, on purpose:
//! - shm buffers only, format `Xrgb8888` (a mandatory `wl_shm` format, so no
//!   extra formats have to be advertised to every client). The compositor's
//!   RGBA is swizzled into it. No `linux_dmabuf` event is sent.
//! - `copy` completes at once from the last frame, so a screenshot of a
//!   static screen works. If no frame exists yet it waits for the first.
//! - `copy_with_damage` completes on the next frame shipped for that
//!   monitor and reports the whole region as damaged. A repaint is forced
//!   so a recorder is not stalled by an idle screen; that costs a full
//!   composite per captured frame.
//! - `overlay_cursor` is ignored: the cursor is drawn by the render side,
//!   not part of the shipped frame.
//!
//! Not gated yet: any client that can reach the socket may bind this global.
//! Privileged globals need a trust policy before sandboxed clients exist.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use smithay::output::Output;
use smithay::wayland::shm::{with_buffer_contents, with_buffer_contents_mut};
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
    protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::server::State;

/// Protocol version implemented (3 adds `buffer_done`; we send no dmabuf).
const VERSION: u32 = 3;
const FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;

/// A capture rectangle in output pixels, already clamped to the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Clamp a requested rectangle to an `out_w`×`out_h` output. `None` when it
/// is empty or lies entirely outside.
pub fn clamp_region(x: i32, y: i32, w: i32, h: i32, out_w: u32, out_h: u32) -> Option<Region> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let x0 = i64::from(x).max(0);
    let y0 = i64::from(y).max(0);
    let x1 = (i64::from(x) + i64::from(w)).min(i64::from(out_w));
    let y1 = (i64::from(y) + i64::from(h)).min(i64::from(out_h));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Region { x: x0 as u32, y: y0 as u32, w: (x1 - x0) as u32, h: (y1 - y0) as u32 })
}

/// Copy `r` out of the RGBA frame `src` (`sw`×`sh`) into `dst`, rows
/// `stride` bytes apart, as `Xrgb8888` (memory order B, G, R, X).
/// `false` if anything would be out of bounds.
pub fn copy_region_xrgb(src: &[u8], sw: u32, sh: u32, r: &Region, dst: &mut [u8], stride: usize) -> bool {
    let (sw_, rw, rh) = (sw as usize, r.w as usize, r.h as usize);
    if r.x + r.w > sw || r.y + r.h > sh || src.len() < sw_ * sh as usize * 4 || rw == 0 || rh == 0 {
        return false;
    }
    if stride < rw * 4 || dst.len() < stride * (rh - 1) + rw * 4 {
        return false;
    }
    for row in 0..rh {
        let s = ((r.y as usize + row) * sw_ + r.x as usize) * 4;
        let d = row * stride;
        let (dpx, _) = dst[d..d + rw * 4].as_chunks_mut::<4>();
        let (spx, _) = src[s..s + rw * 4].as_chunks::<4>();
        for (o, i) in dpx.iter_mut().zip(spx) {
            o[0] = i[2];
            o[1] = i[1];
            o[2] = i[0];
            o[3] = 255;
        }
    }
    true
}

/// User data of a frame object.
pub struct FrameData {
    monitor: usize,
    /// `None`: the request was invalid, the frame is answered with `failed`.
    region: Option<Region>,
    used: AtomicBool,
}

struct Pending {
    frame: ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
    monitor: usize,
    region: Region,
    with_damage: bool,
}

pub struct Screencopy {
    pending: Vec<Pending>,
}

impl Screencopy {
    pub fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<State, ZwlrScreencopyManagerV1, ()>(VERSION, ());
        Self { pending: Vec::new() }
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

fn send_ready(frame: &ZwlrScreencopyFrameV1, with_damage: bool, r: &Region) {
    if with_damage {
        frame.damage(0, 0, r.w, r.h);
    }
    frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
    // Wall-clock stamp: grim and recorders ignore it, and the shipped frame
    // carries no presentation time to report.
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    frame.ready((secs >> 32) as u32, secs as u32, d.subsec_nanos());
}

/// Write `p`'s region of `src` into its buffer, then answer `ready` or `failed`.
fn complete(p: &Pending, src: &[u8], sw: u32, sh: u32) {
    let ok = with_buffer_contents_mut(&p.buffer, |ptr, len, d| {
        let off = d.offset.max(0) as usize;
        if off >= len {
            return false;
        }
        // SAFETY: `ptr..ptr + len` is the mapped shm pool that smithay hands
        // us for exactly this call; `off < len` was just checked.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr.add(off), len - off) };
        copy_region_xrgb(src, sw, sh, &p.region, dst, d.stride.max(0) as usize)
    })
    .unwrap_or(false);
    if ok {
        send_ready(&p.frame, p.with_damage, &p.region);
    } else {
        tracing::debug!("screencopy: could not fill the client buffer");
        p.frame.failed();
    }
}

/// Called by `ship_frame` with each finished frame: completes the
/// `copy_with_damage` (and not-yet-served `copy`) requests waiting on it.
pub fn on_frame_shipped(state: &mut State, idx: usize, frame: &Arc<Vec<u8>>, w: u32, h: u32) {
    let waiting = std::mem::take(&mut state.screencopy.pending);
    for p in waiting {
        if !p.frame.is_alive() {
            continue;
        }
        if p.monitor == idx {
            complete(&p, frame, w, h);
        } else {
            state.screencopy.pending.push(p);
        }
    }
}

fn monitor_for(state: &State, output: &WlOutput) -> Option<usize> {
    let o = Output::from_resource(output)?;
    state.layer_outputs.iter().position(|x| *x == o)
}

/// Create the frame object and announce the buffer the client must supply.
fn start_frame(
    state: &State,
    data_init: &mut DataInit<'_, State>,
    id: New<ZwlrScreencopyFrameV1>,
    output: &WlOutput,
    rect: Option<(i32, i32, i32, i32)>,
) {
    let mon = monitor_for(state, output).filter(|i| *i < state.monitors.len());
    let region = mon.and_then(|i| {
        let m = &state.monitors[i];
        match rect {
            None => clamp_region(0, 0, m.output_w as i32, m.output_h as i32, m.output_w, m.output_h),
            Some((x, y, w, h)) => clamp_region(x, y, w, h, m.output_w, m.output_h),
        }
    });
    let frame = data_init.init(
        id,
        FrameData { monitor: mon.unwrap_or(0), region, used: AtomicBool::new(false) },
    );
    match region {
        Some(r) => {
            frame.buffer(FORMAT, r.w, r.h, r.w * 4);
            if frame.version() >= 3 {
                frame.buffer_done();
            }
        }
        None => frame.failed(),
    }
}

fn handle_copy(
    state: &mut State,
    frame: &ZwlrScreencopyFrameV1,
    data: &FrameData,
    buffer: WlBuffer,
    with_damage: bool,
) {
    if data.used.swap(true, Ordering::Relaxed) {
        frame.post_error(zwlr_screencopy_frame_v1::Error::AlreadyUsed, "the frame was already copied");
        return;
    }
    let Some(region) = data.region else {
        return; // already answered with `failed` in `start_frame`
    };
    // The client must supply exactly the buffer announced in `start_frame`.
    let fits = with_buffer_contents(&buffer, |_, _, d| {
        d.format == FORMAT
            && d.width as u32 == region.w
            && d.height as u32 == region.h
            && d.stride >= (region.w * 4) as i32
            && d.offset >= 0
    })
    .unwrap_or(false);
    if !fits {
        frame.post_error(
            zwlr_screencopy_frame_v1::Error::InvalidBuffer,
            "buffer must be a shm Xrgb8888 buffer of the announced size and stride",
        );
        return;
    }
    let p = Pending { frame: frame.clone(), buffer, monitor: data.monitor, region, with_damage };
    let last = if with_damage { None } else { state.monitors.get(p.monitor) };
    if let Some(m) = last {
        if let Some(src) = &m.prev_frame {
            complete(&p, src, m.output_w, m.output_h);
            return;
        }
    }
    // Wait for the next shipped frame, and make sure one is coming.
    state.screencopy.pending.push(p);
    crate::server::mark_dirty_full(state);
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, (), State> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, (), State> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput { frame, output, .. } => {
                start_frame(state, data_init, frame, &output, None);
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame, output, x, y, width, height, ..
            } => {
                start_frame(state, data_init, frame, &output, Some((x, y, width, height)));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData, State> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                handle_copy(state, frame, data, buffer, false);
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                handle_copy(state, frame, data, buffer, true);
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {
                state.screencopy.pending.retain(|p| &p.frame != frame);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_inside_and_trims_overhang() {
        assert_eq!(clamp_region(10, 20, 30, 40, 100, 100), Some(Region { x: 10, y: 20, w: 30, h: 40 }));
        assert_eq!(clamp_region(-5, -5, 20, 20, 100, 100), Some(Region { x: 0, y: 0, w: 15, h: 15 }));
        assert_eq!(clamp_region(90, 90, 50, 50, 100, 100), Some(Region { x: 90, y: 90, w: 10, h: 10 }));
    }

    #[test]
    fn clamp_rejects_empty_and_outside() {
        assert_eq!(clamp_region(0, 0, 0, 10, 100, 100), None);
        assert_eq!(clamp_region(0, 0, 10, -1, 100, 100), None);
        assert_eq!(clamp_region(100, 0, 10, 10, 100, 100), None);
        assert_eq!(clamp_region(-20, 0, 10, 10, 100, 100), None);
        // No overflow on silly protocol input.
        assert_eq!(clamp_region(i32::MAX, 0, i32::MAX, 10, 100, 100), None);
    }

    #[test]
    fn copy_swizzles_rgba_to_xrgb_and_respects_stride() {
        // 2x2 frame: R,G,B,A per pixel.
        let src = [
            1, 2, 3, 9, 4, 5, 6, 9, //
            7, 8, 10, 9, 11, 12, 13, 9,
        ];
        let r = Region { x: 1, y: 0, w: 1, h: 2 };
        let mut dst = [0xEEu8; 16];
        assert!(copy_region_xrgb(&src, 2, 2, &r, &mut dst, 8));
        assert_eq!(&dst[0..4], &[6, 5, 4, 255]); // pixel (1,0): B,G,R,X
        assert_eq!(&dst[4..8], &[0xEE; 4]); // stride padding untouched
        assert_eq!(&dst[8..12], &[13, 12, 11, 255]); // pixel (1,1)
    }

    #[test]
    fn copy_refuses_out_of_bounds_and_short_buffers() {
        let src = [0u8; 16];
        let mut dst = [0u8; 16];
        let far = Region { x: 1, y: 1, w: 2, h: 1 };
        assert!(!copy_region_xrgb(&src, 2, 2, &far, &mut dst, 8));
        let r = Region { x: 0, y: 0, w: 2, h: 2 };
        assert!(!copy_region_xrgb(&src, 2, 2, &r, &mut dst[..11], 8)); // dst too short
        assert!(!copy_region_xrgb(&src, 2, 2, &r, &mut dst, 4)); // stride < row
        assert!(!copy_region_xrgb(&src[..8], 2, 2, &r, &mut dst, 8)); // src too short
    }
}
