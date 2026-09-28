//! Terminal input backend — reads crossterm events and maps them to evdev
//! keycodes / pointer coordinates. Used under a WM or over SSH.

use super::{InputBackend, InputCtx};
use crate::input::InputCmd;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::Duration;

#[derive(Default)]
pub struct CrosstermInput;

impl CrosstermInput {
    pub fn new() -> Self {
        CrosstermInput
    }
}

impl InputBackend for CrosstermInput {
    fn name(&self) -> &'static str {
        "crossterm"
    }

    fn run(self: Box<Self>, ctx: InputCtx) {
        let InputCtx {
            tx,
            running,
            host_stop,
            geom,
        } = ctx;
        eprintln!("[veil-host] input thread started (crossterm)");

        let mut tick = 0u32;
        let mut btns = [false; 3];
        let mut cur_cols = geom.cols.load(Ordering::Relaxed);
        let mut cur_rows = geom.rows.load(Ordering::Relaxed);
        let mut last_event_time = std::time::Instant::now();

        // `event::poll` blocks up to this long but returns *immediately* the
        // instant a real event is ready — it does not add latency to key
        // presses or mouse moves. What it *does* govern is how often we wake
        // up to find nothing waiting. This was 1ms, i.e. up to 1000
        // wake+syscall+timeout cycles per second even while totally idle —
        // a full CPU core spent doing nothing, directly competing with the
        // compositor's render thread for cycles on weak hardware. 50ms still
        // keeps idle-log ticks reasonably granular, at 5% the wakeups.
        const IDLE_POLL: Duration = Duration::from_millis(50);

        while running.load(Ordering::Relaxed) {
            match event::poll(IDLE_POLL) {
                Ok(true) => {
                    last_event_time = std::time::Instant::now();
                }
                Ok(false) => {
                    tick = tick.wrapping_add(1);
                    let idle_secs = last_event_time.elapsed().as_secs();
                    // 600 ticks × 50ms = 30s, same cadence as before the
                    // poll interval changed (was 30_000 ticks × 1ms).
                    if idle_secs > 0 && tick.is_multiple_of(600) {
                        eprintln!("[veil-host] input idle {}s", idle_secs);
                    }
                    continue;
                }
                Err(e) => {
                    eprintln!("[veil-host] poll error: {e}");
                    continue;
                }
            }

            match event::read() {
                Ok(ev) => {
                    tracing::debug!("event: {ev:?}");
                    match ev {
                        Event::Key(k) if is_ctrl_c(&k) || is_kill_key(&k) => {
                            eprintln!(
                                "[veil-host] {} → shutdown",
                                if is_kill_key(&k) { "HOME" } else { "ctrl-c" }
                            );
                            crate::vt::emergency_restore();
                            running.store(false, Ordering::Relaxed);
                            host_stop.store(true, Ordering::Relaxed);
                            break;
                        }
                        Event::Key(k) => forward_key(&tx, k),
                        Event::Mouse(m) => {
                            let cw = geom.comp_w.load(Ordering::Relaxed);
                            let ch = geom.comp_h.load(Ordering::Relaxed);
                            forward_mouse(&tx, m, cur_cols, cur_rows, cw, ch, &mut btns);
                        }
                        Event::Resize(new_cols, new_rows) => {
                            cur_cols = new_cols;
                            cur_rows = new_rows;
                            geom.cols.store(new_cols, Ordering::Relaxed);
                            geom.rows.store(new_rows, Ordering::Relaxed);
                            let (nw, nh) = super::term_pixel_size()
                                .unwrap_or((new_cols as u32 * 8, new_rows as u32 * 16));
                            geom.comp_w.store(nw, Ordering::Relaxed);
                            geom.comp_h.store(nh, Ordering::Relaxed);
                            let _ = tx.send(InputCmd::Resize {
                                width: nw,
                                height: nh,
                            });
                        }
                        _ => {}
                    }
                }
                Err(e) => eprintln!("[veil-host] read error: {e}"),
            }
        }
        eprintln!("[veil-host] input thread exited (crossterm)");
    }
}

// ─── mouse forwarding ─────────────────────────────────────────────────────────

fn forward_mouse(
    tx: &Sender<InputCmd>,
    m: MouseEvent,
    cols: u16,
    rows: u16,
    comp_w: u32,
    comp_h: u32,
    btns: &mut [bool; 3],
) {
    // Map terminal cell coords → compositor pixel coords, aiming at the
    // CENTER of the cell so clicks land where users visually aim.
    let x = ((m.column as u32 * 2 + 1) * comp_w / (2 * cols.max(1) as u32)) as i32;
    let y = ((m.row as u32 * 2 + 1) * comp_h / (2 * rows.max(1) as u32)) as i32;

    match m.kind {
        MouseEventKind::Moved | MouseEventKind::Drag(_) => {
            let _ = tx.send(InputCmd::PointerMotionAbs {
                x,
                y,
                width: comp_w,
                height: comp_h,
            });
        }
        MouseEventKind::Down(b) => {
            let idx = btn_idx(b);
            if btns[idx] {
                return;
            }
            btns[idx] = true;
            let _ = tx.send(InputCmd::PointerMotionAbs {
                x,
                y,
                width: comp_w,
                height: comp_h,
            });
            let _ = tx.send(InputCmd::PointerButton {
                button: btn_code(b),
                pressed: true,
            });
        }
        MouseEventKind::Up(b) => {
            let idx = btn_idx(b);
            if !btns[idx] {
                return;
            }
            btns[idx] = false;
            let _ = tx.send(InputCmd::PointerButton {
                button: btn_code(b),
                pressed: false,
            });
        }
        MouseEventKind::ScrollDown => {
            let _ = tx.send(InputCmd::Scroll { v120: 120 });
        }
        MouseEventKind::ScrollUp => {
            let _ = tx.send(InputCmd::Scroll { v120: -120 });
        }
        _ => {}
    }
}

// evdev BTN_* codes from linux/input-event-codes.h
fn btn_code(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
    }
}

fn btn_idx(b: MouseButton) -> usize {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
    }
}

// ─── key forwarding ───────────────────────────────────────────────────────────

fn is_ctrl_c(k: &KeyEvent) -> bool {
    matches!(k.code, KeyCode::Char('c')) && k.modifiers.contains(KeyModifiers::CONTROL)
}

/// HOME is the compositor kill key — an escape hatch that always tears veil
/// down even if the guest app wedges. Only fires on press, not release/repeat,
/// so the shutdown log isn't spammed. The guest never receives HOME.
fn is_kill_key(k: &KeyEvent) -> bool {
    matches!(k.code, KeyCode::Home) && k.kind != KeyEventKind::Release
}

/// Map a crossterm KeyEvent → evdev keycodes and forward it as a discrete
/// press+release "tap".
///
/// Why not track hold/release: terminals don't reliably report key *release*
/// (only the kitty keyboard protocol does, and not every terminal supports it).
/// Depending on release meant a tapped key stuck "held" in the client, which
/// then repeated forever via wl_keyboard.repeat_info — the runaway-repeat bug.
/// Emitting each keystroke as its own press+release means nothing is ever left
/// held: holding a key still repeats, but driven by the terminal's own
/// auto-repeat (it resends Press/Repeat events), never by a stuck key. Explicit
/// Release events, when a terminal does send them, are redundant → ignored.
fn forward_key(tx: &Sender<InputCmd>, k: KeyEvent) {
    if k.kind == KeyEventKind::Release {
        return;
    }
    let keycode = match keycode_for(k.code) {
        Some(c) => c,
        None => return,
    };
    let shift = needs_shift(k.code) || k.modifiers.contains(KeyModifiers::SHIFT);
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);

    // Modifiers down, key down, key up, modifiers up — a self-contained chord.
    if shift {
        let _ = tx.send(InputCmd::Key {
            keycode: KEY_LEFTSHIFT,
            mods: 0,
            pressed: true,
        });
    }
    if ctrl {
        let _ = tx.send(InputCmd::Key {
            keycode: KEY_LEFTCTRL,
            mods: 0,
            pressed: true,
        });
    }
    if alt {
        let _ = tx.send(InputCmd::Key {
            keycode: KEY_LEFTALT,
            mods: 0,
            pressed: true,
        });
    }
    let _ = tx.send(InputCmd::Key {
        keycode,
        mods: 0,
        pressed: true,
    });
    let _ = tx.send(InputCmd::Key {
        keycode,
        mods: 0,
        pressed: false,
    });
    if alt {
        let _ = tx.send(InputCmd::Key {
            keycode: KEY_LEFTALT,
            mods: 0,
            pressed: false,
        });
    }
    if ctrl {
        let _ = tx.send(InputCmd::Key {
            keycode: KEY_LEFTCTRL,
            mods: 0,
            pressed: false,
        });
    }
    if shift {
        let _ = tx.send(InputCmd::Key {
            keycode: KEY_LEFTSHIFT,
            mods: 0,
            pressed: false,
        });
    }
}

fn needs_shift(code: KeyCode) -> bool {
    match code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => true,
        KeyCode::Char(c) => matches!(
            c,
            '!' | '@'
                | '#'
                | '$'
                | '%'
                | '^'
                | '&'
                | '*'
                | '('
                | ')'
                | '_'
                | '+'
                | '{'
                | '}'
                | '|'
                | ':'
                | '"'
                | '<'
                | '>'
                | '?'
                | '~'
        ),
        _ => false,
    }
}

// evdev keycodes (linux/input-event-codes.h).
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;

fn keycode_for(code: KeyCode) -> Option<u32> {
    Some(match code {
        KeyCode::Char(c) => match c.to_ascii_lowercase() {
            'a' => 30,
            'b' => 48,
            'c' => 46,
            'd' => 32,
            'e' => 18,
            'f' => 33,
            'g' => 34,
            'h' => 35,
            'i' => 23,
            'j' => 36,
            'k' => 37,
            'l' => 38,
            'm' => 50,
            'n' => 49,
            'o' => 24,
            'p' => 25,
            'q' => 16,
            'r' => 19,
            's' => 31,
            't' => 20,
            'u' => 22,
            'v' => 47,
            'w' => 17,
            'x' => 45,
            'y' => 21,
            'z' => 44,
            '1' | '!' => 2,
            '2' | '@' => 3,
            '3' | '#' => 4,
            '4' | '$' => 5,
            '5' | '%' => 6,
            '6' | '^' => 7,
            '7' | '&' => 8,
            '8' | '*' => 9,
            '9' | '(' => 10,
            '0' | ')' => 11,
            '-' | '_' => 12,
            '=' | '+' => 13,
            '[' | '{' => 26,
            ']' | '}' => 27,
            '\\' | '|' => 43,
            ';' | ':' => 39,
            '\'' | '"' => 40,
            ',' | '<' => 51,
            '.' | '>' => 52,
            '/' | '?' => 53,
            '`' | '~' => 41,
            ' ' => 57,
            _ => return None,
        },
        KeyCode::Enter => 28,
        KeyCode::Esc => 1,
        KeyCode::Backspace => 14,
        KeyCode::Tab => 15,
        KeyCode::Left => 105,
        KeyCode::Right => 106,
        KeyCode::Up => 103,
        KeyCode::Down => 108,
        KeyCode::Home => 102,
        KeyCode::End => 107,
        KeyCode::PageUp => 104,
        KeyCode::PageDown => 109,
        KeyCode::Insert => 110,
        KeyCode::Delete => 111,
        KeyCode::F(n) => match n {
            1..=10 => 58 + n as u32,
            11 => 87,
            12 => 88,
            _ => return None,
        },
        _ => return None,
    })
}
