//! veil-osd's side of the Veil IPC. One thread, one non-blocking socket: the
//! main loop polls `fd()` together with the Wayland fd, so an idle OSD wakes
//! once per ping and for nothing else.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use veil_ipc::osd::OsdEvent;
use veil_ipc::{encode, topics, ClientMessage, FrameBuffer, ServerMessage, PROTOCOL_VERSION};

/// The name sent in `hello`; the supervisor matches it to the component.
pub const NAME: &str = "osd";
/// The supervisor's limit is 10 s of silence; the bar and notify use 3 s too.
pub const PING_EVERY: Duration = Duration::from_secs(3);

#[derive(Debug, PartialEq)]
pub enum Incoming {
    Hello { palette: Value, config: Value },
    Osd(OsdEvent),
    Palette(Value),
    Config(Value),
}

pub struct Client {
    stream: UnixStream,
    frames: FrameBuffer,
    next_ping: Instant,
}

fn io_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> io::Error {
    io::Error::other(e)
}

impl Client {
    pub fn connect(path: &Path, now: Instant) -> io::Result<Client> {
        let mut stream = UnixStream::connect(path)?;
        let hello = ClientMessage::Hello { version: PROTOCOL_VERSION, name: NAME.into() };
        stream.write_all(&encode(&hello).map_err(io_err)?)?;
        let subscribe = ClientMessage::Subscribe {
            topics: vec![topics::OSD.into(), topics::PALETTE.into(), topics::CONFIG.into()],
        };
        stream.write_all(&encode(&subscribe).map_err(io_err)?)?;
        stream.set_nonblocking(true)?;
        Ok(Client { stream, frames: FrameBuffer::new(), next_ping: now + PING_EVERY })
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    pub fn next_ping(&self) -> Instant {
        self.next_ping
    }

    /// Send the heartbeat when it is due.
    pub fn ping_if_due(&mut self, now: Instant) -> io::Result<()> {
        if now < self.next_ping {
            return Ok(());
        }
        self.next_ping = now + PING_EVERY;
        match self.stream.write_all(&encode(&ClientMessage::Ping).map_err(io_err)?) {
            Ok(()) => Ok(()),
            // A full socket buffer means the compositor is not reading; the
            // supervisor will notice the missing pong and deal with us.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Everything the compositor has sent. Errors when the connection is gone.
    pub fn read(&mut self) -> io::Result<Vec<Incoming>> {
        let mut buf = [0u8; 4096];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "compositor closed the socket")),
                Ok(n) => self.frames.push(&buf[..n]).map_err(io_err)?,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let mut out = Vec::new();
        while let Some(msg) = self.frames.next_frame::<ServerMessage>().map_err(io_err)? {
            if let ServerMessage::Hello { version, .. } = &msg {
                if *version > PROTOCOL_VERSION {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("compositor speaks protocol {version}, this OSD speaks {PROTOCOL_VERSION}")));
                }
            }
            if let Some(i) = convert(msg) {
                out.push(i);
            }
        }
        Ok(out)
    }
}

fn convert(msg: ServerMessage) -> Option<Incoming> {
    match msg {
        ServerMessage::Hello { palette, config, .. } => Some(Incoming::Hello { palette, config }),
        ServerMessage::Event { topic, data } => match topic.as_str() {
            topics::OSD => OsdEvent::from_json(&data).map(Incoming::Osd),
            topics::PALETTE => Some(Incoming::Palette(data)),
            topics::CONFIG => Some(Incoming::Config(data)),
            _ => None,
        },
        ServerMessage::Reply { .. } | ServerMessage::Pong => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    fn event(topic: &str, data: Value) -> ServerMessage {
        ServerMessage::Event { topic: topic.into(), data }
    }

    #[test]
    fn only_the_subscribed_topics_come_through() {
        let ev = OsdEvent::new("Volume", "", Some(30), 2000, "");
        assert_eq!(convert(event("osd", ev.to_json())), Some(Incoming::Osd(ev)));
        assert!(matches!(convert(event("palette", serde_json::json!({}))), Some(Incoming::Palette(_))));
        assert!(matches!(convert(event("config", serde_json::json!({}))), Some(Incoming::Config(_))));
        assert_eq!(convert(event("workspaces", serde_json::json!({}))), None);
        assert_eq!(convert(ServerMessage::Pong), None);
        // a malformed osd payload is dropped, not fatal
        assert_eq!(convert(event("osd", serde_json::json!({ "body": "no title" }))), None);
    }

    #[test]
    fn talks_to_a_server_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let now = Instant::now();
        let mut client = Client::connect(&path, now).unwrap();

        let (conn, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains(r#""t":"hello""#) && line.contains(r#""name":"osd""#), "{line}");
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains(r#""osd""#) && line.contains(r#""palette""#) && line.contains(r#""config""#), "{line}");
        assert!(!line.contains("volume"), "must not subscribe to volume: {line}");

        let mut conn = conn;
        let ev = OsdEvent::new("Brightness", "", Some(70), 2000, "veil-host-0");
        for m in [ServerMessage::Hello { version: PROTOCOL_VERSION, palette: serde_json::json!({}), config: serde_json::json!({}) }, event("osd", ev.to_json())] {
            conn.write_all(&encode(&m).unwrap()).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        let got = client.read().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], Incoming::Osd(ev));

        // heartbeat only when due
        client.ping_if_due(now).unwrap();
        client.ping_if_due(now + PING_EVERY).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains(r#""t":"ping""#), "{line}");

        // a closed socket is an error, so the main loop reconnects
        drop(conn);
        drop(reader);
        std::thread::sleep(Duration::from_millis(50));
        assert!(client.read().is_err());
    }

    #[test]
    fn a_newer_protocol_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut client = Client::connect(&path, Instant::now()).unwrap();
        let (mut conn, _) = listener.accept().unwrap();
        let m = ServerMessage::Hello { version: PROTOCOL_VERSION + 1, palette: Value::Null, config: Value::Null };
        conn.write_all(&encode(&m).unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(client.read().unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
