//! Media: now-playing from any MPRIS player, click to play/pause.
//!
//! One worker thread owns the session-bus connection. It polls the players
//! every couple of seconds and wakes at once for a click, so the render
//! loop never waits on D-Bus. zbus 6 is async-only; with no runtime of its
//! own the connection is driven by zbus's helper thread while this thread
//! blocks on it with `futures_lite::future::block_on`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use veil_ui::{Action, Metrics, Theme, Widget};
use zbus::OwnedVariant;

use crate::util::{label, truncate_chars};

const POLL: Duration = Duration::from_secs(2);
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MAX_CHARS: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Now {
    /// "Artist — Title", or just the title. Empty when nothing is loaded.
    pub text: String,
    pub playing: bool,
}

/// How good a player's state is as "the one to show": playing beats paused
/// beats stopped.
fn rank(status: &str) -> u8 {
    match status {
        "Playing" => 2,
        "Paused" => 1,
        _ => 0,
    }
}

pub fn now_text(artist: &[String], title: &str) -> String {
    let title = title.trim();
    let artist = artist.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect::<Vec<_>>().join(", ");
    match (artist.is_empty(), title.is_empty()) {
        (false, false) => format!("{artist} — {title}"),
        (true, false) => title.to_string(),
        _ => String::new(),
    }
}

struct Shared {
    now: Mutex<Now>,
    version: AtomicU64,
}

pub struct MediaWidget {
    shared: Arc<Shared>,
    seen: u64,
    tx: mpsc::Sender<()>,
}

impl Default for MediaWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaWidget {
    pub fn new() -> Self {
        let shared = Arc::new(Shared { now: Mutex::new(Now::default()), version: AtomicU64::new(0) });
        let (tx, rx) = mpsc::channel();
        let worker = shared.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("veil-bar-media".into())
            .spawn(move || futures_lite::future::block_on(worker_loop(worker, rx)))
        {
            tracing::warn!("bar: could not start the media worker: {e}");
        }
        Self { shared, seen: 0, tx }
    }

    fn now(&self) -> Now {
        self.shared.now.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Widget for MediaWidget {
    fn name(&self) -> &str {
        "media"
    }

    fn update(&mut self, _now: Instant) -> bool {
        let v = self.shared.version.load(Ordering::Acquire);
        let changed = v != self.seen;
        self.seen = v;
        changed
    }

    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &mut TextSystem,
        theme: &Theme,
        m: &Metrics,
        x: f32,
        y: f32,
        h: f32,
    ) -> f32 {
        let now = self.now();
        if now.text.is_empty() {
            return 0.0;
        }
        let icon = if now.playing { "\u{f040a}" } else { "\u{f03e4}" };
        let s = format!("{icon} {}", truncate_chars(&now.text, MAX_CHARS));
        let color = if now.playing { theme.accent } else { theme.text_dim };
        label(pixmap, text, m, x, y, h, &s, color)
    }

    fn on_click(&mut self, _x: f32) -> Option<Action> {
        // Handled by the worker, which holds the bus connection.
        let _ = self.tx.send(());
        None
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

async fn worker_loop(shared: Arc<Shared>, clicks: mpsc::Receiver<()>) {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::info!("bar: media widget off, no session bus: {e}");
            return;
        }
    };
    loop {
        let polled = poll(&conn).await;
        let current = polled.as_ref().map(|(bus, _)| bus.clone());
        publish(&shared, polled.map(|(_, now)| now).unwrap_or_default());
        // Sleep until the next poll, but wake immediately for a click.
        match clicks.recv_timeout(POLL) {
            Ok(()) => {
                if let Some(bus) = &current {
                    play_pause(&conn, bus).await;
                }
                // Show the new state without waiting out the interval.
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return, // the widget was dropped
        }
    }
}

fn publish(shared: &Shared, now: Now) {
    let mut g = shared.now.lock().unwrap_or_else(|e| e.into_inner());
    if *g != now {
        *g = now;
        shared.version.fetch_add(1, Ordering::Release);
    }
}

async fn player_proxy<'a>(conn: &zbus::Connection, bus: &'a str) -> zbus::Result<zbus::Proxy<'a>> {
    zbus::Proxy::new(conn, bus, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player").await
}

/// The best player right now: its bus name and what it is playing.
async fn poll(conn: &zbus::Connection) -> Option<(String, Now)> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.ok()?;
    let names = dbus.list_names().await.ok()?;
    let mut best: Option<(u8, String, Now)> = None;
    for name in names.iter().map(|n| n.as_str()).filter(|n| n.starts_with(MPRIS_PREFIX)) {
        let Ok(p) = player_proxy(conn, name).await else { continue };
        let Ok(status) = p.get_property::<String>("PlaybackStatus").await else { continue };
        let meta: HashMap<String, OwnedVariant> = p.get_property("Metadata").await.unwrap_or_default();
        let title = meta
            .get("xesam:title")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| String::try_from(v).ok())
            .unwrap_or_default();
        let artist = meta
            .get("xesam:artist")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| Vec::<String>::try_from(v).ok())
            .unwrap_or_default();
        let now = Now { text: now_text(&artist, &title), playing: status == "Playing" };
        let r = rank(&status);
        if best.as_ref().is_none_or(|(br, _, _)| r > *br) {
            best = Some((r, name.to_string(), now));
        }
    }
    best.map(|(_, bus, now)| (bus, now))
}

async fn play_pause(conn: &zbus::Connection, bus: &str) {
    if let Ok(p) = player_proxy(conn, bus).await {
        if let Err(e) = p.call_method("PlayPause", &()).await {
            tracing::debug!("bar: PlayPause on {bus} failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artist_and_title_are_joined() {
        assert_eq!(now_text(&["Boards of Canada".into()], "Roygbiv"), "Boards of Canada — Roygbiv");
        assert_eq!(now_text(&["A".into(), "B".into()], "T"), "A, B — T");
        assert_eq!(now_text(&[], "Only Title"), "Only Title");
        assert_eq!(now_text(&["  ".into()], "  Title "), "Title");
        assert_eq!(now_text(&["Artist".into()], ""), "", "no title means nothing to show");
    }

    #[test]
    fn playing_outranks_paused_outranks_stopped() {
        assert!(rank("Playing") > rank("Paused"));
        assert!(rank("Paused") > rank("Stopped"));
        assert_eq!(rank("Stopped"), rank("whatever"));
    }

    #[test]
    fn publishing_only_bumps_the_version_on_change() {
        let s = Shared { now: Mutex::new(Now::default()), version: AtomicU64::new(0) };
        publish(&s, Now::default());
        assert_eq!(s.version.load(Ordering::Relaxed), 0);
        publish(&s, Now { text: "x".into(), playing: true });
        publish(&s, Now { text: "x".into(), playing: true });
        assert_eq!(s.version.load(Ordering::Relaxed), 1);
    }

    /// Needs a session bus and a player; run with `--ignored` to look at it.
    #[test]
    #[ignore]
    fn live_poll() {
        let r = futures_lite::future::block_on(async {
            let conn = zbus::Connection::session().await.unwrap();
            poll(&conn).await
        });
        eprintln!("live MPRIS poll: {r:?}");
    }
}
