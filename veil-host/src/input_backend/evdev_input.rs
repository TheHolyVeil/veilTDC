//! Bare-TTY input backend — reads raw `/dev/input/event*` devices via evdev.
//!
//! On a TTY there is no terminal escape stream to parse: we open the kernel
//! input devices directly. evdev hands us linux keycodes, which is exactly
//! what [`InputCmd::Key`] wants, so keyboard mapping is a passthrough. Mouse
//! motion is relative (REL_X/REL_Y); we accumulate a virtual cursor in
//! compositor pixels and clamp it to the output extent.

use super::{InputBackend, InputCtx};
use crate::input::InputCmd;
use evdev::{Device, EventSummary, KeyCode, RelativeAxisCode};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::Ordering;

pub struct EvdevInput {
    devices: Vec<Device>,
}

impl EvdevInput {
    /// Enumerate input devices, keeping those that emit keys or relative
    /// motion. Returns an error if none are usable (no permission / none found).
    pub fn open() -> std::io::Result<Self> {
        let mut devices = Vec::new();
        for (path, mut dev) in evdev::enumerate() {
            // Keep keyboards and pointing devices; skip everything else.
            let has_keys = dev.supported_keys().is_some();
            let has_rel  = dev.supported_relative_axes().is_some();
            if has_keys || has_rel {
                match dev.set_nonblocking(true) {
                    Ok(()) => {
                        // Exclusively grab: events go ONLY to veil. Stops
                        // keystrokes leaking to the underlying VT and removes any
                        // second reader that could race the HOME kill key.
                        // Non-fatal if it fails — a shared device still works.
                        if let Err(e) = dev.grab() {
                            eprintln!("[veil-host] evdev: {path:?} grab failed (shared): {e}");
                        }
                        devices.push(dev);
                    }
                    Err(e) => eprintln!("[veil-host] evdev: {path:?} nonblocking failed: {e}"),
                }
            }
        }
        if devices.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no readable /dev/input devices",
            ));
        }
        eprintln!("[veil-host] evdev: opened {} device(s)", devices.len());
        Ok(Self { devices })
    }
}

impl InputBackend for EvdevInput {
    fn name(&self) -> &'static str {
        "evdev"
    }

    fn run(self: Box<Self>, ctx: InputCtx) {
        let InputCtx { tx, running, host_stop, geom } = ctx;
        let mut devices = self.devices;
        eprintln!("[veil-host] input thread started (evdev, {} device(s), event-driven)", devices.len());

        // Virtual cursor, starts centred.
        let mut cx = (geom.comp_w.load(Ordering::Relaxed) / 2) as i32;
        let mut cy = (geom.comp_h.load(Ordering::Relaxed) / 2) as i32;

        // Ctrl/Alt held-state for the Ctrl+Alt+Fn VT-switch chord (see
        // handle_event). The exclusive grab below eats this chord before the
        // kernel's own VT switching ever sees it, so we emulate it ourselves.
        let mut ctrl_held = false;
        let mut alt_held = false;

        // Poll the device fds directly instead of busy-polling with a sleep.
        // The kernel wakes us the instant an event lands, so we drain promptly
        // even when the rest of veil is CPU-saturated compositing a nested
        // stack — there's no sleep gap for a HOME press to fall into, which is
        // what made the kill key unreliable under load. fds are stable for the
        // device lifetime; -1 masks a device that has gone away (poll ignores it).
        let mut pfds: Vec<libc::pollfd> = devices.iter()
            .map(|d| libc::pollfd { fd: d.as_raw_fd(), events: libc::POLLIN, revents: 0 })
            .collect();

        let mut was_active = true;

        while running.load(Ordering::Relaxed) {
            let active = crate::seat::session_active();
            if !active && was_active {
                eprintln!("[veil-host] evdev: session inactive -> releasing device grabs for VT switch");
                for dev in &mut devices {
                    let _ = dev.ungrab();
                }
                was_active = false;
            } else if active && !was_active {
                eprintln!("[veil-host] evdev: session active -> re-asserting device grabs");
                for dev in &mut devices {
                    let _ = dev.grab();
                }
                // Any modifier held when we went inactive may never have had
                // its release delivered: events that arrive while inactive
                // are drained above (must be, or the device queue backs up)
                // but never passed through `handle_event` (see the `if
                // active` guard below), so if the physical release happened
                // on the other VT, the compositor's own keyboard state never
                // heard about it — stuck holding a phantom modifier forever
                // after. Synthesize the release now, for both L/R variants
                // regardless of which was actually pressed (ctrl_held/
                // alt_held conflate them already, and releasing an
                // already-released key is a no-op — safe either way). If the
                // key is in fact still physically down, the next real event
                // reasserts it correctly; this only ever clears a phantom.
                if ctrl_held {
                    let _ = tx.send(InputCmd::Key { keycode: KEY_LEFTCTRL as u32, mods: 0, pressed: false });
                    let _ = tx.send(InputCmd::Key { keycode: KEY_RIGHTCTRL as u32, mods: 0, pressed: false });
                    ctrl_held = false;
                }
                if alt_held {
                    let _ = tx.send(InputCmd::Key { keycode: KEY_LEFTALT as u32, mods: 0, pressed: false });
                    let _ = tx.send(InputCmd::Key { keycode: KEY_RIGHTALT as u32, mods: 0, pressed: false });
                    alt_held = false;
                }
                was_active = true;
            }

            // 200 ms cap so we still notice `running` being cleared by another path.
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 200) };
            if n <= 0 { continue; } // 0 = timeout, <0 = EINTR/error → re-check running
            for (i, dev) in devices.iter_mut().enumerate() {
                let re = pfds[i].revents;
                pfds[i].revents = 0;
                if re & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    eprintln!("[veil-host] evdev: device {i} gone, dropping");
                    pfds[i].fd = -1; // poll skips negative fds
                    continue;
                }
                if re & libc::POLLIN == 0 { continue; }

                let events = match dev.fetch_events() {
                    Ok(it) => it,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => { eprintln!("[veil-host] evdev read error: {e}"); continue; }
                };
                let mut moved = false;
                for ev in events {
                    if active {
                        if handle_event(
                            &tx, ev, &geom, &mut cx, &mut cy, &running, &host_stop,
                            &mut ctrl_held, &mut alt_held,
                        ) {
                            moved = true;
                        }
                    }
                }
                if active && moved {
                    let comp_w = geom.comp_w.load(Ordering::Relaxed) as i32;
                    let comp_h = geom.comp_h.load(Ordering::Relaxed) as i32;
                    send_motion(&tx, cx, cy, comp_w, comp_h);
                }
            }
        }
        eprintln!("[veil-host] input thread exited (evdev)");
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_event(
    tx: &std::sync::mpsc::Sender<InputCmd>,
    ev: evdev::InputEvent,
    geom: &super::InputGeometry,
    cx: &mut i32,
    cy: &mut i32,
    running: &std::sync::atomic::AtomicBool,
    host_stop: &std::sync::atomic::AtomicBool,
    ctrl_held: &mut bool,
    alt_held: &mut bool,
) -> bool {
    let mut moved = false;
    match ev.destructure() {
        EventSummary::Key(_, key, value) => {
            // value: 0 = release, 1 = press, 2 = repeat (Wayland drives repeat).
            if value == 2 { return false; }
            let pressed = value == 1;
            let code = key.code();

            if code == KEY_LEFTCTRL || code == KEY_RIGHTCTRL { *ctrl_held = pressed; }
            if code == KEY_LEFTALT || code == KEY_RIGHTALT { *alt_held = pressed; }

            // VT switch chord: swallow it here rather than forward it — the
            // exclusive grab means the kernel/console never gets a chance to
            // act on it, so we drive libseat's switch_session ourselves via
            // the seat module's atomic handoff (DrmOutput owns the !Send
            // Seat on the main thread and consumes this once per frame).
            if *ctrl_held && *alt_held && pressed {
                if let Some(vt) = vt_for_fkey(code) {
                    crate::seat::request_vt_switch(vt);
                    return false;
                }
            }

            // Compositor kill key. Ctrl-C is forwarded to the hosted app (it
            // never reaches us on a TTY), so HOME is the escape hatch: it always
            // tears veil down, even if the guest wedges. The guest never sees it.
            if code == KEY_HOME && pressed {
                eprintln!("[veil-host] HOME → shutdown");
                crate::vt::emergency_restore();
                running.store(false, Ordering::Relaxed);
                host_stop.store(true, Ordering::Relaxed);
                return false;
            }

            if is_button(key) {
                if pressed {
                    let _ = tx.send(InputCmd::PointerButton { button: code as u32, pressed: true });
                } else {
                    let _ = tx.send(InputCmd::PointerButton { button: code as u32, pressed: false });
                }
            } else {
                let _ = tx.send(InputCmd::Key { keycode: code as u32, mods: 0, pressed });
            }
        }
        EventSummary::RelativeAxis(_, axis, value) => {
            let comp_w = geom.comp_w.load(Ordering::Relaxed) as i32;
            let comp_h = geom.comp_h.load(Ordering::Relaxed) as i32;
            match axis {
                RelativeAxisCode::REL_X => {
                    let old = *cx;
                    *cx = (*cx + value).clamp(0, comp_w.max(1) - 1);
                    if *cx != old { moved = true; }
                }
                RelativeAxisCode::REL_Y => {
                    let old = *cy;
                    *cy = (*cy + value).clamp(0, comp_h.max(1) - 1);
                    if *cy != old { moved = true; }
                }
                RelativeAxisCode::REL_WHEEL => {
                    // evdev: +1 = wheel up. InputCmd: +120 = scroll down.
                    let _ = tx.send(InputCmd::Scroll { v120: -value * 120 });
                }
                _ => {}
            }
        }
        _ => {}
    }
    moved
}

fn send_motion(tx: &std::sync::mpsc::Sender<InputCmd>, x: i32, y: i32, w: i32, h: i32) {
    let _ = tx.send(InputCmd::PointerMotionAbs {
        x,
        y,
        width: w.max(1) as u32,
        height: h.max(1) as u32,
    });
}

/// BTN_LEFT..BTN_TASK live in 0x110..=0x117; treat those as pointer buttons.
fn is_button(key: KeyCode) -> bool {
    (0x110..=0x117).contains(&key.code())
}

/// evdev KEY_HOME (linux/input-event-codes.h) — our compositor kill key.
const KEY_HOME: u16 = 102;

// linux/input-event-codes.h — modifier + function-key codes for the
// Ctrl+Alt+Fn VT-switch chord.
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;

/// Map KEY_F1..KEY_F12 to their VT number (F1 → VT 1, etc). F1..F10 are
/// contiguous (59..=68); F11/F12 are non-contiguous (87, 88).
fn vt_for_fkey(code: u16) -> Option<i32> {
    match code {
        59..=68 => Some((code - 59 + 1) as i32),
        87 => Some(11),
        88 => Some(12),
        _ => None,
    }
}
