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

pub mod cli;
pub mod detile;
pub mod font5x7;
pub mod idle_notify;
pub mod input;
pub mod input_backend;
pub mod ipc;
pub mod launcher;
pub mod layer_shell;
pub mod layout;
pub mod lockfile;
pub mod media;
pub mod palette;
pub mod output;
pub mod output_management;
pub mod powermenu;
pub mod seat;
pub mod server;
pub mod sink;
pub mod supervisor;
pub mod vt;
pub mod wallpaper;
pub mod wallpaper_chooser;

pub use input::InputCmd;
pub use input_backend::{InputBackend, InputCtx, InputGeometry};
pub use output::OutputBackend;
pub use sink::Frame;

/// Overrides for the shell plumbing. The defaults are what a real session
/// wants; tests and unusual setups set them instead of touching the process
/// environment.
#[derive(Clone, Default)]
pub struct ShellOptions {
    /// IPC socket path (default: `$XDG_RUNTIME_DIR/veil/ipc.sock`).
    pub ipc_socket: Option<std::path::PathBuf>,
    /// Binary the supervisor runs as `<exe> shell <name>` (default: this one).
    pub shell_exe: Option<std::path::PathBuf>,
    /// Layer-shell namespaces trusted without being supervisor children
    /// (`shell.trusted`): they may use the overlay layer and exclusive keyboard.
    pub trusted_namespaces: Vec<String>,
}

pub struct HostConfig {
    pub socket_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Sync the host clipboard with clients (see `VeilConfig::clipboard_sync`).
    pub clipboard_sync: bool,
    pub spawn: Option<Vec<String>>,
    pub wayland_debug: bool,
    pub keybinds: veil_config::Keybinds,
    pub background: [u8; 3],
    pub theme: veil_config::Theme,
    pub theme_name: veil_config::ThemeName,
    pub theme_mode: veil_config::ThemeMode,
    pub font: veil_config::FontSettings,
    pub bar: veil_config::BarConfig,
    pub notify: veil_config::NotifyConfig,
    pub shell: ShellOptions,
}

/// Signal handle returned by [`Host::spawn`]; flip via [`Host::stop`]
/// from any thread to request a clean shutdown of the compositor loop.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc as StdArc;

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            socket_name: "wayland-veil-0".to_string(),
            width: 1280,
            height: 720,
            fps: 60,
            clipboard_sync: true,
            spawn: None,
            wayland_debug: false,
            keybinds: veil_config::Keybinds::default(),
            background: [0x8c, 0x8c, 0x8c],
            theme: veil_config::Theme::for_name(veil_config::ThemeName::Default),
            theme_name: veil_config::ThemeName::Default,
            theme_mode: veil_config::ThemeMode::Dark,
            font: veil_config::FontSettings::default(),
            bar: veil_config::BarConfig::default(),
            notify: veil_config::NotifyConfig::default(),
            shell: ShellOptions::default(),
        }
    }
}

pub struct Host {
    frames: mpsc::Receiver<Frame>,
    input: mpsc::Sender<InputCmd>,
    stop: StdArc<AtomicBool>,
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
                    config.clipboard_sync,
                    config.spawn,
                    config.wayland_debug,
                    frame_tx,
                    input_rx,
                    stop_t,
                    config.keybinds,
                    config.background,
                    config.theme,
                    config.theme_name,
                    config.theme_mode,
                    config.font,
                    config.bar,
                    config.notify,
                    config.shell,
                ) {
                    eprintln!("[veil-host] server exited: {e}");
                }
            })?;

        Ok(Self {
            frames: frame_rx,
            input: input_tx,
            stop,
            _thread: handle,
        })
    }

    pub fn frames(&self) -> &mpsc::Receiver<Frame> {
        &self.frames
    }

    pub fn send_input(&self, cmd: InputCmd) -> Result<(), mpsc::SendError<InputCmd>> {
        self.input.send(cmd)
    }

    /// Ask the compositor loop to exit on its next iteration.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Clone the input sender for use on another thread.
    pub fn input_sender(&self) -> mpsc::Sender<InputCmd> {
        self.input.clone()
    }

    /// Clone the stop flag for use on another thread.
    pub fn stop_flag(&self) -> StdArc<AtomicBool> {
        self.stop.clone()
    }
}
