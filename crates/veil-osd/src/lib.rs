//! Supervised on-screen display for the Veil desktop shell (`veil shell osd`).
//!
//! The compositor sends `osd` events (volume and brightness levels, config
//! errors, "component failed" notices). This process shows each as a small
//! pill at the bottom of one output and then disappears: the layer surface
//! exists only while something is showing, so an idle OSD costs one wakeup per
//! heartbeat. It never talks to PipeWire or sysfs itself; the compositor's
//! media worker already does, and its answer arrives as the event.
//!
//! While no `osd` subscriber is connected (starting, restarting, crashed) the
//! compositor draws its built-in OSD instead, so a key press is never silent.

pub mod client;
pub mod model;
pub mod render;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use client::{Client, Incoming};
use model::Model;
use serde_json::Value;
use veil_ipc::osd::OsdEvent;
use veil_palette::Tokens;
use veil_ui::{
    Anchor, FontConfig, KeyboardInteractivity, Layer, LayerOptions, LayerShellClient, Metrics, OutputFilter, TextSystem, Theme,
};

/// Entry point for `veil shell osd`.
pub fn run_osd() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init().ok();

    let socket = std::env::var("VEIL_IPC_SOCKET").map(PathBuf::from).unwrap_or_else(|_| veil_ipc::socket_path());
    let mut app = App::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        match Client::connect(&socket, Instant::now()) {
            Ok(client) => {
                backoff = Duration::from_secs(1);
                let why = app.session(client);
                tracing::info!("osd: lost the compositor ({why}); reconnecting");
                app.hide();
            }
            Err(e) => tracing::debug!("osd: no compositor at {} ({e})", socket.display()),
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// A live layer surface and the output it was made for.
struct Shown {
    client: LayerShellClient,
    output: String,
}

struct App {
    model: Model,
    ts: TextSystem,
    theme: Theme,
    metrics: Metrics,
    font: FontConfig,
    surface: Option<Shown>,
    /// Alpha of the last frame committed; `None` means redraw regardless.
    last_alpha: Option<f32>,
    /// When the last frame was committed, to hold fades to one frame per
    /// `model::FRAME` even though Wayland events wake the loop more often.
    last_draw: Option<Instant>,
}

impl App {
    fn new() -> Self {
        let font = FontConfig::default();
        App {
            model: Model::default(),
            ts: TextSystem::new(&font),
            theme: Theme::default(),
            metrics: Metrics::from_font_size(font.size),
            font,
            surface: None,
            last_alpha: None,
            last_draw: None,
        }
    }

    /// Run until the connection drops; the reason is returned.
    fn session(&mut self, mut client: Client) -> String {
        loop {
            let now = Instant::now();
            if let Err(e) = client.ping_if_due(now) {
                return e.to_string();
            }
            // Font discovery finished: the UI font may have changed.
            if self.ts.poll() {
                self.prewarm();
            }
            if self.model.expire(now) {
                self.hide();
            }

            let mut wait = client.next_ping().saturating_duration_since(now);
            if let Some(w) = self.model.next_wake(now) {
                wait = wait.min(w);
            }
            let mut fds = vec![libc::pollfd { fd: client.fd(), events: libc::POLLIN, revents: 0 }];
            if let Some(s) = &self.surface {
                fds.push(libc::pollfd { fd: s.client.wayland_fd(), events: libc::POLLIN, revents: 0 });
            }
            // SAFETY: `fds` is a valid, live slice of `pollfd` for the call.
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, poll_timeout_ms(wait)) };

            if fds[0].revents != 0 {
                match client.read() {
                    Ok(msgs) => msgs.into_iter().for_each(|m| self.handle(m, Instant::now())),
                    Err(e) => return e.to_string(),
                }
            }
            if let Some(s) = &mut self.surface {
                if let Err(e) = s.client.dispatch() {
                    tracing::warn!("osd: wayland: {e}; dropping the surface");
                    self.hide();
                }
            }
            self.draw(Instant::now());
        }
    }

    fn handle(&mut self, msg: Incoming, now: Instant) {
        match msg {
            Incoming::Hello { palette, config } => {
                self.apply_palette(&palette);
                self.apply_config(&config);
                self.prewarm();
            }
            Incoming::Palette(p) => {
                self.apply_palette(&p);
                self.last_alpha = None;
            }
            Incoming::Config(c) => self.apply_config(&c),
            Incoming::Osd(ev) => self.show(ev, now),
        }
    }

    fn show(&mut self, ev: OsdEvent, now: Instant) {
        let output = ev.output.clone();
        if self.surface.as_ref().is_some_and(|s| s.output != output) {
            self.surface = None;
        }
        self.model.show(sanitised(ev), now);
        self.last_alpha = None;
        if self.surface.is_none() {
            self.surface = self.open_surface(&output);
            if self.surface.is_none() {
                self.model.clear();
            }
        }
    }

    fn open_surface(&self, output: &str) -> Option<Shown> {
        let g = render::geometry(&self.metrics);
        let opts = |filter| LayerOptions {
            namespace: "veil-osd".into(),
            layer: Layer::Overlay,
            // Anchored to the bottom edge only, so it centres horizontally.
            anchor: Anchor::BOTTOM,
            size: (g.width, g.height),
            exclusive_zone: 0,
            keyboard: KeyboardInteractivity::None,
            margin: (0, 0, g.margin_bottom, 0),
            output: filter,
        };
        let named = if output.is_empty() { OutputFilter::First } else { OutputFilter::Named(output.to_string()) };
        let mut client = match LayerShellClient::with_options(opts(named), None) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("osd: no layer surface: {e}");
                return None;
            }
        };
        let _ = client.dispatch();
        // The monitor the event named may be gone (unplugged): use the first.
        if client.output_names().is_empty() && !output.is_empty() {
            client = LayerShellClient::with_options(opts(OutputFilter::First), None).ok()?;
            let _ = client.dispatch();
        }
        Some(Shown { client, output: output.to_string() })
    }

    /// Drop the surface (and with it the Wayland connection) and forget what
    /// was showing. Idle cost is then the IPC heartbeat only.
    fn hide(&mut self) {
        self.surface = None;
        self.model.clear();
        self.last_alpha = None;
        self.last_draw = None;
    }

    fn draw(&mut self, now: Instant) {
        let App { surface, model, ts, theme, metrics, last_alpha, last_draw, .. } = self;
        let (Some(shown), Some(ev)) = (surface.as_mut(), model.current().cloned()) else {
            return;
        };
        let reconfigured = shown.client.take_reconfigured();
        if reconfigured {
            // Click-through: the OSD never takes a pointer event.
            for name in shown.client.output_names() {
                shown.client.set_input_region(&name, &[]);
            }
        }
        let alpha = model.alpha(now);
        let fresh = reconfigured || last_alpha.is_none();
        if !fresh && *last_alpha == Some(alpha) {
            return;
        }
        if !fresh && last_draw.is_some_and(|t| now.saturating_duration_since(t) < model::FRAME) {
            return;
        }
        *last_alpha = Some(alpha);
        *last_draw = Some(now);
        shown.client.present_for_each(|_, w, h| render::render(ts, theme, metrics, &ev, w, h, alpha));
    }

    fn apply_palette(&mut self, v: &Value) {
        if let Some(obj) = v.as_object() {
            let tokens = Tokens::from_hex_lookup(|k| obj.get(k).and_then(|x| x.as_str()));
            self.theme = Theme::from_palette(&tokens);
        }
    }

    fn apply_config(&mut self, v: &Value) {
        let Some(f) = v.get("font") else { return };
        let font = FontConfig::from_json(f);
        if font == self.font {
            return;
        }
        self.ts.reload(&font);
        self.metrics = Metrics::from_font_size(font.size);
        self.font = font;
        // The pill's size changed, so a visible one is rebuilt on the next event.
        self.hide();
        self.prewarm();
    }

    /// Parse the UI font and rasterise the common glyphs now, while nobody
    /// is waiting. Without this the first volume key pays for both (about
    /// 130 ms in a debug build, measured with WAYLAND_DEBUG) before the first
    /// pixel appears. Only the UI font is touched; mono and icons stay unread.
    fn prewarm(&mut self) {
        let sample = OsdEvent::new("Volume Brightness CONFIG ERROR 0123456789% abcdefghijklmnopqrstuvwxyz", "ABCDEFGHIJKLMNOPQRSTUVWXYZ .,:;'-", Some(50), 1000, "");
        let g = render::geometry(&self.metrics);
        let _ = render::render(&mut self.ts, &self.theme, &self.metrics, &sample, g.width, g.height, 1.0);
    }
}

/// Clean what the compositor sent before it reaches the screen: control
/// characters and bidirectional overrides are dropped (config errors carry
/// text from files), and the length is capped.
fn sanitised(mut ev: OsdEvent) -> OsdEvent {
    ev.title = clean(&ev.title, 120);
    ev.body = clean(&ev.body, 200);
    ev
}

fn clean(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c };
        let banned = c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}');
        if banned || (c == ' ' && (out.is_empty() || out.ends_with(' '))) {
            continue;
        }
        if out.len() + c.len_utf8() > max {
            out.push('\u{2026}');
            break;
        }
        out.push(c);
    }
    out.trim_end().to_string()
}

/// `poll(2)` takes milliseconds; round up so a short wait never spins at 0.
fn poll_timeout_ms(d: Duration) -> i32 {
    d.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_timeout_rounds_up_and_saturates() {
        assert_eq!(poll_timeout_ms(Duration::ZERO), 0);
        assert_eq!(poll_timeout_ms(Duration::from_micros(1)), 1);
        assert_eq!(poll_timeout_ms(Duration::from_millis(16)), 16);
        assert_eq!(poll_timeout_ms(Duration::from_secs(10_000_000)), i32::MAX);
    }

    #[test]
    fn clean_strips_spoofing_and_caps_length() {
        assert_eq!(clean("  a\n\tb\u{0}\u{202E}c  ", 100), "a bc");
        assert_eq!(clean(&"é".repeat(100), 11), "ééééé\u{2026}");
        assert_eq!(clean("ok", 100), "ok");
    }

    #[test]
    fn a_hostile_event_is_cleaned_before_display() {
        let ev = sanitised(OsdEvent::new("CONFIG ERROR\u{202E}", "line\n3", None, 2000, ""));
        assert_eq!((ev.title.as_str(), ev.body.as_str()), ("CONFIG ERROR", "line 3"));
    }
}
