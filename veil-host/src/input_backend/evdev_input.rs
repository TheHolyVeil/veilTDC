//! Bare-TTY input backend — reads raw `/dev/input/event*` devices via evdev.
//!
//! On a TTY there is no terminal escape stream to parse: we open the kernel
//! input devices directly. evdev hands us linux keycodes, which is exactly
//! what [`InputCmd::Key`] wants, so keyboard mapping is a passthrough. Mouse
//! motion is relative (REL_X/REL_Y); we accumulate a virtual cursor in
//! compositor pixels and clamp it to the output extent.

use super::{InputBackend, InputCtx};
use crate::input::InputCmd;
use super::touchpad::{self, Touchpad};
use evdev::{AbsoluteAxisCode, Device, EventSummary, KeyCode, PropType, RelativeAxisCode, SynchronizationCode};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::Ordering;

pub struct EvdevInput {
    devices: Vec<Device>,
    /// Parallel to `devices`: `Some` for a touchpad (absolute finger
    /// positions, no REL_X/REL_Y), `None` for keyboards and mice.
    touchpads: Vec<Option<Touchpad>>,
}

/// A touchpad reports finger position (ABS_X/ABS_Y) and BTN_TOOL_FINGER, and
/// is not a direct-input touchscreen. Returns its resolution (units per mm).
fn touchpad_resolution(dev: &Device) -> Option<(f32, f32)> {
    let abs = dev.supported_absolute_axes()?;
    if !(abs.contains(AbsoluteAxisCode::ABS_X) && abs.contains(AbsoluteAxisCode::ABS_Y)) {
        return None;
    }
    if dev.properties().contains(PropType::DIRECT) {
        return None; // touchscreen
    }
    if !dev.supported_keys().is_some_and(|k| k.contains(KeyCode::BTN_TOOL_FINGER)) {
        return None;
    }
    let mut res = (0.0f32, 0.0f32);
    let mut range = (0.0f32, 0.0f32);
    if let Ok(infos) = dev.get_absinfo() {
        for (axis, info) in infos {
            let r = info.resolution() as f32;
            let span = (info.maximum() - info.minimum()) as f32;
            if axis == AbsoluteAxisCode::ABS_X {
                res.0 = r;
                range.0 = span;
            } else if axis == AbsoluteAxisCode::ABS_Y {
                res.1 = r;
                range.1 = span;
            }
        }
    }
    // Some firmware reports no resolution; assume a 100 x 65 mm pad.
    if res.0 <= 0.0 {
        res.0 = (range.0 / 100.0).max(1.0);
    }
    if res.1 <= 0.0 {
        res.1 = (range.1 / 65.0).max(1.0);
    }
    Some(res)
}

impl EvdevInput {
    /// Enumerate input devices, keeping those that emit keys or relative
    /// motion. Returns an error if none are usable (no permission / none found).
    pub fn open() -> std::io::Result<Self> {
        let mut devices = Vec::new();
        let mut touchpads = Vec::new();
        for (path, mut dev) in evdev::enumerate() {
            // Keep keyboards and pointing devices; skip everything else.
            let has_keys = dev.supported_keys().is_some();
            let has_rel = dev.supported_relative_axes().is_some();
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
                        let tp = touchpad_resolution(&dev);
                        if let Some((rx, ry)) = tp {
                            eprintln!(
                                "[veil-host] evdev: {path:?} is a touchpad ({rx:.1} x {ry:.1} units/mm)"
                            );
                        }
                        touchpads.push(tp.map(|(rx, ry)| Touchpad::new(rx, ry)));
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
        Ok(Self { devices, touchpads })
    }
}

impl InputBackend for EvdevInput {
    fn name(&self) -> &'static str {
        "evdev"
    }

    fn run(self: Box<Self>, ctx: InputCtx) {
        let InputCtx {
            tx,
            running,
            host_stop,
            geom,
        } = ctx;
        let mut devices = self.devices;
        let mut touchpads = self.touchpads;
        eprintln!(
            "[veil-host] input thread started (evdev, {} device(s), event-driven)",
            devices.len()
        );

        // Virtual cursor, starts centred.
        let mut cx = (geom.comp_w.load(Ordering::Relaxed) / 2) as i32;
        let mut cy = (geom.comp_h.load(Ordering::Relaxed) / 2) as i32;

        // Ctrl/Alt held-state for the Ctrl+Alt+Fn VT-switch chord (see
        // handle_event). The exclusive grab below eats this chord before the
        // kernel's own VT switching ever sees it, so we emulate it ourselves.
        let mut ctrl_held = false;
        let mut alt_held = false;
        // Shift joins the chord for the compositor kill key (K3): a bare HOME
        // (Fn+Left on most laptops) must never tear the compositor down.
        let mut shift_held = false;

        // Poll the device fds directly instead of busy-polling with a sleep.
        // The kernel wakes us the instant an event lands, so we drain promptly
        // even when the rest of veil is CPU-saturated compositing a nested
        // stack — there's no sleep gap for a HOME press to fall into, which is
        // what made the kill key unreliable under load. fds are stable for the
        // device lifetime; -1 masks a device that has gone away (poll ignores it).
        let mut pfds: Vec<libc::pollfd> = devices
            .iter()
            .map(|d| libc::pollfd {
                fd: d.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();

        let mut was_active = true;

        while running.load(Ordering::Relaxed) {
            let active = crate::seat::session_active();
            if !active && was_active {
                eprintln!(
                    "[veil-host] evdev: session inactive -> releasing device grabs for VT switch"
                );
                for dev in &mut devices {
                    let _ = dev.ungrab();
                }
                for tp in touchpads.iter_mut().flatten() {
                    tp.reset();
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
                    let _ = tx.send(InputCmd::Key {
                        keycode: KEY_LEFTCTRL as u32,
                        mods: 0,
                        pressed: false,
                    });
                    let _ = tx.send(InputCmd::Key {
                        keycode: KEY_RIGHTCTRL as u32,
                        mods: 0,
                        pressed: false,
                    });
                    ctrl_held = false;
                }
                if alt_held {
                    let _ = tx.send(InputCmd::Key {
                        keycode: KEY_LEFTALT as u32,
                        mods: 0,
                        pressed: false,
                    });
                    let _ = tx.send(InputCmd::Key {
                        keycode: KEY_RIGHTALT as u32,
                        mods: 0,
                        pressed: false,
                    });
                    alt_held = false;
                }
                if shift_held {
                    for code in [KEY_LEFTSHIFT, KEY_RIGHTSHIFT] {
                        let _ = tx.send(InputCmd::Key {
                            keycode: code as u32,
                            mods: 0,
                            pressed: false,
                        });
                    }
                    shift_held = false;
                }
                was_active = true;
            }

            // 200 ms cap so we still notice `running` being cleared by another path.
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 200) };
            if n <= 0 {
                continue;
            } // 0 = timeout, <0 = EINTR/error → re-check running
            for (i, dev) in devices.iter_mut().enumerate() {
                let re = pfds[i].revents;
                pfds[i].revents = 0;
                if re & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    eprintln!("[veil-host] evdev: device {i} gone, dropping");
                    pfds[i].fd = -1; // poll skips negative fds
                    continue;
                }
                if re & libc::POLLIN == 0 {
                    continue;
                }

                let events = match dev.fetch_events() {
                    Ok(it) => it,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => {
                        eprintln!("[veil-host] evdev read error: {e}");
                        continue;
                    }
                };
                let mut moved = false;
                let mut tp_out: Vec<touchpad::Out> = Vec::new();
                let now = std::time::Instant::now();
                for ev in events {
                    // Touchpad-specific events (finger position, BTN_TOUCH,
                    // BTN_TOOL_*, SYN_REPORT) feed the touchpad state machine.
                    // The physical click (BTN_LEFT..) falls through below.
                    if let Some(tp) = touchpads[i].as_mut() {
                        let ours = match ev.destructure() {
                            EventSummary::Key(_, key, value) if value != 2 => {
                                tp.key(key.code(), value == 1, now)
                            }
                            EventSummary::AbsoluteAxis(_, axis, value) => {
                                // Non-position axes (MT slots, pressure) are
                                // ignored here but never mistaken for keys.
                                tp.abs(axis.0, value);
                                true
                            }
                            EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                                if active {
                                    tp.sync(&mut tp_out);
                                }
                                true
                            }
                            // Misc (scan time etc.): nothing to do.
                            EventSummary::Misc(..) => true,
                            _ => false,
                        };
                        if ours {
                            continue;
                        }
                    }
                    if active
                        && handle_event(
                            &tx,
                            ev,
                            &geom,
                            &mut cx,
                            &mut cy,
                            &running,
                            &host_stop,
                            &mut ctrl_held,
                            &mut alt_held,
                            &mut shift_held,
                        )
                    {
                        moved = true;
                    }
                }
                if active {
                    let comp_w = geom.comp_w.load(Ordering::Relaxed) as i32;
                    let comp_h = geom.comp_h.load(Ordering::Relaxed) as i32;
                    for o in tp_out {
                        match o {
                            touchpad::Out::Motion(dx, dy) => {
                                cx = (cx + dx).clamp(0, comp_w.max(1) - 1);
                                cy = (cy + dy).clamp(0, comp_h.max(1) - 1);
                                moved = true;
                            }
                            touchpad::Out::Scroll(v120) => {
                                let _ = tx.send(InputCmd::Scroll { v120 });
                            }
                            touchpad::Out::Tap => {
                                // Position first so the click lands where
                                // the pointer is.
                                send_motion(&tx, cx, cy, comp_w, comp_h);
                                for pressed in [true, false] {
                                    let _ = tx.send(InputCmd::PointerButton {
                                        button: 0x110,
                                        pressed,
                                    });
                                }
                            }
                        }
                    }
                    if moved {
                        send_motion(&tx, cx, cy, comp_w, comp_h);
                    }
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
    shift_held: &mut bool,
) -> bool {
    let mut moved = false;
    match ev.destructure() {
        EventSummary::Key(_, key, value) => {
            // value: 0 = release, 1 = press, 2 = repeat (Wayland drives repeat).
            if value == 2 {
                return false;
            }
            let pressed = value == 1;
            let code = key.code();

            if code == KEY_LEFTCTRL || code == KEY_RIGHTCTRL {
                *ctrl_held = pressed;
            }
            if code == KEY_LEFTALT || code == KEY_RIGHTALT {
                *alt_held = pressed;
            }
            if code == KEY_LEFTSHIFT || code == KEY_RIGHTSHIFT {
                *shift_held = pressed;
            }

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

            // Compositor kill chord: Ctrl+Alt+Shift+Home. Ctrl-C is forwarded
            // to the hosted app (it never reaches us on a TTY), so this is the
            // escape hatch: it tears veil down even if the guest wedges. It
            // must be a chord — a bare HOME (Fn+Left on most laptops) used to
            // kill the compositor from inside any app. The guest never sees it.
            if code == KEY_HOME && pressed && *ctrl_held && *alt_held && *shift_held {
                eprintln!("[veil-host] Ctrl+Alt+Shift+Home → shutdown");
                crate::vt::emergency_restore();
                running.store(false, Ordering::Relaxed);
                host_stop.store(true, Ordering::Relaxed);
                return false;
            }

            if is_button(key) {
                if pressed {
                    let _ = tx.send(InputCmd::PointerButton {
                        button: code as u32,
                        pressed: true,
                    });
                } else {
                    let _ = tx.send(InputCmd::PointerButton {
                        button: code as u32,
                        pressed: false,
                    });
                }
            } else {
                let _ = tx.send(InputCmd::Key {
                    keycode: code as u32,
                    mods: 0,
                    pressed,
                });
            }
        }
        EventSummary::RelativeAxis(_, axis, value) => {
            let comp_w = geom.comp_w.load(Ordering::Relaxed) as i32;
            let comp_h = geom.comp_h.load(Ordering::Relaxed) as i32;
            match axis {
                RelativeAxisCode::REL_X => {
                    let old = *cx;
                    *cx = (*cx + value).clamp(0, comp_w.max(1) - 1);
                    if *cx != old {
                        moved = true;
                    }
                }
                RelativeAxisCode::REL_Y => {
                    let old = *cy;
                    *cy = (*cy + value).clamp(0, comp_h.max(1) - 1);
                    if *cy != old {
                        moved = true;
                    }
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

/// evdev KEY_HOME (linux/input-event-codes.h) — last key of the kill chord.
const KEY_HOME: u16 = 102;

// linux/input-event-codes.h — modifier + function-key codes for the
// Ctrl+Alt+Fn VT-switch chord and the Ctrl+Alt+Shift+Home kill chord.
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
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
