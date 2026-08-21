//! DRM/KMS framebuffer output backend for bare TTY.
//!
//! When there's no terminal to draw into, veil-host *becomes* the display
//! server: it mode-sets a CRTC and scans out frames directly from GPU memory.
//! Device access + VT switching go through libseat ([`crate::seat::Seat`]), so
//! we cooperate with logind instead of stealing the card.
//!
//! Rendering is double-buffered dumb buffers with async page-flips. The
//! compositor's RGBA frames are repacked into XRGB8888 (the format KMS scans
//! out), clipped/letterboxed to the display mode.

use super::OutputBackend;
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

/// Card-bound KMS state produced by [`DrmOutput::try_card`], before the owned
/// [`Seat`] is folded in.
struct CardSetup {
    card:   Card,
    crtc:   crtc::Handle,
    conn:   connector::Handle,
    mode:   Mode,
    width:  u32,
    height: u32,
    bufs:   [DumbBuffer; 2],
    fbs:    [framebuffer::Handle; 2],
}

impl CardSetup {
    fn into_output(self, seat: Seat, vt: VtGuard) -> DrmOutput {
        DrmOutput {
            seat,
            card: self.card,
            crtc: self.crtc,
            conn: self.conn,
            mode: self.mode,
            width: self.width,
            height: self.height,
            bufs: self.bufs,
            fbs: self.fbs,
            back: 1,
            flip_pending: false,
            was_active: true,
            _vt: vt,
        }
    }
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
    seat:         Seat,
    card:         Card,
    crtc:         crtc::Handle,
    conn:         connector::Handle,
    mode:         Mode,
    width:        u32,
    height:       u32,
    bufs:         [DumbBuffer; 2],
    fbs:          [framebuffer::Handle; 2],
    /// Index of the buffer we'll render into next (not currently scanned out).
    back:         usize,
    /// A page-flip is queued; its completion event hasn't been drained yet.
    flip_pending: bool,
    /// Seat activity as of the last `render_frame` call, to detect the
    /// disable→enable edge (VT switched back to us) and re-assert our CRTC —
    /// whatever grabbed the VT in between (fbcon, another compositor) has its
    /// own idea of what that CRTC should be scanning out.
    was_active:   bool,
    /// VT held in graphics mode. Declared LAST so it drops last: our `Drop`
    /// destroys buffers, then `card` drops (releasing DRM-master), then this
    /// restores text mode — handing a clean console back to fbcon / the
    /// resuming compositor.
    _vt:          VtGuard,
}

impl Drop for DrmOutput {
    fn drop(&mut self) {
        // Best-effort teardown — we're going away regardless. Destroying the
        // framebuffers + dumb buffers and dropping master lets the previously
        // paused session (e.g. niri) re-modeset cleanly on resume.
        for fb in self.fbs {
            let _ = self.card.destroy_framebuffer(fb);
        }
        for db in self.bufs {
            let _ = self.card.destroy_dumb_buffer(db);
        }
    }
}

impl DrmOutput {
    /// Open a GPU via libseat, pick the first connected output, mode-set it,
    /// and allocate double buffers. Scans every `/dev/dri/card*` because the
    /// KMS-capable node isn't always `card0` (common on multi-GPU / AMD rigs).
    /// Fails (→ caller falls back to terminal) if not on an active VT, no card,
    /// or no connected display.
    pub fn new() -> io::Result<Self> {
        let mut seat = Seat::open()?;

        // Suspend fbcon BEFORE touching any CRTC. Mode-setting while the VT is
        // in text mode lets fbcon and our page-flips fight over the same CRTC —
        // the amdgpu hard-lock path. If acquire fails, the loop's `vt` is
        // dropped (text mode restored) and the caller falls back to terminal.
        let vt = VtGuard::acquire()?;

        let cards = list_cards();
        if cards.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no /dev/dri/card* nodes found",
            ));
        }

        let mut last_err: Option<io::Error> = None;
        for path in &cards {
            match Self::try_card(&mut seat, path) {
                Ok(setup) => return Ok(setup.into_output(seat, vt)),
                Err(e) => {
                    eprintln!("[veil-host] DRM: {path} unusable: {e}");
                    last_err = Some(e);
                }
            }
        }
        // No card worked: `vt` drops here, restoring text mode.
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no usable DRM card")
        }))
    }

    /// Attempt full KMS setup on one card node. Returns the card-bound state;
    /// the (owned) seat is folded in by the caller via [`CardSetup::into_output`].
    fn try_card(seat: &mut Seat, path: &str) -> io::Result<CardSetup> {
        let dev = seat.open_device(path)?;
        let card = Card { dev };

        // Non-blocking so we can drain page-flip events without stalling.
        set_nonblocking(card.as_fd())?;

        let res = card.resource_handles()?;

        let con = res
            .connectors()
            .iter()
            .flat_map(|c| card.get_connector(*c, true))
            .find(|i| i.state() == connector::State::Connected)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no connected display"))?;

        let &mode = con
            .modes()
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connector has no modes"))?;

        // Pick a CRTC the connector can actually drive: walk its encoders
        // (preferring the one already in use) and take the first CRTC allowed
        // by that encoder's possible_crtcs mask. Taking crtcs[0] blindly is the
        // classic cause of EINVAL on set_crtc.
        let encoders = con
            .current_encoder()
            .into_iter()
            .chain(con.encoders().iter().copied());
        let crtc = encoders
            .filter_map(|enc| card.get_encoder(enc).ok())
            .flat_map(|info| res.filter_crtcs(info.possible_crtcs()))
            .next()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no CRTC compatible with connector")
            })?;

        let (mw, mh) = mode.size();
        let (width, height) = (mw as u32, mh as u32);

        let make = || -> io::Result<(DumbBuffer, framebuffer::Handle)> {
            let db = card.create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)?;
            let fb = card.add_framebuffer(&db, 24, 32)?;
            Ok((db, fb))
        };
        let (b0, f0) = make()?;
        let (b1, f1) = make()?;

        // Initial mode-set scans out buffer 0; we render into buffer 1 first.
        card.set_crtc(crtc, Some(f0), (0, 0), &[con.handle()], Some(mode))?;

        eprintln!(
            "[veil-host] DRM: {path} {width}x{height}@{}Hz",
            mode.vrefresh()
        );

        Ok(CardSetup {
            card,
            crtc,
            conn: con.handle(),
            mode,
            width,
            height,
            bufs: [b0, b1],
            fbs: [f0, f1],
        })
    }

    /// Drain any completed page-flip events (clears `flip_pending`).
    fn drain_flips(&mut self) -> io::Result<()> {
        if !self.flip_pending {
            return Ok(());
        }
        match self.card.receive_events() {
            Ok(events) => {
                let mut got = false;
                for _ in events {
                    got = true;
                }
                if got {
                    self.flip_pending = false;
                }
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()), // still in flight
            Err(e) => Err(e),
        }
    }
}

impl OutputBackend for DrmOutput {
    fn render_frame(&mut self, rgba: &[u8], fw: u32, fh: u32) -> io::Result<()> {
        // Service VT enable/disable. While suspended (another VT foreground)
        // we must not touch the card.
        let _ = self.seat.dispatch();

        // A Ctrl+Alt+Fn chord caught by the (grabbed) evdev thread — relay it
        // to libseat. Checked before the is_active early-return: this is how
        // we switch AWAY, so it must fire even mid-transition.
        if let Some(vt) = crate::seat::take_pending_vt() {
            if let Err(e) = self.seat.switch_session(vt) {
                eprintln!("[veil-host] VT switch to {vt} failed: {e}");
            }
        }

        let active_now = self.seat.is_active();
        if active_now && !self.was_active {
            // Coming back from a VT switch: whatever had the display in
            // between left the CRTC in an unknown state, and any flip we had
            // in flight before switching away is never going to complete —
            // its fence belonged to the old CRTC config. Re-assert our mode
            // before touching page_flip again, or it'll EINVAL and (since
            // that error propagates out of the frame loop) take the whole
            // compositor down with it.
            self.flip_pending = false;
            match self.on_vt_switch(true) {
                // Only clear the resume flag once the re-modeset actually
                // lands — a transient failure (e.g. the outgoing session
                // hasn't released the CRTC yet) retries every frame instead
                // of wedging silently until the next VT-switch cycle.
                Ok(()) => self.was_active = true,
                Err(e) => eprintln!("[veil-host] VT resume: re-modeset failed, retrying: {e}"),
            }
        } else {
            self.was_active = active_now;
        }

        if !active_now {
            return Ok(());
        }
        if !self.was_active {
            // Re-modeset still hasn't landed — don't attempt to flip against
            // a CRTC we don't trust yet.
            return Ok(());
        }

        self.drain_flips()?;

        let back = self.back;
        let pitch = self.bufs[back].pitch() as usize;
        {
            let mut map = self.card.map_dumb_buffer(&mut self.bufs[back])?;
            blit_rgba_to_xrgb(map.as_mut(), pitch, self.width, self.height, rgba, fw, fh);
        }

        if !self.flip_pending {
            match self
                .card
                .page_flip(self.crtc, self.fbs[back], PageFlipFlags::EVENT, None)
            {
                Ok(()) => {
                    self.flip_pending = true;
                    self.back ^= 1;
                }
                // EBUSY: previous flip not retired yet — drop this frame.
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
                // Anything else (e.g. a stale CRTC state we didn't catch):
                // drop the frame rather than taking the whole compositor
                // down over one bad flip.
                Err(e) => eprintln!("[veil-host] page_flip failed: {e}"),
            }
        }
        Ok(())
    }

    fn get_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn on_vt_switch(&mut self, switch_in: bool) -> io::Result<()> {
        // libseat drives the actual enable/disable via dispatch(); this is a
        // hook for the frame loop. On switch-in, re-assert our mode.
        if switch_in && self.seat.is_active() {
            let front = self.back ^ 1;
            self.card.set_crtc(
                self.crtc,
                Some(self.fbs[front]),
                (0, 0),
                &[self.conn],
                Some(self.mode),
            )?;
        }
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
fn blit_rgba_to_xrgb(
    dst: &mut [u8],
    pitch: usize,
    dw: u32,
    dh: u32,
    src: &[u8],
    sw: u32,
    sh: u32,
) {
    let copy_w = dw.min(sw) as usize;
    let copy_h = dh.min(sh) as usize;
    let src_pitch = (sw as usize) * 4;

    for y in 0..dh as usize {
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
