//! veil-host — nested headless Wayland compositor.
//!
//! One client, fullscreen, no output backend. The hosted app commits
//! buffers; we hand them out as RGBA via `frames()`. Input flows the
//! other way: caller pushes `InputCmd`s, we route them through the seat.
//!
//! Runs alongside any other compositor by creating its own
//! `WAYLAND_DISPLAY` socket (default: `wayland-veil-0`).

use std::sync::mpsc;
use std::thread;

pub mod detile;
pub mod font5x7;
pub mod input;
pub mod input_backend;
pub mod launcher;
pub mod layout;
pub mod powermenu;
pub mod lockfile;
pub mod server;
pub mod sink;
pub mod output;
pub mod seat;
pub mod vt;

pub use input::InputCmd;
pub use input_backend::{InputBackend, InputCtx, InputGeometry};
pub use sink::Frame;
pub use output::OutputBackend;

pub struct HostConfig {
    pub socket_name:   String,
    pub width:         u32,
    pub height:        u32,
    pub fps:           u32,
    pub spawn:         Option<Vec<String>>,
    pub wayland_debug: bool,
    pub keybinds:      veil_config::Keybinds,
    pub background:    [u8; 3],
    pub theme:         veil_config::Theme,
    pub bar:           veil_config::BarConfig,
}

/// Signal handle returned by [`Host::spawn`]; flip via [`Host::stop`]
/// from any thread to request a clean shutdown of the compositor loop.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc as StdArc;

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            socket_name:   "wayland-veil-0".to_string(),
            width:         1280,
            height:        720,
            fps:           60,
            spawn:         None,
            wayland_debug: false,
            keybinds:      veil_config::Keybinds::default(),
            background:    [0x8c, 0x8c, 0x8c],
            theme:         veil_config::Theme::for_name(veil_config::ThemeName::Default),
            bar:           veil_config::BarConfig::default(),
        }
    }
}

pub struct Host {
    frames: mpsc::Receiver<Frame>,
    input:  mpsc::Sender<InputCmd>,
    stop:   StdArc<AtomicBool>,
    _thread: thread::JoinHandle<()>,
}

impl Host {
    pub fn spawn(config: HostConfig) -> std::io::Result<Self> {
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>();
        let (input_tx, input_rx) = mpsc::channel::<InputCmd>();
        let stop = StdArc::new(AtomicBool::new(false));
        let stop_t = stop.clone();

        let handle = thread::Builder::new()
            .name("veil-host".into())
            .spawn(move || {
                if let Err(e) = server::run(
                    &config.socket_name,
                    config.width,
                    config.height,
                    config.fps,
                    config.spawn,
                    config.wayland_debug,
                    frame_tx,
                    input_rx,
                    stop_t,
                    config.keybinds,
                    config.background,
                    config.theme,
                    config.bar,
                ) {
                    eprintln!("[veil-host] server exited: {e}");
                }
            })?;

        Ok(Self { frames: frame_rx, input: input_tx, stop, _thread: handle })
    }

    pub fn frames(&self) -> &mpsc::Receiver<Frame> { &self.frames }

    pub fn send_input(&self, cmd: InputCmd) -> Result<(), mpsc::SendError<InputCmd>> {
        self.input.send(cmd)
    }

    /// Ask the compositor loop to exit on its next iteration.
    pub fn stop(&self) { self.stop.store(true, Ordering::Relaxed); }

    /// Clone the input sender for use on another thread.
    pub fn input_sender(&self) -> mpsc::Sender<InputCmd> { self.input.clone() }

    /// Clone the stop flag for use on another thread.
    pub fn stop_flag(&self) -> StdArc<AtomicBool> { self.stop.clone() }
}
