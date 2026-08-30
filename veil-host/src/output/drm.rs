//! DRM/KMS framebuffer output backend for bare TTY.
//!
//! When there's no terminal to draw into, veil-host *becomes* the display
//! server: it mode-sets a CRTC per connected output and scans out frames
//! directly from GPU memory. Device access + VT switching go through
//! libseat ([`crate::seat::Seat`]), so we cooperate with logind instead of
//! stealing the card.
//!
//! Rendering is double-buffered dumb buffers with async page-flips, one
//! independent pipeline per physical monitor. The compositor's RGBA frames
//! are repacked into XRGB8888 (the format KMS scans out), clipped/
//! letterboxed to each display's own mode.
//!
//! Multi-monitor is "dumb" by design (see MULTI_MONITOR_SCOPE.md): every
//! connected connector on every card gets its own independent CRTC + buffer
//! pair here. This module only enumerates and drives them — it has no idea
//! what content goes on which monitor, or how they're arranged in space;
//! that's `State`'s job (Phase 2+), driven by `monitor_count()`/`get_size()`
//! at startup and a `monitor` index on every call after.

use super::OutputBackend;
use crate::layout::Rect;
use crate::seat::Seat;
use crate::vt::VtGuard;
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{
    connector, crtc, dumbbuffer::DumbBuffer, framebuffer, Device as ControlDevice, Mode,
    PageFlipFlags,
};
use drm::Device as DrmDevice;
use std::io;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd};

/// Thin wrapper giving a libseat-owned device fd the drm crate traits.
struct Card {
    dev: libseat::Device,
}

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.dev.as_fd()
    }
}
impl DrmDevice for Card {}
impl ControlDevice for Card {}

/// One physical display's full KMS pipeline: its own CRTC, its own pair of
/// dumb buffers, its own page-flip and damage-accumulator state. Fully
/// independent of every other `Monitor` except for sharing a `Card` fd
/// (referenced by index, not owned — DRM ioctls on `Card` take `&self`, so
/// multiple monitors can drive the same card concurrently without any
/// exclusive-access dance).
struct Monitor {
    /// Index into `DrmOutput::cards` — which physical GPU node this
    /// monitor's CRTC lives on.
    card_idx: usize,
    crtc:     crtc::Handle,
    conn:     connector::Handle,
    mode:     Mode,
    width:    u32,
    height:   u32,
    bufs:     [DumbBuffer; 2],
    fbs:      [framebuffer::Handle; 2],
    /// Index of the buffer we'll render into next (not currently scanned out).
    back:     usize,
    /// Damage not yet applied to buffer `[i]`, accumulated since that
    /// buffer's last write. See the equivalent field in the original
    /// single-monitor design — same buffer-age reasoning, just now one copy
    /// of this state per monitor instead of one for the whole backend.
    pending_damage: [Rect; 2],
    /// A page-flip is queued; its completion event hasn't been drained yet.
    flip_pending: bool,
    /// Seat activity as of the last `render_frame` call for *this* monitor,
    /// to detect the disable→enable edge (VT switched back to us) and
    /// re-assert this CRTC specifically.
    was_active: bool,
}

/// Enumerate `/dev/dri/card*` primary nodes, numerically sorted.
pub(crate) fn list_cards() -> Vec<String> {
    let mut cards: Vec<String> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.strip_prefix("card")
                .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
                .map(|_| format!("/dev/dri/{name}"))
        })
        .collect();
    cards.sort();
    cards
}

pub struct DrmOutput {
    seat:     Seat,
    cards:    Vec<Card>,
    monitors: Vec<Monitor>,
    /// VT held in graphics mode. Declared LAST so it drops last: our `Drop`
    /// destroys buffers, then `cards` drop (releasing DRM-master), then this
    /// restores text mode — handing a clean console back to fbcon / the
    /// resuming compositor.
    _vt: VtGuard,
}

impl Drop for DrmOutput {
    fn drop(&mut self) {
        // Best-effort teardown — we're going away regardless.
        for m in &self.monitors {
            let card = &self.cards[m.card_idx];
            for fb in m.fbs {
                let _ = card.destroy_framebuffer(fb);
            }
            for db in m.bufs {
                let _ = card.destroy_dumb_buffer(db);
            }
        }
    }
}

impl DrmOutput {
    /// Open every GPU via libseat, mode-set every connected connector found
    /// on any of them, and allocate double buffers for each. Tries every
    /// card (not just the first that works) so a multi-GPU rig's displays
    /// on a secondary card aren't silently dropped. Fails (→ caller falls
    /// back to terminal) if not on an active VT or no card has any
    /// connected, usable display.
    pub fn new() -> io::Result<Self> {
        let mut seat = Seat::open()?;

        // Suspend fbcon BEFORE touching any CRTC. Mode-setting while the VT is
        // in text mode lets fbcon and our page-flips fight over the same CRTC —
        // the amdgpu hard-lock path. If acquire fails, the loop's `vt` is
        // dropped (text mode restored) and the caller falls back to terminal.
        let vt = VtGuard::acquire()?;

        let card_paths = list_cards();
        if card_paths.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no /dev/dri/card* nodes found",
            ));
        }

        let mut cards: Vec<Card> = Vec::new();
        let mut monitors: Vec<Monitor> = Vec::new();
        let mut last_err: Option<io::Error> = None;

        for path in &card_paths {
            match Self::try_card_all(&mut seat, path) {
                Ok((card, mut card_monitors)) => {
                    let card_idx = cards.len();
                    eprintln!(
                        "[veil-host] DRM: {path} → {} display(s)",
                        card_monitors.len()
                    );
                    for m in &card_monitors {
                        eprintln!(
                            "[veil-host] DRM:   {}x{}@{}Hz",
                            m.width, m.height, m.mode.vrefresh()
                        );
                    }
                    for m in &mut card_monitors {
                        m.card_idx = card_idx;
                    }
                    cards.push(card);
                    monitors.extend(card_monitors);
                }
                Err(e) => {
                    eprintln!("[veil-host] DRM: {path} unusable: {e}");
                    last_err = Some(e);
                }
            }
        }

        if monitors.is_empty() {
            // No card worked: `vt` drops here, restoring text mode.
            return Err(last_err.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no usable DRM card")
            }));
        }

        Ok(Self { seat, cards, monitors, _vt: vt })
    }

    /// Open one card and mode-set *every* connected connector on it, not
    /// just the first. Returns the opened card plus one [`Monitor`] per
    /// connector that mode-set successfully (`card_idx` left as `0`; the
    /// caller fixes it up once it knows this card's real index).
    fn try_card_all(seat: &mut Seat, path: &str) -> io::Result<(Card, Vec<Monitor>)> {
        let dev = seat.open_device(path)?;
        let card = Card { dev };

        // Non-blocking so we can drain page-flip events without stalling.
        set_nonblocking(card.as_fd())?;

        let res = card.resource_handles()?;

        let connected: Vec<_> = res
            .connectors()
            .iter()
            .flat_map(|c| card.get_connector(*c, true))
            .filter(|i| i.state() == connector::State::Connected)
            .collect();

        if connected.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no connected display"));
        }

        // Track CRTCs already claimed by an earlier connector on this same
        // card — without this, two connectors sharing an encoder's
        // possible_crtcs mask (common on a single-GPU dual-output setup)
        // would both pick the same CRTC and the second set_crtc call would
        // silently steal the first connector's display.
        let mut claimed: Vec<crtc::Handle> = Vec::new();
        let mut monitors = Vec::new();
        let mut errs = Vec::new();

        for con in &connected {
            let &mode = match con.modes().first() {
                Some(m) => m,
                None => {
                    errs.push(format!("{:?}: no modes", con.handle()));
                    continue;
                }
            };

            // Pick a CRTC the connector can actually drive: walk its encoders
            // (preferring the one already in use), take the first CRTC
            // allowed by that encoder's possible_crtcs mask that no earlier
            // connector on this card has already claimed. Taking crtcs[0]
            // blindly is the classic cause of EINVAL on set_crtc.
            let encoders = con
                .current_encoder()
                .into_iter()
                .chain(con.encoders().iter().copied());
            let crtc = encoders
                .filter_map(|enc| card.get_encoder(enc).ok())
                .flat_map(|info| res.filter_crtcs(info.possible_crtcs()))
                .find(|c| !claimed.contains(c));
            let crtc = match crtc {
                Some(c) => c,
                None => {
                    errs.push(format!("{:?}: no free CRTC", con.handle()));
                    continue;
                }
            };

            let (mw, mh) = mode.size();
            let (width, height) = (mw as u32, mh as u32);

            let make = || -> io::Result<(DumbBuffer, framebuffer::Handle)> {
                let db = card.create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)?;
                let fb = card.add_framebuffer(&db, 24, 32)?;
                Ok((db, fb))
            };
            let setup = (|| -> io::Result<Monitor> {
                let (b0, f0) = make()?;
                let (b1, f1) = make()?;
                // Initial mode-set scans out buffer 0; we render into buffer 1 first.
                card.set_crtc(crtc, Some(f0), (0, 0), &[con.handle()], Some(mode))?;
                let full = Rect { x: 0, y: 0, w: width, h: height };
                Ok(Monitor {
                    card_idx: 0, // fixed up by the caller
                    crtc,
                    conn: con.handle(),
                    mode,
                    width,
                    height,
                    bufs: [b0, b1],
                    fbs: [f0, f1],
                    back: 1,
                    // Both slots start "fully dirty": neither dumb buffer has
                    // real content yet, so the first write to each must be a
                    // full frame regardless of what the compositor's first
                    // damage rect says.
                    pending_damage: [full, full],
                    flip_pending: false,
                    was_active: true,
                })
            })();

            match setup {
                Ok(m) => {
                    claimed.push(m.crtc);
                    monitors.push(m);
                }
                Err(e) => errs.push(format!("{:?}: {e}", con.handle())),
            }
        }

        if monitors.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no usable connector on {path}: {}", errs.join("; ")),
            ));
        }

        Ok((card, monitors))
    }

    /// Drain any completed page-flip events for one monitor's card,
    /// clearing that monitor's `flip_pending`.
    fn drain_flips(&mut self, idx: usize) -> io::Result<()> {
        if !self.monitors[idx].flip_pending {
            return Ok(());
        }
        let card = &self.cards[self.monitors[idx].card_idx];
        match card.receive_events() {
            Ok(events) => {
                let mut got = false;
                for _ in events {
                    got = true;
                }
                if got {
                    self.monitors[idx].flip_pending = false;
                }
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()), // still in flight
            Err(e) => Err(e),
        }
    }
}

impl OutputBackend for DrmOutput {
    fn monitor_count(&self) -> usize {
        self.monitors.len()
    }

    fn render_frame(&mut self, monitor: usize, rgba: &[u8], fw: u32, fh: u32, damage: Rect) -> io::Result<()> {
        if monitor >= self.monitors.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no such monitor"));
        }

        // Accumulate into both buffer-age slots before anything else in this
        // function can early-return. See `Monitor::pending_damage` — same
        // per-slot reasoning as before, now scoped to one monitor.
        for p in &mut self.monitors[monitor].pending_damage {
            *p = p.union(&damage);
        }

        // Service VT enable/disable. While suspended (another VT foreground)
        // we must not touch any card. This is seat-wide, not per-monitor —
        // dispatched once regardless of which monitor's render_frame call
        // happens to trigger it first this tick.
        let _ = self.seat.dispatch();

        // A Ctrl+Alt+Fn chord caught by the (grabbed) evdev thread — relay it
        // to libseat. Checked before the is_active early-return: this is how
        // we switch AWAY, so it must fire even mid-transition. Only needs
        // doing once per tick; harmless to repeat per-monitor since
        // `take_pending_vt()` drains the flag on first call.
        if let Some(vt) = crate::seat::take_pending_vt() {
            if let Err(e) = self.seat.switch_session(vt) {
                eprintln!("[veil-host] VT switch to {vt} failed: {e}");
            }
        }

        let active_now = self.seat.is_active();
        if active_now && !self.monitors[monitor].was_active {
            // Coming back from a VT switch: whatever had the display in
            // between left this CRTC in an unknown state, and any flip we
            // had in flight before switching away is never going to
            // complete — its fence belonged to the old CRTC config.
            // Re-assert this monitor's mode before touching page_flip
            // again, or it'll EINVAL and (since that error propagates out
            // of the frame loop) take the whole compositor down with it.
            self.monitors[monitor].flip_pending = false;
            match self.reassert_crtc(monitor) {
                Ok(()) => self.monitors[monitor].was_active = true,
                Err(e) => eprintln!("[veil-host] VT resume: re-modeset failed on monitor {monitor}, retrying: {e}"),
            }
        } else {
            self.monitors[monitor].was_active = active_now;
        }

        if !active_now || !self.monitors[monitor].was_active {
            // Either suspended, or re-modeset still hasn't landed — don't
            // attempt to flip against a CRTC we don't trust yet.
            return Ok(());
        }

        self.drain_flips(monitor)?;

        let card_idx = self.monitors[monitor].card_idx;
        let back = self.monitors[monitor].back;
        let (width, height) = (self.monitors[monitor].width, self.monitors[monitor].height);
        let pitch = self.monitors[monitor].bufs[back].pitch() as usize;
        let rows = self.monitors[monitor].pending_damage[back];
        let y0 = rows.y.max(0) as usize;
        let y1 = ((rows.y + rows.h as i32).max(0) as usize).min(height as usize);
        {
            let card = &self.cards[card_idx];
            let mut map = card.map_dumb_buffer(&mut self.monitors[monitor].bufs[back])?;
            blit_rgba_to_xrgb(map.as_mut(), pitch, width, height, rgba, fw, fh, y0, y1);
        }
        // This slot now matches the source for everything in [y0, y1) — the
        // only rows it was behind on.
        self.monitors[monitor].pending_damage[back] = Rect { x: 0, y: 0, w: 0, h: 0 };

        if !self.monitors[monitor].flip_pending {
            let card = &self.cards[card_idx];
            let crtc = self.monitors[monitor].crtc;
            let fb = self.monitors[monitor].fbs[back];
            match card.page_flip(crtc, fb, PageFlipFlags::EVENT, None) {
                Ok(()) => {
                    self.monitors[monitor].flip_pending = true;
                    self.monitors[monitor].back ^= 1;
                }
                // EBUSY: previous flip not retired yet — drop this frame.
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
                // Anything else (e.g. a stale CRTC state we didn't catch):
                // drop the frame rather than taking the whole compositor
                // down over one bad flip.
                Err(e) => eprintln!("[veil-host] page_flip failed on monitor {monitor}: {e}"),
            }
        }
        Ok(())
    }

    fn get_size(&self, monitor: usize) -> (u32, u32) {
        self.monitors.get(monitor).map_or((0, 0), |m| (m.width, m.height))
    }

    fn on_vt_switch(&mut self, switch_in: bool) -> io::Result<()> {
        // libseat drives the actual enable/disable via dispatch(); this is a
        // hook for the frame loop. On switch-in, re-assert every monitor's
        // mode — the whole seat came back, not just one display.
        if switch_in && self.seat.is_active() {
            for idx in 0..self.monitors.len() {
                if let Err(e) = self.reassert_crtc(idx) {
                    eprintln!("[veil-host] on_vt_switch: re-modeset failed on monitor {idx}: {e}");
                }
            }
        }
        Ok(())
    }
}

impl DrmOutput {
    /// Re-assert one monitor's CRTC to whatever it last had scanned out.
    /// Pulled out of `render_frame`/`on_vt_switch` since both need the exact
    /// same "front buffer, same connector, same mode" call.
    fn reassert_crtc(&self, idx: usize) -> io::Result<()> {
        let m = &self.monitors[idx];
        let card = &self.cards[m.card_idx];
        let front = m.back ^ 1;
        card.set_crtc(m.crtc, Some(m.fbs[front]), (0, 0), &[m.conn], Some(m.mode))?;
        Ok(())
    }
}

/// Set O_NONBLOCK on the DRM fd so `receive_events` returns immediately.
fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Repack compositor RGBA (R,G,B,A) into KMS XRGB8888 little-endian (B,G,R,X),
/// honouring the destination pitch and clipping to the overlap of frame/display.
/// Only rows in `[y0, y1)` are touched — callers pass the rest of the buffer's
/// existing content is assumed still correct (buffer-age damage tracking in
/// `render_frame` guarantees that). Full row width is still copied within
/// that range regardless of the source damage rect's x-extent: the source
/// `src` is always a complete, correct frame (composite never produces a
/// partial one), so widening within an already-included row wastes a few
/// bytes, never risks correctness.
fn blit_rgba_to_xrgb(
    dst: &mut [u8],
    pitch: usize,
    dw: u32,
    dh: u32,
    src: &[u8],
    sw: u32,
    sh: u32,
    y0: usize,
    y1: usize,
) {
    let copy_w = dw.min(sw) as usize;
    let copy_h = dh.min(sh) as usize;
    let src_pitch = (sw as usize) * 4;

    for y in y0..y1.min(dh as usize) {
        let drow = &mut dst[y * pitch..y * pitch + (dw as usize) * 4];
        if y >= copy_h {
            drow.fill(0);
            continue;
        }
        let srow = &src[y * src_pitch..y * src_pitch + (sw as usize) * 4];
        for x in 0..copy_w {
            let s = x * 4;
            let d = x * 4;
            drow[d]     = srow[s + 2]; // B
            drow[d + 1] = srow[s + 1]; // G
            drow[d + 2] = srow[s];     // R
            drow[d + 3] = 0;           // X
        }
        // Letterbox to the right of the copied region.
        if copy_w < dw as usize {
            drow[copy_w * 4..].fill(0);
        }
    }
}
