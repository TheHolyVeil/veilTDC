//! `org.freedesktop.Notifications` on the session bus.
//!
//! zbus 6 is async-only, so one worker thread owns the connection and blocks on
//! it with `futures_lite::future::block_on` (the same pattern as `veil-bar`'s
//! media widget). The interface handlers run on zbus's own executor, do only
//! cheap work (parse, sanitise, allocate an id) and hand the result to the main
//! loop through a bounded channel: D-Bus never waits on rendering and a stalled
//! main loop cannot make this process grow without bound.
//!
//! Name ownership: the name is requested with *no* flags, so a second daemon
//! (nested testing under DMS, `swaync`) puts us in the queue instead of failing
//! or stealing the name, and we take over when the owner exits. We do not allow
//! replacement, so nobody can take the name from us. zbus's `Builder::name` and
//! `request_name` would use `REPLACE_EXISTING | DO_NOT_QUEUE`, which is the
//! opposite, hence the explicit `request_name_with_flags` after `serve_at`.
//!
//! Ids and who may use them: `replaces_id` and `CloseNotification` work for any
//! caller, as long as the id was actually handed out. This is deliberate.
//! Updating a notification from another short-lived process (`notify-send -r`,
//! volume and brightness scripts) is the standard pattern, and every process is
//! a new bus connection, so tying ids to the sender breaks it. The session bus
//! has no isolation between clients anyway: any client can already send a
//! notification under any app name, so id ownership would protect nothing real.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use futures_lite::future::block_on;
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::names::BusName;
use zbus::object_server::SignalEmitter;
use zbus::OwnedVariant;

use crate::ids::IdGen;
use crate::model::{Hints, Limits, Notification, NotifyArgs, RawImage};
use crate::waker::Waker;

pub const BUS_NAME: &str = "org.freedesktop.Notifications";
pub const OBJECT_PATH: &str = "/org/freedesktop/Notifications";
/// Notification spec 1.3. We do not emit its optional `ActivationToken` signal.
pub const SPEC_VERSION: &str = "1.3";
pub const CAPABILITIES: [&str; 4] = ["actions", "body", "body-markup", "icon-static"];

/// Never act on more raw action strings than this, however many an app sends.
const MAX_RAW_ACTIONS: usize = 128;

/// Why a notification closed, as sent in `NotificationClosed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Expired = 1,
    Dismissed = 2,
    Closed = 3,
    Undefined = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameState {
    Owner,
    Queued,
}

/// D-Bus thread to main loop.
#[derive(Debug)]
pub enum Event {
    /// An app sent a notification. `replaced` is true when it reuses an id the
    /// same sender already owns (so the main loop swaps it in place).
    Notify { notification: Notification, replaced: bool },
    /// An app called `CloseNotification` for one of its own ids.
    Close { id: u32 },
    Name(NameState),
    /// No session bus, or the connection failed. The worker has exited.
    Unavailable(String),
}

/// Main loop to D-Bus thread.
#[derive(Debug)]
pub enum Out {
    Closed { id: u32, reason: CloseReason },
    Action { id: u32, key: String },
    Shutdown,
}

pub struct ServiceConfig {
    pub limits: Limits,
    /// Connect here instead of the session bus (used by tests).
    pub bus_address: Option<String>,
    /// Events buffered between the bus and the main loop; extra ones are dropped.
    pub queue_depth: usize,
    /// How often a queued daemon checks whether it has become the owner.
    pub name_poll: Duration,
    /// Poked after every event queued for the main loop.
    pub waker: Option<Arc<Waker>>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        ServiceConfig { limits: Limits::default(), bus_address: None, queue_depth: 256, name_poll: Duration::from_secs(2), waker: None }
    }
}

// ---------------------------------------------------------------------------
// Ids

/// Pick the id for a `Notify` call: reuse `replaces_id` when it was handed out
/// (the spec says the same id comes back), otherwise allocate a fresh one. The
/// second value says whether the id was reused. An id that was never handed out
/// cannot be claimed, so a caller cannot park the counter in the future.
pub(crate) fn resolve_id(ids: &IdGen, replaces_id: u32) -> (u32, bool) {
    if replaces_id != 0 && replaces_id < ids.peek() {
        (replaces_id, true)
    } else {
        (ids.alloc(), false)
    }
}

// ---------------------------------------------------------------------------
// Hints

fn urgency_byte(v: &OwnedVariant) -> Option<u8> {
    u8::try_from(v)
        .ok()
        .or_else(|| u32::try_from(v).ok().map(|n| n.min(255) as u8))
        .or_else(|| i32::try_from(v).ok().map(|n| n.clamp(0, 255) as u8))
}

fn raw_image(v: OwnedVariant) -> Option<RawImage> {
    let (width, height, rowstride, has_alpha, bits_per_sample, channels, data): (i32, i32, i32, bool, i32, i32, Vec<u8>) =
        v.try_into().ok()?;
    Some(RawImage { width, height, rowstride, has_alpha, bits_per_sample, channels, data })
}

/// Pull out the hints we act on. Wrongly typed values are ignored, never an error.
/// Accepts the spec names and the deprecated `image_data`, `icon_data`, `image_path`.
pub(crate) fn extract_hints(mut hints: HashMap<String, OwnedVariant>) -> Hints {
    let mut h = Hints::default();
    if let Some(v) = hints.remove("urgency") {
        h.urgency = urgency_byte(&v);
    }
    if let Some(v) = hints.remove("transient") {
        h.transient = bool::try_from(&v).unwrap_or(false);
    }
    if let Some(v) = hints.remove("resident") {
        h.resident = bool::try_from(&v).unwrap_or(false);
    }
    if let Some(v) = hints.remove("desktop-entry") {
        h.desktop_entry = String::try_from(v).ok();
    }
    for key in ["image-path", "image_path"] {
        if let Some(v) = hints.remove(key) {
            if h.image_path.is_none() {
                h.image_path = String::try_from(v).ok();
            }
        }
    }
    for key in ["image-data", "image_data", "icon_data"] {
        if let Some(v) = hints.remove(key) {
            if h.image_data.is_none() {
                h.image_data = raw_image(v);
            }
        }
    }
    h
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The interface

struct Notifications {
    tx: Sender<Event>,
    ids: IdGen,
    limits: Limits,
    waker: Option<Arc<Waker>>,
}

impl Notifications {
    fn send(&self, ev: Event) {
        match self.tx.try_send(ev) {
            Ok(()) => {
                if let Some(w) = &self.waker {
                    w.wake();
                }
            }
            Err(TrySendError::Full(_)) => tracing::warn!("notification queue full, dropping an event"),
            Err(TrySendError::Disconnected(_)) => tracing::debug!("main loop is gone, dropping an event"),
        }
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    fn get_capabilities(&self) -> Vec<String> {
        CAPABILITIES.iter().map(|s| s.to_string()).collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        mut actions: Vec<String>,
        hints: HashMap<String, OwnedVariant>,
        expire_timeout: i32,
    ) -> u32 {
        let (id, replaced) = resolve_id(&self.ids, replaces_id);
        actions.truncate(MAX_RAW_ACTIONS);
        let args = NotifyArgs {
            app_name,
            replaces_id,
            app_icon,
            summary,
            body,
            actions,
            hints: extract_hints(hints),
            expire_timeout,
        };
        let notification = Notification::from_args(id, args, &self.limits, now_ms());
        self.send(Event::Notify { notification, replaced });
        id
    }

    fn close_notification(&self, id: u32) -> zbus::fdo::Result<()> {
        if id == 0 || id >= self.ids.peek() {
            return Err(zbus::fdo::Error::Failed("no such notification".into()));
        }
        self.send(Event::Close { id });
        Ok(())
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        ("veil-notify".into(), "TheHolyVeil".into(), env!("CARGO_PKG_VERSION").into(), SPEC_VERSION.into())
    }

    #[zbus(signal)]
    async fn notification_closed(emitter: &SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(emitter: &SignalEmitter<'_>, id: u32, action_key: &str) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// The service: worker thread and handle

pub struct Service {
    /// Everything the bus wants the main loop to know.
    pub events: Receiver<Event>,
    out: Sender<Out>,
    thread: Option<JoinHandle<()>>,
}

impl Service {
    /// Start the worker. Returns at once; connection problems arrive as
    /// [`Event::Unavailable`], and the name state as [`Event::Name`].
    pub fn spawn(cfg: ServiceConfig, ids: IdGen) -> io::Result<Service> {
        let (tx, events) = crossbeam_channel::bounded(cfg.queue_depth.max(1));
        let (out, out_rx) = crossbeam_channel::unbounded();
        let thread = std::thread::Builder::new()
            .name("veil-notify-dbus".into())
            .spawn(move || run(cfg, ids, tx, out_rx))?;
        Ok(Service { events, out, thread: Some(thread) })
    }

    /// Tell the app a notification closed (timeout, dismissal, or its own call).
    pub fn closed(&self, id: u32, reason: CloseReason) {
        let _ = self.out.send(Out::Closed { id, reason });
    }

    /// Tell the app the user invoked an action.
    pub fn action(&self, id: u32, key: &str) {
        let _ = self.out.send(Out::Action { id, key: key.to_string() });
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.out.send(Out::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(cfg: ServiceConfig, ids: IdGen, tx: Sender<Event>, out_rx: Receiver<Out>) {
    let iface = Notifications { tx: tx.clone(), ids, limits: cfg.limits.clone(), waker: cfg.waker.clone() };
    let wake = || {
        if let Some(w) = &cfg.waker {
            w.wake();
        }
    };
    let builder = match &cfg.bus_address {
        Some(addr) => zbus::connection::Builder::address(addr.as_str()),
        None => zbus::connection::Builder::session(),
    };
    let setup = block_on(async {
        let conn = builder.serve_at(OBJECT_PATH, iface).build().await?;
        // After serve_at, so no call can arrive before the handlers exist.
        let reply = conn.request_name_with_flags(BUS_NAME, RequestNameFlags::empty()).await?;
        let state = match reply {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => NameState::Owner,
            RequestNameReply::InQueue | RequestNameReply::Exists => NameState::Queued,
        };
        zbus::Result::Ok((conn, state))
    });
    let (conn, mut state) = match setup {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!("notification service unavailable: {e}");
            let _ = tx.send(Event::Unavailable(e.to_string()));
            wake();
            return;
        }
    };
    if state == NameState::Queued {
        tracing::warn!("{BUS_NAME} is owned by another daemon; waiting for it to exit");
    }
    let _ = tx.send(Event::Name(state));
    wake();

    loop {
        // Only a queued daemon needs to wake up on its own; an owner sleeps until told something.
        let msg = if state == NameState::Queued {
            match out_rx.recv_timeout(cfg.name_poll) {
                Ok(m) => Some(m),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match out_rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            }
        };
        match msg {
            Some(Out::Shutdown) => break,
            Some(Out::Closed { id, reason }) => {
                if let Err(e) = block_on(emit_closed(&conn, id, reason as u32)) {
                    tracing::debug!("NotificationClosed({id}) not sent: {e}");
                }
            }
            Some(Out::Action { id, key }) => {
                if let Err(e) = block_on(emit_action(&conn, id, &key)) {
                    tracing::debug!("ActionInvoked({id}) not sent: {e}");
                }
            }
            None => {
                if block_on(owns_name(&conn)) {
                    state = NameState::Owner;
                    tracing::info!("took over {BUS_NAME}");
                    let _ = tx.send(Event::Name(state));
                    wake();
                }
            }
        }
    }
}

async fn emit_closed(conn: &zbus::Connection, id: u32, reason: u32) -> zbus::Result<()> {
    let iface = conn.object_server().interface::<_, Notifications>(OBJECT_PATH).await?;
    Notifications::notification_closed(iface.signal_emitter(), id, reason).await
}

async fn emit_action(conn: &zbus::Connection, id: u32, key: &str) -> zbus::Result<()> {
    let iface = conn.object_server().interface::<_, Notifications>(OBJECT_PATH).await?;
    Notifications::action_invoked(iface.signal_emitter(), id, key).await
}

async fn owns_name(conn: &zbus::Connection) -> bool {
    let Ok(proxy) = DBusProxy::new(conn).await else { return false };
    let Ok(name) = BusName::try_from(BUS_NAME) else { return false };
    match proxy.get_name_owner(name).await {
        Ok(owner) => conn.unique_name().is_some_and(|me| me.as_str() == owner.as_str()),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use zbus::Variant;

    // ----- pure parts

    #[test]
    fn resolve_id_rules() {
        let ids = IdGen::new(10);
        assert_eq!(resolve_id(&ids, 0), (10, false), "0 means a new notification");
        assert_eq!(resolve_id(&ids, 4), (4, true), "a handed-out id is reused, whoever asks");
        assert_eq!(resolve_id(&ids, 11), (11, false), "the next id was not handed out yet");
        let (id, replaced) = resolve_id(&ids, 4_000_000_000);
        assert!(!replaced && id == 12, "an id never handed out cannot be claimed");
    }

    fn ov(v: Variant<'_>) -> OwnedVariant {
        OwnedVariant::try_from(v).unwrap()
    }

    #[test]
    fn hints_are_extracted_and_bad_types_ignored() {
        let mut m = HashMap::new();
        m.insert("urgency".to_string(), ov(Variant::new(2u8)));
        m.insert("transient".to_string(), ov(Variant::new(true)));
        m.insert("resident".to_string(), ov(Variant::new("not a bool")));
        m.insert("desktop-entry".to_string(), ov(Variant::new("org.app.Mail")));
        m.insert("image_path".to_string(), ov(Variant::new("/tmp/a.png")));
        let h = extract_hints(m);
        assert_eq!(h.urgency, Some(2));
        assert!(h.transient && !h.resident);
        assert_eq!(h.desktop_entry.as_deref(), Some("org.app.Mail"));
        assert_eq!(h.image_path.as_deref(), Some("/tmp/a.png"), "the deprecated name still works");

        let mut m = HashMap::new();
        m.insert("urgency".to_string(), ov(Variant::new(1u32)));
        assert_eq!(extract_hints(m).urgency, Some(1), "an int urgency from a sloppy app");
    }

    #[test]
    fn image_data_hint_decodes() {
        let mut m = HashMap::new();
        m.insert("image-data".to_string(), ov(Variant::new((1i32, 1i32, 3i32, false, 8i32, 3i32, vec![9u8, 8, 7]))));
        let img = extract_hints(m).image_data.expect("decoded");
        assert_eq!((img.width, img.height, img.channels, img.data), (1, 1, 3, vec![9, 8, 7]));
        let mut m = HashMap::new();
        m.insert("image-data".to_string(), ov(Variant::new("garbage")));
        assert!(extract_hints(m).image_data.is_none());
    }

    #[test]
    fn capabilities_are_the_documented_set() {
        assert_eq!(CAPABILITIES, ["actions", "body", "body-markup", "icon-static"]);
    }

    // ----- against a private bus

    struct TestBus {
        child: Child,
        address: String,
    }

    impl TestBus {
        fn start() -> Option<TestBus> {
            let mut child = Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let mut line = String::new();
            BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
            let address = line.trim().to_string();
            (!address.is_empty()).then_some(TestBus { child, address })
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    macro_rules! bus_or_skip {
        () => {
            match TestBus::start() {
                Some(b) => b,
                None => {
                    eprintln!("dbus-daemon not available, skipping");
                    return;
                }
            }
        };
    }

    fn cfg(bus: &TestBus) -> ServiceConfig {
        ServiceConfig { bus_address: Some(bus.address.clone()), name_poll: Duration::from_millis(100), ..ServiceConfig::default() }
    }

    const WAIT: Duration = Duration::from_secs(5);

    /// The next non-`Name` event.
    fn next_event(svc: &Service) -> Event {
        loop {
            match svc.events.recv_timeout(WAIT).expect("no event within 5 s") {
                Event::Name(_) => continue,
                ev => return ev,
            }
        }
    }

    fn expect_name(svc: &Service, want: NameState) {
        match svc.events.recv_timeout(WAIT).expect("no name event") {
            Event::Name(s) => assert_eq!(s, want),
            other => panic!("expected a name event, got {other:?}"),
        }
    }

    fn client(bus: &TestBus) -> zbus::Connection {
        block_on(zbus::connection::Builder::address(bus.address.as_str()).build()).expect("client connects")
    }

    fn proxy(conn: &zbus::Connection) -> zbus::Proxy<'static> {
        block_on(zbus::Proxy::new(conn, BUS_NAME, OBJECT_PATH, BUS_NAME)).expect("proxy")
    }

    type Hint = HashMap<&'static str, Variant<'static>>;

    fn notify(p: &zbus::Proxy<'_>, replaces: u32, summary: &str, hints: &Hint) -> zbus::Result<u32> {
        block_on(p.call("Notify", &("app", replaces, "", summary, "<b>body</b>", vec!["default", "Open"], hints, -1i32)))
    }

    #[test]
    fn answers_the_basic_methods() {
        let bus = bus_or_skip!();
        let svc = Service::spawn(cfg(&bus), IdGen::new(1)).unwrap();
        expect_name(&svc, NameState::Owner);
        let p = proxy(&client(&bus));
        let caps: Vec<String> = block_on(p.call("GetCapabilities", &())).unwrap();
        assert_eq!(caps, CAPABILITIES);
        let info: (String, String, String, String) = block_on(p.call("GetServerInformation", &())).unwrap();
        assert_eq!((info.0.as_str(), info.1.as_str(), info.3.as_str()), ("veil-notify", "TheHolyVeil", "1.3"));
    }

    #[test]
    fn notify_round_trip_carries_everything() {
        let bus = bus_or_skip!();
        let svc = Service::spawn(cfg(&bus), IdGen::new(1)).unwrap();
        expect_name(&svc, NameState::Owner);
        let p = proxy(&client(&bus));
        let mut hints = Hint::new();
        hints.insert("urgency", Variant::new(2u8));
        hints.insert("desktop-entry", Variant::new("org.app.Mail"));
        hints.insert("image-data", Variant::new((1i32, 1i32, 3i32, false, 8i32, 3i32, vec![1u8, 2, 3])));
        let id = notify(&p, 0, "Hello", &hints).unwrap();
        assert_eq!(id, 1);
        match next_event(&svc) {
            Event::Notify { notification: n, replaced } => {
                assert!(!replaced);
                assert_eq!((n.id, n.summary.as_str(), n.body.as_str()), (1, "Hello", "<b>body</b>"));
                assert_eq!(n.urgency, crate::model::Urgency::Critical);
                assert_eq!(n.desktop_entry.as_deref(), Some("org.app.Mail"));
                assert_eq!(n.image.as_ref().map(|i| i.rgba.clone()), Some(vec![1, 2, 3, 255]));
                assert!(n.default_action().is_some());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn replaces_id_works_across_connections() {
        let bus = bus_or_skip!();
        let svc = Service::spawn(cfg(&bus), IdGen::new(1)).unwrap();
        expect_name(&svc, NameState::Owner);
        // Two connections, like two `notify-send` runs.
        let (a, b) = (proxy(&client(&bus)), proxy(&client(&bus)));
        let none = Hint::new();
        let id = notify(&a, 0, "first", &none).unwrap();
        assert_eq!(notify(&b, id, "second", &none).unwrap(), id, "another process updates it, as notify-send -r does");
        let fresh = notify(&b, 4_000_000, "third", &none).unwrap();
        assert_ne!(fresh, 4_000_000, "an id that was never handed out is not honoured");
        let mut seen = Vec::new();
        for _ in 0..3 {
            match next_event(&svc) {
                Event::Notify { notification, replaced } => seen.push((notification.summary, replaced)),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(seen, vec![("first".to_string(), false), ("second".to_string(), true), ("third".to_string(), false)]);
    }

    #[test]
    fn close_notification_needs_a_known_id() {
        let bus = bus_or_skip!();
        let svc = Service::spawn(cfg(&bus), IdGen::new(1)).unwrap();
        expect_name(&svc, NameState::Owner);
        let (a, b) = (proxy(&client(&bus)), proxy(&client(&bus)));
        let id = notify(&a, 0, "mine", &Hint::new()).unwrap();
        assert!(matches!(next_event(&svc), Event::Notify { .. }));
        assert!(block_on(a.call::<_, _, ()>("CloseNotification", &(9999u32,))).is_err(), "never existed");
        assert!(block_on(a.call::<_, _, ()>("CloseNotification", &(0u32,))).is_err());
        // a different process can close it, as `dunstify -C` style scripts do
        block_on(b.call::<_, _, ()>("CloseNotification", &(id,))).unwrap();
        assert!(matches!(next_event(&svc), Event::Close { id: got } if got == id));
    }

    #[test]
    fn signals_reach_the_client() {
        use futures_lite::StreamExt;
        let bus = bus_or_skip!();
        let svc = Service::spawn(cfg(&bus), IdGen::new(1)).unwrap();
        expect_name(&svc, NameState::Owner);
        let p = proxy(&client(&bus));
        let id = notify(&p, 0, "x", &Hint::new()).unwrap();
        let (mut closed, mut action) = block_on(async {
            (p.receive_signal("NotificationClosed").await.unwrap(), p.receive_signal("ActionInvoked").await.unwrap())
        });
        svc.action(id, "default");
        svc.closed(id, CloseReason::Dismissed);
        let got: (u32, String) = block_on(async { action.next().await }).unwrap().body().deserialize().unwrap();
        assert_eq!(got, (id, "default".to_string()));
        let got: (u32, u32) = block_on(async { closed.next().await }).unwrap().body().deserialize().unwrap();
        assert_eq!(got, (id, 2));
    }

    #[test]
    fn a_second_daemon_queues_and_takes_over_when_the_first_exits() {
        let bus = bus_or_skip!();
        let first = Service::spawn(cfg(&bus), IdGen::new(1)).unwrap();
        expect_name(&first, NameState::Owner);
        let second = Service::spawn(cfg(&bus), IdGen::new(1)).unwrap();
        expect_name(&second, NameState::Queued);

        // While queued, the first daemon still answers.
        let p = proxy(&client(&bus));
        notify(&p, 0, "to the owner", &Hint::new()).unwrap();
        assert!(matches!(next_event(&first), Event::Notify { .. }));
        assert!(second.events.try_recv().is_err(), "the queued daemon sees nothing");

        drop(first);
        expect_name(&second, NameState::Owner);
        notify(&p, 0, "to the new owner", &Hint::new()).unwrap();
        assert!(matches!(next_event(&second), Event::Notify { .. }));
    }

    #[test]
    fn no_bus_is_reported_not_fatal() {
        let svc = Service::spawn(
            ServiceConfig { bus_address: Some("unix:path=/nonexistent/veil-test-bus".into()), ..ServiceConfig::default() },
            IdGen::new(1),
        )
        .unwrap();
        match svc.events.recv_timeout(WAIT).unwrap() {
            Event::Unavailable(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}
