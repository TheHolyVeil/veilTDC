//! Supervised desktop bar for the Veil window manager.

pub mod bar;
pub mod poll;
pub mod registry;
pub mod util;
pub mod widgets;

pub use bar::BarLayout;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use smithay_client_toolkit::shell::wlr_layer::{Anchor, Layer};
use veil_ipc::{BarSettings, ClientMessage, FrameBuffer, ServerMessage};
use veil_ui::{Action, LayerShellClient, SurfacePosition, Theme, UiInputEvent};

/// How long to wait for the compositor's `hello` before drawing the first
/// frame with defaults.
const HELLO_WAIT: Duration = Duration::from_millis(1000);

struct Ipc {
    stream: UnixStream,
    frames: FrameBuffer,
    next_id: u64,
}

impl Ipc {
    fn send(&mut self, msg: &ClientMessage) {
        if let Ok(bytes) = veil_ipc::encode(msg) {
            let _ = self.stream.write_all(&bytes);
        }
    }

    fn send_cmd(&mut self, cmd: &str, args: Value) {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&ClientMessage::Cmd { id, cmd: cmd.into(), args });
    }
}

/// The state of the running bar.
struct App {
    bar: BarLayout,
    client: LayerShellClient,
    ipc: Option<Ipc>,
    /// Palette: `shown` is what is on screen right now; a `palette` event
    /// starts a crossfade from it to the new tokens.
    shown: veil_palette::Tokens,
    fade: Option<(veil_palette::Tokens, veil_palette::Tokens, Instant)>,
    needs_redraw: bool,
    /// (at_top, height) the surface was last asked for.
    geometry: (bool, u32),
}

/// Anchor, size and exclusive zone for a bar on the top or bottom edge.
fn surface_geometry(at_top: bool, height: u32) -> (Anchor, (u32, u32), i32) {
    let edge = if at_top { Anchor::TOP } else { Anchor::BOTTOM };
    (edge | Anchor::LEFT | Anchor::RIGHT, (0, height), height as i32)
}

impl App {
    /// The bar's edge or height changed (config reload, font size): tell the
    /// compositor so the surface and its exclusive zone follow.
    fn apply_geometry(&mut self) {
        let now = (self.bar.at_top(), self.bar.height());
        if now != self.geometry {
            tracing::info!("bar: geometry {:?} -> {:?}", self.geometry, now);
            let (anchor, size, zone) = surface_geometry(now.0, now.1);
            self.client.reconfigure(anchor, size, zone);
            self.geometry = now;
            self.needs_redraw = true;
        }
    }

    fn apply_config(&mut self, config: &Value) {
        if let Some(font) = config.get("font") {
            let fc = veil_text::FontConfig::from_json(font);
            if self.bar.set_font(&fc) {
                tracing::info!("bar: font reloaded to {:?}", self.bar.text.resolved_paths());
                self.needs_redraw = true;
            }
        }
        if let Some(b) = config.get("bar") {
            self.bar.apply_settings(BarSettings::from_json(b));
            self.needs_redraw = true;
        }
        self.apply_geometry();
    }

    fn handle_message(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Event { topic, data } => {
                tracing::debug!("bar: event {topic}");
                match topic.as_str() {
                    "workspaces" => {
                        let monitors = data
                            .get("monitors")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .map(|m| {
                                        let active = m.get("active").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                                        let occupied = m
                                            .get("occupied")
                                            .and_then(|v| v.as_array())
                                            .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as usize)).collect())
                                            .unwrap_or_default();
                                        (active, occupied)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.bar.on_workspaces(monitors);
                        self.needs_redraw = true;
                    }
                    "palette" => self.fade = Some((self.shown, tokens_from_json(&data), Instant::now())),
                    "focus" => {
                        let title = data.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                        self.bar.on_focus(title);
                        self.needs_redraw = true;
                    }
                    "volume" => {
                        let vol = data.get("volume").and_then(|v| v.as_u64()).unwrap_or(50).min(100) as u8;
                        let muted = data.get("muted").and_then(|v| v.as_bool()).unwrap_or(false);
                        self.bar.on_volume(vol, muted);
                        self.needs_redraw = true;
                    }
                    "notifications" => {
                        let count = data.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        let dnd = data.get("dnd").and_then(|v| v.as_bool()).unwrap_or(false);
                        self.bar.on_notifications(count, dnd);
                        self.needs_redraw = true;
                    }
                    "config" => self.apply_config(&data),
                    _ => {}
                }
            }
            ServerMessage::Hello { palette, config, .. } => {
                // First palette: no fade, there is nothing to fade from.
                if palette.is_object() {
                    self.shown = tokens_from_json(&palette);
                    self.bar.theme = Theme::from_palette(&self.shown);
                    self.fade = None;
                    self.needs_redraw = true;
                }
                self.apply_config(&config);
            }
            ServerMessage::Reply { ok: false, error, .. } => {
                tracing::info!("bar: compositor refused a command: {}", error.unwrap_or_default());
            }
            _ => {}
        }
    }

    fn pump_ipc(&mut self) {
        let mut incoming = Vec::new();
        if let Some(ipc) = &mut self.ipc {
            let mut buf = [0u8; 4096];
            loop {
                match ipc.stream.read(&mut buf) {
                    Ok(0) => {
                        tracing::warn!("IPC socket closed by compositor");
                        break;
                    }
                    Ok(n) => {
                        let _ = ipc.frames.push(&buf[..n]);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!("IPC read error: {e}");
                        break;
                    }
                }
            }
            while let Ok(Some(msg)) = ipc.frames.next_frame::<ServerMessage>() {
                incoming.push(msg);
            }
        }
        for msg in incoming {
            self.handle_message(msg);
        }
    }

    /// Clicks and scrolls from the pointer, routed to the widget under it.
    fn pump_input(&mut self) {
        let mut actions = Vec::new();
        while let Ok(ev) = self.client.input_rx.try_recv() {
            match ev {
                UiInputEvent::Press { x, output, .. } => actions.extend(self.bar.on_press(&output, x as f32)),
                UiInputEvent::Scroll { x, dy, output, .. } => {
                    actions.extend(self.bar.on_scroll(&output, x as f32, dy as f32))
                }
                UiInputEvent::Motion { .. } => {}
            }
        }
        for a in actions {
            self.run_action(a);
        }
    }

    fn run_action(&mut self, action: Action) {
        match action {
            Action::Exec(cmd) => spawn_detached(&cmd),
            Action::Ipc { cmd, args } => match &mut self.ipc {
                Some(ipc) => ipc.send_cmd(&cmd, args),
                None => tracing::warn!("bar: no compositor connection for '{cmd}'"),
            },
        }
        self.needs_redraw = true;
    }

    /// Step the palette crossfade; returns true while it is running.
    fn step_fade(&mut self) -> bool {
        let Some((from, to, started)) = self.fade else {
            return false;
        };
        let elapsed = started.elapsed().as_millis() as u64;
        if elapsed >= veil_palette::CROSSFADE_MS {
            self.shown = to;
            self.fade = None;
        } else {
            self.shown = veil_palette::mix(&from, &to, veil_palette::crossfade_progress(elapsed));
        }
        self.bar.theme = Theme::from_palette(&self.shown);
        self.needs_redraw = true;
        self.fade.is_some()
    }
}

/// Run a command line detached (`sh -c`), reaping it so it never lingers as
/// a zombie.
fn spawn_detached(cmd: &str) {
    match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!("bar: could not run '{cmd}': {e}"),
    }
}

/// Connect, say hello and subscribe, then wait briefly for the compositor's
/// reply so the first surface already has the right height, edge and font.
/// Everything that arrives before the hello is handed back in order.
fn connect_ipc() -> (Option<Ipc>, Vec<ServerMessage>) {
    let socket_path = std::env::var("VEIL_IPC_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| veil_ipc::socket_path());
    let stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Could not connect to Veil IPC socket at {}: {e}", socket_path.display());
            return (None, Vec::new());
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut ipc = Ipc { stream, frames: FrameBuffer::new(), next_id: 0 };
    ipc.send(&ClientMessage::Hello { version: veil_ipc::PROTOCOL_VERSION, name: "bar".into() });
    ipc.send(&ClientMessage::Subscribe {
        topics: ["workspaces", "focus", "volume", "notifications", "palette", "config"]
            .iter()
            .map(|t| t.to_string())
            .collect(),
    });

    let mut early = Vec::new();
    let start = Instant::now();
    let mut buf = [0u8; 4096];
    'wait: while start.elapsed() < HELLO_WAIT {
        match ipc.stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let _ = ipc.frames.push(&buf[..n]);
            }
            Err(ref e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
        while let Ok(Some(msg)) = ipc.frames.next_frame::<ServerMessage>() {
            let is_hello = matches!(msg, ServerMessage::Hello { .. });
            early.push(msg);
            if is_hello {
                break 'wait;
            }
        }
    }
    let _ = ipc.stream.set_read_timeout(None);
    let _ = ipc.stream.set_nonblocking(true);
    (Some(ipc), early)
}

/// Run the bar main loop.
pub fn run_bar() -> Result<(), String> {
    // Reached via `veil shell bar` (no standalone binary any more), so the
    // subscriber has to be set up here or every log line is dropped.
    // stderr, not the default stdout: the session log only captures stderr.
    let filter = std::env::var("VEIL_LOG")
        .ok()
        .and_then(|f| tracing_subscriber::EnvFilter::try_new(f).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init();
    tracing::info!("Starting veil-bar...");

    // 1. IPC first: the hello carries the bar settings and the font.
    let (mut ipc, early) = connect_ipc();

    let mut bar = BarLayout::default();
    let mut shown = veil_palette::Tokens::default();
    let mut config = Value::Null;
    for msg in &early {
        if let ServerMessage::Hello { palette, config: c, .. } = msg {
            if palette.is_object() {
                shown = tokens_from_json(palette);
            }
            config = c.clone();
        }
    }
    if let Some(font) = config.get("font") {
        bar.set_font(&veil_text::FontConfig::from_json(font));
    }
    if let Some(b) = config.get("bar") {
        bar.apply_settings(BarSettings::from_json(b));
    }
    bar.theme = Theme::from_palette(&shown);

    // 2. The layer-shell surface, already the right size and on the right edge.
    let geometry = (bar.at_top(), bar.height());
    let client = LayerShellClient::new(
        "veil-bar",
        Layer::Top,
        if geometry.0 { SurfacePosition::Top } else { SurfacePosition::Bottom },
        geometry.1,
        true,
    )?;

    let mut app = App { bar, client, ipc: ipc.take(), shown, fade: None, needs_redraw: true, geometry };
    // Anything that arrived after the hello but before the surface existed.
    for msg in early.into_iter().filter(|m| !matches!(m, ServerMessage::Hello { .. })) {
        app.handle_message(msg);
    }

    let mut last_tick = Instant::now();
    let mut last_ping = Instant::now();

    // Initial render
    app.client.present_for_each(|out, w, h| app.bar.render_for(out, w, h));

    loop {
        // Dispatch Wayland events
        let t_dispatch = Instant::now();
        if let Err(e) = app.client.dispatch() {
            tracing::error!("Wayland dispatch error: {e}");
            break;
        }
        if t_dispatch.elapsed() > Duration::from_millis(40) {
            tracing::debug!("bar: slow dispatch {:?}", t_dispatch.elapsed());
        }

        app.pump_ipc();
        app.pump_input();

        // Heartbeat: the protocol is client Ping -> server Pong, so the
        // supervisor's liveness check (10 s) only holds if we keep pinging.
        let now = Instant::now();
        if now.duration_since(last_ping) >= Duration::from_secs(3) {
            last_ping = now;
            if let Some(ipc) = &mut app.ipc {
                ipc.send(&ClientMessage::Ping);
            }
        }

        // Widget updates
        if now.duration_since(last_tick) >= Duration::from_millis(500) {
            last_tick = now;
            if app.bar.update(now) {
                app.needs_redraw = true;
            }
        }

        // Compositor resized or configured a surface (new monitor, mode
        // change): the committed buffer is the old size until we redraw.
        if app.client.take_reconfigured() {
            app.needs_redraw = true;
        }

        let fading = app.step_fade();

        if app.needs_redraw {
            app.needs_redraw = false;
            tracing::debug!("bar: presenting");
            let bar = &mut app.bar;
            app.client.present_for_each(|out, w, h| bar.render_for(out, w, h));
        }

        // Sleep to cap wakeups; ~60 fps while a palette fade is running.
        std::thread::sleep(Duration::from_millis(if fading { 16 } else { 50 }));
    }

    Ok(())
}

/// A `palette` payload (flat `{"bg": "#rrggbb", ...}`) as tokens. Anything
/// missing or unparsable keeps its default, so a partial palette still works.
fn tokens_from_json(v: &Value) -> veil_palette::Tokens {
    veil_palette::Tokens::from_hex_lookup(|k| v.get(k).and_then(|x| x.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_and_bottom_geometry() {
        let (a, size, zone) = surface_geometry(true, 40);
        assert_eq!(a, Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        assert_eq!((size, zone), ((0, 40), 40));
        let (a, ..) = surface_geometry(false, 40);
        assert_eq!(a, Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    }
}
