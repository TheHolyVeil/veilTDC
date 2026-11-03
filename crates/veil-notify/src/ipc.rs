//! veil-notify's side of the Veil IPC: publish the bell state, receive control
//! events. Pure plumbing, no notification logic.
//!
//! One thread owns the connection's writes and the heartbeat; a second blocks
//! on the socket, so an idle connection costs one wakeup per ping interval and
//! nothing else. If the compositor is not there yet (or restarts) the client
//! keeps retrying with a capped backoff and re-publishes the latest state on
//! every new connection.
//!
//! The supervisor treats a component that has not pinged for 10 s as hung, so
//! the ping is sent whenever it is due, even in the middle of a burst of other
//! traffic. Expiry is not this module's business: it only ever sends the state
//! it is given.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};
use serde_json::Value;
use veil_ipc::notify::{Control, NotificationsState};
use veil_ipc::{commands, topics, ClientMessage, FrameBuffer, ServerMessage, PROTOCOL_VERSION};

/// The name sent in `hello`; the supervisor matches it to the component.
pub const COMPONENT_NAME: &str = "notify";

/// Master to main loop.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// The compositor said hello; the palette and config it sent are for the
    /// toast and panel theme and font (step 5).
    Connected { palette: Value, config: Value },
    /// A `notify.dnd` or `notify.panel` request.
    Control(Control),
    Palette(Value),
    Config(Value),
    /// The connection dropped; the client is already trying to get it back.
    Disconnected,
}

pub struct IpcConfig {
    pub socket: PathBuf,
    /// Heartbeat interval. The supervisor's limit is 10 s; the bar uses 3 s.
    pub ping: Duration,
    pub retry_min: Duration,
    pub retry_max: Duration,
}

impl Default for IpcConfig {
    fn default() -> Self {
        let socket = std::env::var("VEIL_IPC_SOCKET").map(PathBuf::from).unwrap_or_else(|_| veil_ipc::socket_path());
        IpcConfig { socket, ping: Duration::from_secs(3), retry_min: Duration::from_secs(1), retry_max: Duration::from_secs(10) }
    }
}

enum Msg {
    Publish(NotificationsState),
    /// A frame from the reader of connection `generation`.
    Frame(u64, ServerMessage),
    /// The reader of connection `generation` hit EOF or an error.
    Closed(u64),
    Shutdown,
}

pub struct Ipc {
    /// Everything the compositor wants the main loop to know.
    pub incoming: Receiver<Incoming>,
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl Ipc {
    /// Start the client. Returns at once; connecting happens on the thread.
    pub fn spawn(cfg: IpcConfig) -> io::Result<Ipc> {
        let (in_tx, incoming) = crossbeam_channel::bounded(256);
        let (tx, rx) = crossbeam_channel::unbounded();
        let own_tx = tx.clone();
        let thread = std::thread::Builder::new().name("veil-notify-ipc".into()).spawn(move || run(cfg, rx, own_tx, in_tx))?;
        Ok(Ipc { incoming, tx, thread: Some(thread) })
    }

    /// Tell the compositor what the bell should show. Sending the same state
    /// twice is free: only changes go out.
    pub fn publish(&self, state: NotificationsState) {
        let _ = self.tx.send(Msg::Publish(state));
    }
}

impl Drop for Ipc {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum Ended {
    Lost,
    Shutdown,
}

fn run(cfg: IpcConfig, rx: Receiver<Msg>, tx: Sender<Msg>, incoming: Sender<Incoming>) {
    let mut wanted: Option<NotificationsState> = None;
    let mut backoff = cfg.retry_min;
    let mut generation = 0u64;
    loop {
        let stream = match UnixStream::connect(&cfg.socket) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("notify: no compositor at {} ({e}); retrying in {backoff:?}", cfg.socket.display());
                if idle(&rx, backoff, &mut wanted) {
                    return;
                }
                backoff = (backoff * 2).min(cfg.retry_max);
                continue;
            }
        };
        backoff = cfg.retry_min;
        generation += 1;
        match session(&cfg, stream, generation, &rx, &tx, &incoming, &mut wanted) {
            Ended::Shutdown => return,
            Ended::Lost => emit(&incoming, Incoming::Disconnected),
        }
        // A compositor that accepts and immediately drops must not spin us.
        if idle(&rx, cfg.retry_min, &mut wanted) {
            return;
        }
    }
}

/// Wait out `dur` while disconnected, keeping the latest wanted state.
/// Returns true if told to shut down.
fn idle(rx: &Receiver<Msg>, dur: Duration, wanted: &mut Option<NotificationsState>) -> bool {
    let deadline = Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Msg::Publish(s)) => *wanted = Some(s),
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => return true,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return false,
        }
    }
}

struct Conn {
    w: UnixStream,
    next_id: u64,
    /// What the compositor currently has, so unchanged states are not resent.
    sent: Option<NotificationsState>,
}

impl Conn {
    fn send(&mut self, msg: &ClientMessage) -> io::Result<()> {
        let bytes = veil_ipc::encode(msg).map_err(io::Error::other)?;
        self.w.write_all(&bytes)
    }

    fn publish(&mut self, state: NotificationsState) -> io::Result<()> {
        if self.sent == Some(state) {
            return Ok(());
        }
        let id = self.next_id;
        self.next_id += 1;
        let args = serde_json::to_value(state).unwrap_or_default();
        self.send(&ClientMessage::Cmd { id, cmd: commands::NOTIFICATIONS_SET.into(), args })?;
        self.sent = Some(state);
        Ok(())
    }
}

fn session(
    cfg: &IpcConfig,
    stream: UnixStream,
    generation: u64,
    rx: &Receiver<Msg>,
    tx: &Sender<Msg>,
    incoming: &Sender<Incoming>,
    wanted: &mut Option<NotificationsState>,
) -> Ended {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("notify: cannot clone the IPC socket: {e}");
            return Ended::Lost;
        }
    };
    let reader = std::thread::Builder::new().name("veil-notify-ipc-rx".into()).spawn({
        let tx = tx.clone();
        move || read_frames(reader_stream, generation, tx)
    });
    let Ok(reader) = reader else { return Ended::Lost };

    let mut conn = Conn { w: stream, next_id: 1, sent: None };
    let greeted = conn
        .send(&ClientMessage::Hello { version: PROTOCOL_VERSION, name: COMPONENT_NAME.into() })
        .and_then(|()| {
            conn.send(&ClientMessage::Subscribe {
                topics: [topics::NOTIFY_CONTROL, topics::PALETTE, topics::CONFIG].iter().map(|t| t.to_string()).collect(),
            })
        })
        .and_then(|()| wanted.map_or(Ok(()), |s| conn.publish(s)));

    let mut last_ping = Instant::now();
    let end = if greeted.is_err() {
        Ended::Lost
    } else {
        loop {
            let wait = cfg.ping.saturating_sub(last_ping.elapsed());
            match rx.recv_timeout(wait) {
                Ok(Msg::Publish(s)) => {
                    *wanted = Some(s);
                    if conn.publish(s).is_err() {
                        break Ended::Lost;
                    }
                }
                Ok(Msg::Frame(g, frame)) if g == generation => handle_frame(frame, incoming),
                Ok(Msg::Closed(g)) if g == generation => break Ended::Lost,
                Ok(Msg::Frame(..) | Msg::Closed(_)) => {} // from a previous connection
                Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break Ended::Shutdown,
                Err(RecvTimeoutError::Timeout) => {}
            }
            // Due whether we woke for a message or a timeout, so a burst of
            // traffic cannot starve the heartbeat.
            if last_ping.elapsed() >= cfg.ping {
                last_ping = Instant::now();
                if conn.send(&ClientMessage::Ping).is_err() {
                    break Ended::Lost;
                }
            }
        }
    };
    let _ = conn.w.shutdown(Shutdown::Both);
    let _ = reader.join();
    end
}

fn read_frames(mut stream: UnixStream, generation: u64, tx: Sender<Msg>) {
    let mut frames = FrameBuffer::new();
    let mut buf = [0u8; 4096];
    'read: loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if frames.push(&buf[..n]).is_err() {
                    break;
                }
                loop {
                    match frames.next_frame::<ServerMessage>() {
                        Ok(Some(frame)) => {
                            if tx.send(Msg::Frame(generation, frame)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!("notify: bad frame from the compositor: {e}");
                            break 'read;
                        }
                    }
                }
            }
        }
    }
    let _ = tx.send(Msg::Closed(generation));
}

fn handle_frame(frame: ServerMessage, incoming: &Sender<Incoming>) {
    match frame {
        ServerMessage::Hello { palette, config, .. } => emit(incoming, Incoming::Connected { palette, config }),
        ServerMessage::Event { topic, data } => match topic.as_str() {
            topics::NOTIFY_CONTROL => match serde_json::from_value::<Control>(data) {
                Ok(control) => emit(incoming, Incoming::Control(control)),
                Err(e) => tracing::debug!("notify: ignoring a malformed control event: {e}"),
            },
            topics::PALETTE => emit(incoming, Incoming::Palette(data)),
            topics::CONFIG => emit(incoming, Incoming::Config(data)),
            _ => {}
        },
        ServerMessage::Reply { ok: false, error, .. } => {
            tracing::warn!("notify: the compositor refused a command: {}", error.unwrap_or_default());
        }
        ServerMessage::Reply { .. } | ServerMessage::Pong => {}
    }
}

fn emit(incoming: &Sender<Incoming>, ev: Incoming) {
    match incoming.try_send(ev) {
        Ok(()) | Err(TrySendError::Disconnected(_)) => {}
        Err(TrySendError::Full(_)) => tracing::warn!("notify: IPC event queue full, dropping an event"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use veil_ipc::notify::DndMode;

    fn state(count: u32, dnd: bool) -> NotificationsState {
        NotificationsState { count, dnd }
    }

    fn cfg(socket: PathBuf) -> IpcConfig {
        IpcConfig {
            socket,
            ping: Duration::from_millis(80),
            retry_min: Duration::from_millis(30),
            retry_max: Duration::from_millis(100),
        }
    }

    struct FakeServer {
        listener: UnixListener,
    }

    impl FakeServer {
        fn bind(path: &std::path::Path) -> FakeServer {
            let listener = UnixListener::bind(path).unwrap();
            listener.set_nonblocking(true).unwrap();
            FakeServer { listener }
        }

        fn accept(&self) -> Peer {
            let end = Instant::now() + Duration::from_secs(3);
            loop {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                        return Peer { stream, frames: FrameBuffer::new() };
                    }
                    Err(_) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
                    Err(e) => panic!("the client never connected: {e}"),
                }
            }
        }
    }

    struct Peer {
        stream: UnixStream,
        frames: FrameBuffer,
    }

    impl Peer {
        fn next(&mut self) -> Option<ClientMessage> {
            loop {
                if let Ok(Some(m)) = self.frames.next_frame::<ClientMessage>() {
                    return Some(m);
                }
                let mut buf = [0u8; 4096];
                match self.stream.read(&mut buf) {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => self.frames.push(&buf[..n]).unwrap(),
                }
            }
        }

        /// The next command, skipping heartbeats.
        fn next_cmd(&mut self) -> Option<(String, Value)> {
            loop {
                match self.next()? {
                    ClientMessage::Ping => continue,
                    ClientMessage::Cmd { cmd, args, .. } => return Some((cmd, args)),
                    _ => continue,
                }
            }
        }

        fn send(&mut self, msg: &ServerMessage) {
            self.stream.write_all(&veil_ipc::encode(msg).unwrap()).unwrap();
        }
    }

    fn setup() -> (tempfile::TempDir, FakeServer, Ipc) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc.sock");
        let server = FakeServer::bind(&path);
        let ipc = Ipc::spawn(cfg(path)).unwrap();
        (dir, server, ipc)
    }

    fn recv(ipc: &Ipc) -> Incoming {
        ipc.incoming.recv_timeout(Duration::from_secs(2)).expect("no event within 2 s")
    }

    fn set_args(count: u32, dnd: bool) -> Value {
        serde_json::json!({ "count": count, "dnd": dnd })
    }

    #[test]
    fn says_hello_subscribes_and_publishes_only_changes() {
        let (_dir, server, ipc) = setup();
        let mut peer = server.accept();
        assert_eq!(peer.next(), Some(ClientMessage::Hello { version: PROTOCOL_VERSION, name: "notify".into() }));
        match peer.next() {
            Some(ClientMessage::Subscribe { topics }) => {
                for want in ["notify.control", "palette", "config"] {
                    assert!(topics.iter().any(|t| t == want), "missing {want} in {topics:?}");
                }
                assert!(!topics.iter().any(|t| t == "notifications"), "notify publishes that topic, it does not read it");
            }
            other => panic!("expected a subscribe, got {other:?}"),
        }

        ipc.publish(state(2, false));
        ipc.publish(state(2, false)); // unchanged: must not go out
        ipc.publish(state(0, true));
        assert_eq!(peer.next_cmd(), Some(("notifications.set".into(), set_args(2, false))));
        assert_eq!(peer.next_cmd(), Some(("notifications.set".into(), set_args(0, true))), "the duplicate was skipped");
    }

    #[test]
    fn pings_on_schedule_even_under_a_stream_of_updates() {
        let (_dir, server, ipc) = setup();
        let mut peer = server.accept();
        let ipc = std::sync::Arc::new(ipc);
        let spammer = {
            let ipc = ipc.clone();
            std::thread::spawn(move || {
                for i in 0..40 {
                    ipc.publish(state(i, false)); // every 10 ms, well inside the 80 ms ping interval
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };
        let (mut pings, mut cmds) = (0, 0);
        let end = Instant::now() + Duration::from_millis(600);
        peer.stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        while Instant::now() < end {
            match peer.next() {
                Some(ClientMessage::Ping) => pings += 1,
                Some(ClientMessage::Cmd { .. }) => cmds += 1,
                _ => {}
            }
        }
        spammer.join().unwrap();
        assert!(cmds >= 30, "updates kept flowing ({cmds})");
        assert!(pings >= 3, "the heartbeat was starved by traffic ({pings} pings)");
    }

    #[test]
    fn delivers_control_palette_and_config_and_ignores_the_rest() {
        let (_dir, server, ipc) = setup();
        let mut peer = server.accept();
        peer.send(&ServerMessage::Hello { version: 1, palette: serde_json::json!({"a": 1}), config: serde_json::json!({"b": 2}) });
        let event = |topic: &str, data: Value| ServerMessage::Event { topic: topic.into(), data };
        peer.send(&event("notify.control", serde_json::json!({"dnd": "toggle"})));
        peer.send(&event("notify.control", serde_json::json!({"dnd": "banana"}))); // malformed: dropped
        peer.send(&event("workspaces", serde_json::json!({}))); // not ours
        peer.send(&event("notify.control", serde_json::json!({"panel": "open"})));
        peer.send(&event("palette", serde_json::json!({"c": 3})));
        peer.send(&event("config", serde_json::json!({"d": 4})));
        peer.send(&ServerMessage::Reply { id: 1, ok: false, error: Some("nope".into()), data: None }); // logged only

        assert_eq!(recv(&ipc), Incoming::Connected { palette: serde_json::json!({"a": 1}), config: serde_json::json!({"b": 2}) });
        assert_eq!(recv(&ipc), Incoming::Control(Control { dnd: Some(DndMode::Toggle), panel: None }));
        assert!(matches!(recv(&ipc), Incoming::Control(Control { dnd: None, panel: Some(_) })));
        assert_eq!(recv(&ipc), Incoming::Palette(serde_json::json!({"c": 3})));
        assert_eq!(recv(&ipc), Incoming::Config(serde_json::json!({"d": 4})));
    }

    #[test]
    fn a_refused_command_does_not_break_the_connection() {
        let (_dir, server, ipc) = setup();
        let mut peer = server.accept();
        ipc.publish(state(1, false));
        assert!(peer.next_cmd().is_some());
        peer.send(&ServerMessage::Reply { id: 1, ok: false, error: Some("nope".into()), data: None });
        ipc.publish(state(2, false));
        assert_eq!(peer.next_cmd(), Some(("notifications.set".into(), set_args(2, false))));
    }

    #[test]
    fn reconnects_and_republishes_the_latest_state() {
        let (_dir, server, ipc) = setup();
        let mut first = server.accept();
        ipc.publish(state(5, false));
        assert_eq!(first.next_cmd(), Some(("notifications.set".into(), set_args(5, false))));
        drop(first); // the compositor goes away

        let mut seen_disconnect = false;
        for _ in 0..3 {
            if recv(&ipc) == Incoming::Disconnected {
                seen_disconnect = true;
                break;
            }
        }
        assert!(seen_disconnect);

        let mut second = server.accept();
        assert!(matches!(second.next(), Some(ClientMessage::Hello { .. })));
        assert!(matches!(second.next(), Some(ClientMessage::Subscribe { .. })));
        assert_eq!(second.next_cmd(), Some(("notifications.set".into(), set_args(5, false))), "the new compositor learns the bell state");
    }

    #[test]
    fn a_state_published_while_disconnected_is_sent_on_connect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc.sock");
        let ipc = Ipc::spawn(cfg(path.clone())).unwrap(); // nothing is listening yet
        ipc.publish(state(1, false));
        ipc.publish(state(4, true));
        std::thread::sleep(Duration::from_millis(250));
        let server = FakeServer::bind(&path);
        let mut peer = server.accept();
        assert_eq!(peer.next_cmd(), Some(("notifications.set".into(), set_args(4, true))), "only the latest, once");
    }

    #[test]
    fn shutting_down_is_prompt_connected_or_not() {
        let t = Instant::now();
        let dir = tempfile::tempdir().unwrap();
        drop(Ipc::spawn(cfg(dir.path().join("none.sock"))).unwrap());
        let (_d, server, ipc) = setup();
        let _peer = server.accept();
        drop(ipc);
        assert!(t.elapsed() < Duration::from_secs(1), "took {:?}", t.elapsed());
    }
}
