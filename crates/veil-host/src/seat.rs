//! libseat session management: VT switching, device access (DRM, input).
//!
//! Wraps libseat to handle seat activation and device open. Only used in
//! bare-TTY mode ([`crate::output::DrmOutput`]). libseat hands us an fd for a
//! privileged device (the GPU) that is DRM-master while our VT is active, and
//! revokes it on VT switch — so we can cooperate with logind/other sessions
//! instead of fighting for the card.
//!
//! Session-active state is published in a process-global so the input thread
//! (a separate thread that can't share the `!Send` libseat handle) can gate
//! event forwarding without any plumbing: when our VT isn't foreground we stop
//! delivering input and stop drawing.
//!
//! NOTE: this type is `!Send` (libseat's handle is a raw pointer). It must
//! live on, and be dispatched from, the thread that owns it.

use libseat::{Device, Seat as LibSeat, SeatEvent};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

// libseat returns `errno::Errno`, a tuple struct over the raw OS errno.
// We map it to io::Error without naming the type (avoids a direct dep).
macro_rules! err {
    () => {
        |e| io::Error::from_raw_os_error(e.0)
    };
}

/// True while our session holds the seat (our VT is foreground). Defaults to
/// `true` so non-seat modes (terminal/windowed) are always "active".
static SESSION_ACTIVE: AtomicBool = AtomicBool::new(true);

/// Whether the bare-TTY session is currently foreground. Read by the input
/// backend to decide whether to forward events.
pub fn session_active() -> bool {
    SESSION_ACTIVE.load(Ordering::SeqCst)
}

/// Requested target VT, or 0 = none pending. The evdev input thread grabs
/// keyboards exclusively (see [`crate::input_backend::evdev_input`]), which
/// stops the kernel's own Ctrl+Alt+Fn handling from ever seeing the chord —
/// so we intercept it ourselves and relay the request here. `Seat` is
/// `!Send` and lives on the main thread (inside `DrmOutput`), which can't be
/// touched from the input thread directly, hence the atomic handoff.
static PENDING_VT: AtomicI32 = AtomicI32::new(0);

/// Called from the input thread when Ctrl+Alt+Fn is pressed.
pub fn request_vt_switch(vt: i32) {
    PENDING_VT.store(vt, Ordering::SeqCst);
}

/// Consumed once per frame by `DrmOutput::render_frame` (main thread).
pub fn take_pending_vt() -> Option<i32> {
    match PENDING_VT.swap(0, Ordering::SeqCst) {
        0 => None,
        vt => Some(vt),
    }
}

pub struct Seat {
    inner: LibSeat,
}

impl Seat {
    /// Open the seat and block until it is activated (our VT is foreground).
    /// Times out after 5s so we fail to terminal output rather than hang.
    pub fn open() -> io::Result<Self> {
        SESSION_ACTIVE.store(false, Ordering::SeqCst);

        let inner = LibSeat::open(move |seat, event| match event {
            SeatEvent::Enable => SESSION_ACTIVE.store(true, Ordering::SeqCst),
            SeatEvent::Disable => {
                SESSION_ACTIVE.store(false, Ordering::SeqCst);
                // Ack the disable so libseat can hand the seat to the next session.
                let _ = seat.disable();
            }
        })
        .map_err(err!())?;

        let mut seat = Seat { inner };

        let start = Instant::now();
        while !session_active() {
            seat.inner.dispatch(1000).map_err(err!())?;
            if start.elapsed() > Duration::from_secs(5) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "seat never activated (not on an active VT?)",
                ));
            }
        }
        Ok(seat)
    }

    /// Open a device (e.g. `/dev/dri/card0`) through the seat. The returned
    /// [`Device`] exposes its fd via `AsFd`; keep it alive for as long as the
    /// device is in use.
    pub fn open_device(&mut self, path: &str) -> io::Result<Device> {
        self.inner.open_device(&path).map_err(err!())
    }

    /// Pump pending seat events (VT enable/disable). Non-blocking.
    pub fn dispatch(&mut self) -> io::Result<()> {
        self.inner.dispatch(0).map_err(err!()).map(|_| ())
    }

    /// Whether our session currently holds the seat (foreground VT).
    pub fn is_active(&self) -> bool {
        session_active()
    }

    /// Ask libseat/seatd to switch to another VT. Cooperative — libseat
    /// disables us and enables the target session; our `SeatEvent::Disable`
    /// handler acks the handoff. A call here doesn't guarantee a switch
    /// happens (see libseat docs), just requests one.
    pub fn switch_session(&mut self, vt: i32) -> io::Result<()> {
        self.inner.switch_session(vt).map_err(err!())
    }
}
