//! Mirror the greeter onto every other connected monitor.
//!
//! Slint's linuxkms backend drives one connector and keeps its DRM fd to
//! itself, so we get at the same device the legitimate way round: the backend
//! opens /dev/dri/cardN in THIS process, and `fcntl(F_DUPFD_CLOEXEC)` on that
//! fd shares its open file description — and therefore its DRM master. With
//! that we can modeset the remaining connectors on spare CRTCs, each at its
//! OWN best mode (a 720p TV doesn't drag the main monitor down to 720p).
//!
//! Frames come from `Window::take_snapshot()` on a timer, scaled on the CPU
//! (bilinear, letterboxed) into one dumb buffer per extra monitor. The
//! greeter is a login form, so ~12 fps is plenty.
//!
//! Everything here is best-effort: any failure only means that one extra
//! monitor stays dark. The primary display is never touched. `shutdown()`
//! must run before the Slint window is dropped so the incoming session can
//! take DRM master cleanly.

use crate::display::{best_mode, mode_infos, Card};
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{
    connector, crtc, dumbbuffer::DumbBuffer, framebuffer, Device as ControlDevice, Mode,
};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::cell::{Cell, RefCell};
use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(80);
/// Re-issue set_crtc this often (ticks): after a VT round-trip the kernel
/// has dropped our modes, and the greeter has no other signal for it.
const REASSERT_EVERY: u32 = 40;
/// Look for newly powered-on monitors this often while we have none.
const RESCAN_EVERY: u32 = 25;

struct Secondary {
    name: String,
    conn: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    db: DumbBuffer,
    fb: framebuffer::Handle,
    w: u32,
    h: u32,
}

struct Mirror {
    card: Card,
    primary: String,
    outs: Vec<Secondary>,
    tick: u32,
}

fn conn_name(c: &connector::Info) -> String {
    format!("{}-{}", c.interface().as_str(), c.interface_id())
}

/// Dup the DRM fd Slint opened for `primary` (same open file => same master).
fn find_card(primary: &str) -> Option<Card> {
    for e in std::fs::read_dir("/proc/self/fd").ok()?.flatten() {
        let Ok(fd) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Ok(target) = std::fs::read_link(e.path()) else { continue };
        if !target.to_string_lossy().starts_with("/dev/dri/card") {
            continue;
        }
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            continue;
        }
        let card = Card(File::from(unsafe { OwnedFd::from_raw_fd(dup) }));
        let Ok(res) = card.resource_handles() else { continue };
        let owns_primary = res.connectors().iter().any(|h| {
            card.get_connector(*h, false)
                .is_ok_and(|c| conn_name(&c) == primary && c.state() == connector::State::Connected)
        });
        if owns_primary {
            return Some(card);
        }
    }
    None
}

impl Mirror {
    fn start(primary: &str) -> Option<Self> {
        let card = find_card(primary)?;
        let mut m = Mirror { card, primary: primary.to_string(), outs: Vec::new(), tick: 0 };
        // None until Slint has actually modeset its own CRTC (else we could
        // grab that CRTC for ourselves).
        m.outs = m.setup()?;
        Some(m)
    }

    fn setup(&self) -> Option<Vec<Secondary>> {
        let card = &self.card;
        let res = card.resource_handles().ok()?;
        let mut used: Vec<crtc::Handle> = Vec::new();
        let mut primary_crtc = false;
        for h in res.connectors() {
            let Ok(con) = card.get_connector(*h, false) else { continue };
            if conn_name(&con) != self.primary {
                continue;
            }
            if let Some(c) = con
                .current_encoder()
                .and_then(|e| card.get_encoder(e).ok())
                .and_then(|i| i.crtc())
            {
                used.push(c);
                primary_crtc = true;
            }
        }
        if !primary_crtc {
            return None; // primary not lit yet — retry next tick
        }

        let mut outs = Vec::new();
        for h in res.connectors() {
            let Ok(con) = card.get_connector(*h, false) else { continue };
            let name = conn_name(&con);
            if name == self.primary || con.state() != connector::State::Connected {
                continue;
            }
            let infos = mode_infos(con.modes());
            let Some((idx, info)) = best_mode(&infos) else { continue };
            let (w, h2) = (info.w, info.h);
            let mode = con.modes()[idx];
            let crtc = con
                .encoders()
                .iter()
                .filter_map(|e| card.get_encoder(*e).ok())
                .flat_map(|i| res.filter_crtcs(i.possible_crtcs()))
                .find(|c| !used.contains(c));
            let Some(crtc) = crtc else {
                eprintln!("[velogin] mirror: no free CRTC for {name}");
                continue;
            };
            match Self::make(card, con.handle(), crtc, mode, w, h2, name.clone()) {
                Ok(s) => {
                    eprintln!(
                        "[velogin] mirror: {name} {w}x{h2}@{:.2}Hz on {crtc:?}",
                        info.mhz as f32 / 1000.0
                    );
                    used.push(crtc);
                    outs.push(s);
                }
                Err(e) => eprintln!("[velogin] mirror: {name} failed: {e}"),
            }
        }
        Some(outs)
    }

    fn make(
        card: &Card,
        conn: connector::Handle,
        crtc: crtc::Handle,
        mode: Mode,
        w: u32,
        h: u32,
        name: String,
    ) -> std::io::Result<Secondary> {
        let mut db = card.create_dumb_buffer((w, h), DrmFourcc::Xrgb8888, 32)?;
        let fb = match card.add_framebuffer(&db, 24, 32) {
            Ok(fb) => fb,
            Err(e) => {
                let _ = card.destroy_dumb_buffer(db);
                return Err(e);
            }
        };
        if let Ok(mut map) = card.map_dumb_buffer(&mut db) {
            map.as_mut().fill(0); // black until the first frame
        }
        if let Err(e) = card.set_crtc(crtc, Some(fb), (0, 0), &[conn], Some(mode)) {
            let _ = card.destroy_framebuffer(fb);
            let _ = card.destroy_dumb_buffer(db);
            return Err(e);
        }
        Ok(Secondary { name, conn, crtc, mode, db, fb, w, h })
    }

    /// Per-tick housekeeping; returns whether a frame should be rendered.
    fn tick_needs_frame(&mut self) -> bool {
        self.tick = self.tick.wrapping_add(1);
        if self.outs.is_empty() {
            if self.tick.is_multiple_of(RESCAN_EVERY) {
                if let Some(outs) = self.setup() {
                    self.outs = outs;
                }
            }
            return false;
        }
        if self.tick.is_multiple_of(REASSERT_EVERY) {
            for o in &self.outs {
                let _ = self.card.set_crtc(o.crtc, Some(o.fb), (0, 0), &[o.conn], Some(o.mode));
            }
        }
        true
    }

    fn present(&mut self, frame: &SharedPixelBuffer<Rgba8Pixel>) {
        let (sw, sh) = (frame.width() as usize, frame.height() as usize);
        if sw == 0 || sh == 0 {
            return;
        }
        let src = frame.as_bytes();
        for o in &mut self.outs {
            let pitch = o.db.pitch() as usize;
            let Ok(mut map) = self.card.map_dumb_buffer(&mut o.db) else { continue };
            blit_scaled(src, sw, sh, map.as_mut(), pitch, o.w as usize, o.h as usize);
        }
    }

    fn teardown(self) {
        for o in self.outs {
            let _ = self.card.set_crtc(o.crtc, None, (0, 0), &[], None);
            let _ = self.card.destroy_framebuffer(o.fb);
            let _ = self.card.destroy_dumb_buffer(o.db);
            eprintln!("[velogin] mirror: released {}", o.name);
        }
        // `self.card` (our dup of the master fd) closes here.
    }
}

/// Bilinear-scale RGBA `src` into XRGB8888 `dst`, keeping aspect ratio and
/// centring (the bars keep whatever the buffer already holds: black).
fn blit_scaled(src: &[u8], sw: usize, sh: usize, dst: &mut [u8], pitch: usize, dw: usize, dh: usize) {
    if src.len() < sw * sh * 4 || dst.len() < pitch * dh || pitch < dw * 4 {
        return;
    }
    let scale = (dw as f32 / sw as f32).min(dh as f32 / sh as f32);
    let tw = ((sw as f32 * scale).round() as usize).clamp(1, dw);
    let th = ((sh as f32 * scale).round() as usize).clamp(1, dh);
    let (ox, oy) = ((dw - tw) / 2, (dh - th) / 2);

    // (left byte offset, right byte offset, weight of right in 0..=256)
    let axis = |src_n: usize, tgt: usize| -> Vec<(usize, usize, u32)> {
        (0..tgt)
            .map(|i| {
                let f = ((i as f32 + 0.5) * src_n as f32 / tgt as f32 - 0.5).max(0.0);
                let a = (f as usize).min(src_n - 1);
                let b = (a + 1).min(src_n - 1);
                (a, b, ((f - a as f32) * 256.0) as u32)
            })
            .collect()
    };
    let xs = axis(sw, tw);
    let ys = axis(sh, th);

    for (y, &(ya, yb, wy)) in ys.iter().enumerate() {
        let r0 = &src[ya * sw * 4..(ya + 1) * sw * 4];
        let r1 = &src[yb * sw * 4..(yb + 1) * sw * 4];
        let row = &mut dst[(oy + y) * pitch + ox * 4..][..tw * 4];
        for (x, &(xa, xb, wx)) in xs.iter().enumerate() {
            let mut rgb = [0u8; 3];
            for (c, out) in rgb.iter_mut().enumerate() {
                let top = (r0[xa * 4 + c] as u32 * (256 - wx) + r0[xb * 4 + c] as u32 * wx) >> 8;
                let bot = (r1[xa * 4 + c] as u32 * (256 - wx) + r1[xb * 4 + c] as u32 * wx) >> 8;
                *out = ((top * (256 - wy) + bot * wy) >> 8) as u8;
            }
            // XRGB8888, little-endian: B, G, R, X.
            row[x * 4] = rgb[2];
            row[x * 4 + 1] = rgb[1];
            row[x * 4 + 2] = rgb[0];
            row[x * 4 + 3] = 255;
        }
    }
}

pub struct MirrorHandle {
    timer: slint::Timer,
    mirror: Rc<RefCell<Option<Mirror>>>,
}

/// Mirror only once the UI has gone quiet this long: rendering a snapshot
/// (a second render plus a GPU read-back) on the UI thread stalls the real
/// display, which showed up as mouse stutter when it ran every few frames.
const SETTLE: Duration = Duration::from_millis(150);

/// Start mirroring the greeter (whose output is `SLINT_DRM_OUTPUT`) onto the
/// other connected monitors. Returns `None` when there is no known primary.
///
/// Snapshots are taken only when the window has rendered something new AND
/// has been idle for `SETTLE`, so the mirror trails the main display briefly
/// during pointer motion but never competes with it.
pub fn start<C: slint::ComponentHandle + 'static>(ui: &C) -> Option<MirrorHandle> {
    let primary = std::env::var("SLINT_DRM_OUTPUT").ok()?;
    let mirror: Rc<RefCell<Option<Mirror>>> = Rc::new(RefCell::new(None));

    let dirty = Rc::new(Cell::new(true));
    let last_render = Rc::new(Cell::new(Instant::now()));
    let snapshotting = Rc::new(Cell::new(false));
    let notifier_ok = {
        let (dirty, last_render, snapshotting) = (dirty.clone(), last_render.clone(), snapshotting.clone());
        ui.window()
            .set_rendering_notifier(move |state, _| {
                // Our own snapshot render fires this too; don't count it.
                if matches!(state, slint::RenderingState::AfterRendering) && !snapshotting.get() {
                    dirty.set(true);
                    last_render.set(Instant::now());
                }
            })
            .is_ok()
    };
    if !notifier_ok {
        eprintln!("[velogin] mirror: no render notifier, falling back to fixed-rate snapshots");
    }

    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    let state = mirror.clone();
    let mut ticks = 0u32;
    timer.start(slint::TimerMode::Repeated, TICK, move || {
        ticks = ticks.wrapping_add(1);
        let mut guard = state.borrow_mut();
        let Some(m) = guard.as_mut() else {
            // The backend opens DRM when the window is created; retry about
            // once a second until it has modeset and we can find its fd.
            if ticks.is_multiple_of(12) {
                *guard = Mirror::start(&primary);
            }
            return;
        };
        if !m.tick_needs_frame() {
            return;
        }
        let quiet = !notifier_ok || last_render.get().elapsed() >= SETTLE;
        let due = dirty.get() || (!notifier_ok && ticks.is_multiple_of(3));
        if !(quiet && due) {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };
        snapshotting.set(true);
        let frame = ui.window().take_snapshot();
        snapshotting.set(false);
        dirty.set(false);
        if let Ok(frame) = frame {
            m.present(&frame);
        }
    });
    Some(MirrorHandle { timer, mirror })
}

impl MirrorHandle {
    /// Blank and release the extra monitors and close our master fd. Call
    /// after the event loop ends, BEFORE dropping the Slint window.
    pub fn shutdown(self) {
        self.timer.stop();
        if let Some(m) = self.mirror.borrow_mut().take() {
            m.teardown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_letterboxes_and_swizzles() {
        // 2x1 source (red, blue) into a 4x4 target: scale 2 -> 4x2, bars top/bottom.
        let src = [255, 0, 0, 255, 0, 0, 255, 255];
        let mut dst = vec![0u8; 4 * 4 * 4];
        blit_scaled(&src, 2, 1, &mut dst, 16, 4, 4);
        assert_eq!(&dst[..16], &[0u8; 16], "top bar stays black");
        let row = &dst[16 + 16..16 + 32]; // y = 1 (first content row)
        assert_eq!(&row[..4], &[0, 0, 255, 255], "left pixel red as BGRX");
        assert_eq!(&row[12..16], &[255, 0, 0, 255], "right pixel blue as BGRX");
    }

    #[test]
    fn blit_refuses_undersized_buffers() {
        let mut dst = vec![7u8; 16];
        blit_scaled(&[0; 4], 1, 1, &mut dst, 16, 4, 4);
        assert_eq!(dst, vec![7u8; 16]);
    }
}
