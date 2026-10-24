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
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Bind the Unix socket at mode 0600.
    pub fn bind(&mut self) -> Result<(), std::io::Error> {
        if let Some(parent) = self.socket_path.parent() {
            fs::create_dir_all(parent)?;
            // Parent dir mode 0700 (user-only)
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }

        // Clean up stale socket if present
        if self.socket_path.exists() {
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
                if let Some(client) = self.clients.get_mut(&client_id) {
                    for t in topics {
                        client.subscriptions.insert(t);
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
        if self.socket_path.exists() {
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
