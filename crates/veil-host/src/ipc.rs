//! Veil IPC Server: Unix socket listener, client supervision, SO_PEERCRED validation,
//! subscription event broadcasting, and command dispatch.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use veil_ipc::{
    ClientMessage, FrameBuffer, IpcError, ServerMessage, MAX_SUBSCRIBER_QUEUE,
    PROTOCOL_VERSION,
};

#[derive(Debug)]
pub struct IpcClient {
    pub id: usize,
    pub stream: UnixStream,
    pub name: Option<String>,
    pub peer_uid: u32,
    pub frame_buf: FrameBuffer,
    pub subscriptions: HashSet<String>,
    pub outgoing: VecDeque<u8>,
}

pub struct IpcServer {
    listener: Option<UnixListener>,
    socket_path: PathBuf,
    next_client_id: usize,
    clients: HashMap<usize, IpcClient>,
    /// Last `notifications` event, replayed to anyone who subscribes later
    /// (a restarted bar must not show a stale bell until the next change).
    notifications: Option<serde_json::Value>,
}

impl IpcServer {
    pub fn new() -> Self {
        Self::with_path(veil_ipc::socket_path())
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self {
            listener: None,
            socket_path: path,
            next_client_id: 1,
            clients: HashMap::new(),
            notifications: None,
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Name a client gave in its `hello`, if it has sent one.
    pub fn client_name(&self, client_id: usize) -> Option<String> {
        self.clients.get(&client_id).and_then(|c| c.name.clone())
    }

    /// Bind the Unix socket at mode 0600.
    pub fn bind(&mut self) -> Result<(), std::io::Error> {
        if let Some(parent) = self.socket_path.parent() {
            fs::create_dir_all(parent)?;
            // Parent dir mode 0700 (user-only)
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }

        // Clean up a stale socket, but never steal a live one: a second veil
        // (e.g. a nested test run inside a real session) must not take the IPC
        // socket away from the session that owns it.
        if self.socket_path.exists() {
            if UnixStream::connect(&self.socket_path).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("{} is owned by another running veil", self.socket_path.display()),
                ));
            }
            let _ = fs::remove_file(&self.socket_path);
        }

        let listener = UnixListener::bind(&self.socket_path)?;
        listener.set_nonblocking(true)?;

        // Socket file mode 0600
        let _ = fs::set_permissions(&self.socket_path, fs::Permissions::from_mode(0o600));
        self.listener = Some(listener);
        Ok(())
    }

    /// Check peer credentials using SO_PEERCRED to ensure same-user authentication.
    pub fn check_peer_cred(stream: &UnixStream) -> Result<u32, std::io::Error> {
        let fd = stream.as_raw_fd();
        let mut ucred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

        let ret = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut ucred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };

        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let my_uid = unsafe { libc::getuid() };
        if ucred.uid != my_uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("Peer UID {} does not match server UID {}", ucred.uid, my_uid),
            ));
        }

        Ok(ucred.uid)
    }

    /// Accept any pending client connections.
    pub fn accept_incoming(&mut self) {
        let Some(listener) = &self.listener else { return };

        while let Ok((stream, _)) = listener.accept() {
            let _ = stream.set_nonblocking(true);
            match Self::check_peer_cred(&stream) {
                Ok(uid) => {
                    let id = self.next_client_id;
                    self.next_client_id += 1;
                    self.clients.insert(
                        id,
                        IpcClient {
                            id,
                            stream,
                            name: None,
                            peer_uid: uid,
                            frame_buf: FrameBuffer::new(),
                            subscriptions: HashSet::new(),
                            outgoing: VecDeque::new(),
                        },
                    );
                }
                Err(e) => {
                    tracing::warn!("Rejected IPC connection: {e}");
                }
            }
        }
    }

    /// Flush outgoing queued bytes for all clients. Disconnects clients that fail or exceed 1 MiB.
    pub fn flush_outgoing(&mut self) {
        let mut to_remove = Vec::new();

        for (&id, client) in self.clients.iter_mut() {
            if client.outgoing.len() > MAX_SUBSCRIBER_QUEUE {
                tracing::warn!("Dropping slow IPC client {id}: queue exceeds 1 MiB limit");
                to_remove.push(id);
                continue;
            }

            while !client.outgoing.is_empty() {
                let (chunk, _) = client.outgoing.as_slices();
                match client.stream.write(chunk) {
                    Ok(0) => {
                        to_remove.push(id);
                        break;
                    }
                    Ok(n) => {
                        client.outgoing.drain(..n);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        break;
                    }
                    Err(_) => {
                        to_remove.push(id);
                        break;
                    }
                }
            }
        }

        for id in to_remove {
            self.clients.remove(&id);
        }
    }

    /// Read incoming bytes from all clients and parse frames.
    /// Returns list of (client_id, ClientMessage).
    pub fn poll_incoming(&mut self) -> Vec<(usize, ClientMessage)> {
        self.accept_incoming();

        let mut messages = Vec::new();
        let mut to_remove = Vec::new();
        let mut buf = [0u8; 8192];

        for (&id, client) in self.clients.iter_mut() {
            loop {
                match client.stream.read(&mut buf) {
                    Ok(0) => {
                        to_remove.push(id);
                        break;
                    }
                    Ok(n) => {
                        if let Err(e) = client.frame_buf.push(&buf[..n]) {
                            tracing::warn!("Client {id} error pushing frame: {e}");
                            to_remove.push(id);
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        break;
                    }
                    Err(_) => {
                        to_remove.push(id);
                        break;
                    }
                }
            }

            while let Ok(Some(msg)) = client.frame_buf.next_frame::<ClientMessage>() {
                messages.push((id, msg));
            }
        }

        for id in to_remove {
            self.clients.remove(&id);
        }

        self.flush_outgoing();
        messages
    }

    /// Send a ServerMessage to a specific client.
    pub fn send_to(&mut self, client_id: usize, msg: &ServerMessage) -> Result<(), IpcError> {
        let bytes = veil_ipc::encode(msg)?;
        if let Some(client) = self.clients.get_mut(&client_id) {
            if client.outgoing.len() + bytes.len() > MAX_SUBSCRIBER_QUEUE {
                self.clients.remove(&client_id);
                return Err(IpcError::MessageTooLarge(MAX_SUBSCRIBER_QUEUE));
            }
            client.outgoing.extend(bytes);
        }
        self.flush_outgoing();
        Ok(())
    }

    /// Is anyone subscribed to `topic`?
    pub fn has_subscriber(&self, topic: &str) -> bool {
        self.clients.values().any(|c| c.subscriptions.contains(topic))
    }

    /// Broadcast an event to all subscribers of `topic`.
    pub fn broadcast(&mut self, topic: &str, data: serde_json::Value) {
        let event = ServerMessage::Event {
            topic: topic.to_string(),
            data,
        };
        let bytes = match veil_ipc::encode(&event) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("Failed to encode broadcast event: {e}");
                return;
            }
        };

        let mut to_drop = Vec::new();
        for (&id, client) in self.clients.iter_mut() {
            if client.subscriptions.contains(topic) {
                if client.outgoing.len() + bytes.len() > MAX_SUBSCRIBER_QUEUE {
                    to_drop.push(id);
                } else {
                    client.outgoing.extend(&bytes);
                }
            }
        }

        for id in to_drop {
            tracing::warn!("Dropped slow IPC subscriber {id}");
            self.clients.remove(&id);
        }

        self.flush_outgoing();
    }

    /// The notification commands. `notifications.set` (from `notify`) is cached
    /// and rebroadcast as the `notifications` event; `notify.dnd` and
    /// `notify.panel` (from keybinds, `veil ctl`, the bar) go to `notify` as a
    /// `notify.control` event. `None` when `cmd` is not one of them.
    pub fn notify_command(
        &mut self,
        cmd: &str,
        args: &serde_json::Value,
    ) -> Option<Result<Option<serde_json::Value>, String>> {
        use veil_ipc::{
            commands as c,
            notify::{Control, NotificationsState},
            topics,
        };
        let control = match cmd {
            c::NOTIFICATIONS_SET => {
                let state = match NotificationsState::from_args(args) {
                    Ok(s) => s,
                    Err(e) => return Some(Err(e)),
                };
                let data = serde_json::to_value(state).unwrap_or_default();
                self.notifications = Some(data.clone());
                self.broadcast(topics::NOTIFICATIONS, data);
                return Some(Ok(None));
            }
            c::NOTIFY_DND => Control::dnd_from_args(args),
            c::NOTIFY_PANEL => Control::panel_from_args(args),
            _ => return None,
        };
        Some(control.and_then(|ctl| {
            // Nobody to tell: say so instead of pretending it worked.
            if !self.has_subscriber(topics::NOTIFY_CONTROL) {
                return Err("notify is not running".into());
            }
            self.broadcast(topics::NOTIFY_CONTROL, serde_json::to_value(ctl).unwrap_or_default());
            Ok(None)
        }))
    }

    /// Handle standard protocol messages (Hello, Subscribe, Ping) internally.
    /// Non-standard or command messages are returned for the compositor to dispatch.
    pub fn process_message(
        &mut self,
        client_id: usize,
        msg: ClientMessage,
        current_palette: serde_json::Value,
        current_config: serde_json::Value,
    ) -> Option<ClientMessage> {
        match msg {
            ClientMessage::Hello { version, name } => {
                if version > PROTOCOL_VERSION {
                    tracing::warn!("Client {client_id} has newer version {version} > {PROTOCOL_VERSION}, rejecting");
                    self.clients.remove(&client_id);
                    return None;
                }
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.name = Some(name.clone());
                }
                let reply = ServerMessage::Hello {
                    version: PROTOCOL_VERSION,
                    palette: current_palette,
                    config: current_config,
                };
                let _ = self.send_to(client_id, &reply);
                None
            }
            ClientMessage::Subscribe { topics } => {
                let wants_notifications = topics.iter().any(|t| t == veil_ipc::topics::NOTIFICATIONS);
                if let Some(client) = self.clients.get_mut(&client_id) {
                    for t in topics {
                        client.subscriptions.insert(t);
                    }
                }
                if wants_notifications {
                    if let Some(data) = self.notifications.clone() {
                        let event = ServerMessage::Event { topic: veil_ipc::topics::NOTIFICATIONS.into(), data };
                        let _ = self.send_to(client_id, &event);
                    }
                }
                None
            }
            ClientMessage::Ping => {
                let _ = self.send_to(client_id, &ServerMessage::Pong);
                None
            }
            other => Some(other),
        }
    }
}

impl Default for IpcServer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        // Only unlink a socket we actually bound; after a failed bind the
        // path belongs to another running veil.
        if self.listener.is_some() && self.socket_path.exists() {
            let _ = fs::remove_file(&self.socket_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn ipc_server_bind_and_connect() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("test.sock");
        let mut server = IpcServer::with_path(sock_path.clone());
        server.bind().unwrap();

        assert!(sock_path.exists());

        // Connect client
        let mut client = UnixStream::connect(&sock_path).unwrap();
        client.set_nonblocking(true).unwrap();

        // Server accepts
        let msgs = server.poll_incoming();
        assert!(msgs.is_empty());
        assert_eq!(server.clients.len(), 1);

        // Send hello from client
        let hello = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            name: "test-client".into(),
        };
        client.write_all(&veil_ipc::encode(&hello).unwrap()).unwrap();

        let msgs = server.poll_incoming();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].1, hello);
    }

    // ---- notification relay

    /// A connected, subscribed test client, plus the server-side bookkeeping.
    fn connect_subscribed(server: &mut IpcServer, path: &std::path::Path, topics: &[&str]) -> UnixStream {
        let client = UnixStream::connect(path).unwrap();
        client.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
        server.poll_incoming();
        let id = *server.clients.keys().max().unwrap();
        let sub = ClientMessage::Subscribe { topics: topics.iter().map(|t| t.to_string()).collect() };
        server.process_message(id, sub, serde_json::Value::Null, serde_json::Value::Null);
        client
    }

    /// Every event the client has received so far.
    fn drain_events(client: &mut UnixStream) -> Vec<(String, serde_json::Value)> {
        let mut fb = FrameBuffer::new();
        let mut buf = [0u8; 4096];
        if let Ok(n) = client.read(&mut buf) {
            fb.push(&buf[..n]).unwrap();
        }
        let mut out = Vec::new();
        while let Ok(Some(ServerMessage::Event { topic, data })) = fb.next_frame::<ServerMessage>() {
            out.push((topic, data));
        }
        out
    }

    fn server_at(dir: &tempfile::TempDir) -> (IpcServer, PathBuf) {
        let path = dir.path().join("n.sock");
        let mut server = IpcServer::with_path(path.clone());
        server.bind().unwrap();
        (server, path)
    }

    #[test]
    fn notifications_set_is_rebroadcast_and_replayed_to_late_subscribers() {
        let dir = tempdir().unwrap();
        let (mut server, path) = server_at(&dir);
        let mut early = connect_subscribed(&mut server, &path, &["notifications"]);

        let r = server.notify_command("notifications.set", &serde_json::json!({"count": 3, "dnd": false}));
        assert!(matches!(r, Some(Ok(None))));
        assert_eq!(
            drain_events(&mut early),
            vec![("notifications".to_string(), serde_json::json!({"count": 3, "dnd": false}))]
        );

        // A bar that starts (or restarts) afterwards gets the current state at once.
        let mut late = connect_subscribed(&mut server, &path, &["workspaces", "notifications"]);
        assert_eq!(
            drain_events(&mut late),
            vec![("notifications".to_string(), serde_json::json!({"count": 3, "dnd": false}))]
        );
        // ...and a client that does not ask for the topic gets nothing.
        let mut other = connect_subscribed(&mut server, &path, &["workspaces"]);
        assert!(drain_events(&mut other).is_empty());
    }

    #[test]
    fn notifications_set_rejects_bad_arguments_and_caches_nothing() {
        let dir = tempdir().unwrap();
        let (mut server, path) = server_at(&dir);
        for bad in [serde_json::Value::Null, serde_json::json!({"count": 1}), serde_json::json!({"count": "x", "dnd": true})] {
            assert!(matches!(server.notify_command("notifications.set", &bad), Some(Err(_))), "{bad}");
        }
        let mut late = connect_subscribed(&mut server, &path, &["notifications"]);
        assert!(drain_events(&mut late).is_empty(), "a rejected update must not become the replayed state");
    }

    #[test]
    fn dnd_and_panel_reach_notify_as_control_events() {
        let dir = tempdir().unwrap();
        let (mut server, path) = server_at(&dir);
        let mut notify = connect_subscribed(&mut server, &path, &["notify.control"]);

        assert!(matches!(server.notify_command("notify.dnd", &serde_json::Value::Null), Some(Ok(None))));
        assert!(matches!(server.notify_command("notify.dnd", &serde_json::json!({"mode": "off"})), Some(Ok(None))));
        assert!(matches!(server.notify_command("notify.panel", &serde_json::json!({"mode": "open"})), Some(Ok(None))));
        assert_eq!(
            drain_events(&mut notify),
            vec![
                ("notify.control".to_string(), serde_json::json!({"dnd": "toggle"})),
                ("notify.control".to_string(), serde_json::json!({"dnd": "off"})),
                ("notify.control".to_string(), serde_json::json!({"panel": "open"})),
            ]
        );
    }

    #[test]
    fn control_commands_fail_loudly_when_notify_is_not_listening() {
        let dir = tempdir().unwrap();
        let (mut server, path) = server_at(&dir);
        assert_eq!(server.notify_command("notify.dnd", &serde_json::Value::Null), Some(Err("notify is not running".into())));
        // A bar subscribed to `notifications` is not a listener for control events.
        let _bar = connect_subscribed(&mut server, &path, &["notifications"]);
        assert_eq!(server.notify_command("notify.panel", &serde_json::Value::Null), Some(Err("notify is not running".into())));
    }

    #[test]
    fn bad_modes_and_foreign_commands_are_handled() {
        let dir = tempdir().unwrap();
        let (mut server, path) = server_at(&dir);
        let mut notify = connect_subscribed(&mut server, &path, &["notify.control"]);
        let r = server.notify_command("notify.dnd", &serde_json::json!({"mode": "maybe"}));
        assert_eq!(r, Some(Err("mode must be on, off or toggle".into())));
        assert!(drain_events(&mut notify).is_empty(), "nothing is sent for a rejected request");
        assert!(server.notify_command("config.reload", &serde_json::Value::Null).is_none(), "not ours");
    }

    /// The real notify client against the real server: the wire format, the
    /// handshake order, the relay, the replay and the reconnect all have to agree.
    #[test]
    fn real_notify_client_and_real_server_agree() {
        use std::time::{Duration, Instant};
        use veil_ipc::notify::{Control, DndMode, NotificationsState};
        use veil_notify::ipc::{Incoming, Ipc, IpcConfig};

        let dir = tempdir().unwrap();
        let (mut server, path) = server_at(&dir);
        let ipc = Ipc::spawn(IpcConfig {
            socket: path.clone(),
            ping: Duration::from_millis(100),
            retry_min: Duration::from_millis(20),
            retry_max: Duration::from_millis(50),
        })
        .unwrap();

        // Drive the server like the compositor tick does, until `done` or 3 s pass.
        let pump = |server: &mut IpcServer, mut done: Box<dyn FnMut(&mut IpcServer) -> bool>| {
            let end = Instant::now() + Duration::from_secs(3);
            while Instant::now() < end {
                for (cid, msg) in server.poll_incoming() {
                    if let Some(ClientMessage::Cmd { cmd, args, .. }) =
                        server.process_message(cid, msg, serde_json::json!({"p": 1}), serde_json::json!({"c": 2}))
                    {
                        let _ = server.notify_command(&cmd, &args);
                    }
                }
                if done(server) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        };

        // The client connects, says hello and subscribes to notify.control.
        assert!(pump(&mut server, Box::new(|s| s.has_subscriber("notify.control"))), "notify never subscribed");
        assert!(matches!(ipc.incoming.recv_timeout(Duration::from_secs(2)), Ok(Incoming::Connected { .. })));
        assert_eq!(server.client_name(*server.clients.keys().next().unwrap()).as_deref(), Some("notify"));

        // A bar subscribes; notify publishes; the bar sees the bell state.
        let mut bar = connect_subscribed(&mut server, &path, &["notifications"]);
        ipc.publish(NotificationsState { count: 4, dnd: false });
        assert!(pump(&mut server, Box::new(|s| s.notifications.is_some())), "the state never arrived");
        assert_eq!(
            drain_events(&mut bar),
            vec![("notifications".to_string(), serde_json::json!({"count": 4, "dnd": false}))]
        );

        // `veil ctl notify.dnd` (no arguments) reaches notify as a toggle.
        server.notify_command("notify.dnd", &serde_json::Value::Null).unwrap().unwrap();
        let got = ipc.incoming.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(got, Incoming::Control(Control { dnd: Some(DndMode::Toggle), panel: None }));

        // A bar that starts later is told the current bell state straight away.
        let mut late = connect_subscribed(&mut server, &path, &["notifications"]);
        assert_eq!(drain_events(&mut late).len(), 1);
    }

    #[test]
    fn subscribe_and_broadcast() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("broadcast.sock");
        let mut server = IpcServer::with_path(sock_path.clone());
        server.bind().unwrap();

        let mut client = UnixStream::connect(&sock_path).unwrap();
        client.set_nonblocking(false).unwrap();

        let msgs = server.poll_incoming();
        assert!(msgs.is_empty());
        let client_id = *server.clients.keys().next().unwrap();

        // Subscribe to workspaces
        let sub = ClientMessage::Subscribe {
            topics: vec!["workspaces".into()],
        };
        server.process_message(client_id, sub, serde_json::Value::Null, serde_json::Value::Null);

        // Broadcast workspaces event
        server.broadcast("workspaces", serde_json::json!({ "active": 2 }));

        let mut fb = FrameBuffer::new();
        let mut buf = [0u8; 1024];
        let n = client.read(&mut buf).unwrap();
        fb.push(&buf[..n]).unwrap();
        let received: ServerMessage = fb.next_frame().unwrap().unwrap();

        assert_eq!(
            received,
            ServerMessage::Event {
                topic: "workspaces".into(),
                data: serde_json::json!({ "active": 2 })
            }
        );
    }
}
