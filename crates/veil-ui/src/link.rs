//! The one-time handshake an on-demand shell component does with the
//! compositor: connect, say hello, take the palette and font from the reply.
//!
//! Without a compositor (the component run on its own for a quick look) every
//! part has a default, so the component still works, just unthemed and with
//! no IPC to send commands to.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use veil_ipc::{ClientMessage, FrameBuffer, ServerMessage};
use veil_palette::Tokens;
use veil_text::FontConfig;

pub struct Link {
    /// Open when the compositor answered; commands go out on it.
    pub stream: Option<UnixStream>,
    pub tokens: Tokens,
    pub font: Option<FontConfig>,
}

/// Connect as `name` using `VEIL_IPC_SOCKET` or the default socket.
pub fn connect(name: &str) -> Link {
    let path = std::env::var("VEIL_IPC_SOCKET").map(std::path::PathBuf::from).unwrap_or_else(|_| veil_ipc::socket_path());
    connect_to(&path, name, Duration::from_millis(200))
}

/// How long the hello reply is waited for: a live compositor answers at once.
pub fn connect_to(path: &Path, name: &str, reply_wait: Duration) -> Link {
    let mut link = Link { stream: None, tokens: Tokens::default(), font: None };
    let mut stream = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("{name}: no compositor at {} ({e})", path.display());
            return link;
        }
    };
    let _ = stream.set_read_timeout(Some(reply_wait));

    let hello = ClientMessage::Hello { version: veil_ipc::PROTOCOL_VERSION, name: name.into() };
    if veil_ipc::encode(&hello).is_ok_and(|b| stream.write_all(&b).is_ok()) {
        read_hello(&mut stream, &mut link);
    }
    // Commands are written later and must not stall on a slow reader.
    let _ = stream.set_read_timeout(None);
    link.stream = Some(stream);
    link
}

fn read_hello(stream: &mut UnixStream, link: &mut Link) {
    let mut frames = FrameBuffer::new();
    let mut buf = [0u8; 4096];
    // A reply can arrive split; give it a few reads before giving up.
    for _ in 0..4 {
        let Ok(n) = stream.read(&mut buf) else { return };
        if n == 0 || frames.push(&buf[..n]).is_err() {
            return;
        }
        while let Ok(Some(msg)) = frames.next_frame::<ServerMessage>() {
            if let ServerMessage::Hello { palette, config, .. } = msg {
                if let Some(obj) = palette.as_object() {
                    link.tokens = Tokens::from_hex_lookup(|k| obj.get(k).and_then(|v| v.as_str()));
                }
                if let Some(f) = config.get("font") {
                    link.font = Some(FontConfig::from_json(f));
                }
                return;
            }
        }
    }
}

/// Send one fire-and-forget command to the compositor.
pub fn send_cmd(stream: &mut UnixStream, cmd: &str, args: serde_json::Value) -> bool {
    let msg = ClientMessage::Cmd { id: 1, cmd: cmd.to_string(), args };
    veil_ipc::encode(&msg).is_ok_and(|b| stream.write_all(&b).is_ok())
}

/// A connection that can ask for something and wait for the answer, for the
/// components that must know whether a command worked (the settings app).
///
/// Keeps one frame buffer for the life of the connection, so a message that
/// arrives split across reads is never lost between requests.
pub struct Session {
    stream: UnixStream,
    frames: FrameBuffer,
    next_id: u64,
}

impl Session {
    pub fn new(stream: UnixStream) -> Self {
        Self { stream, frames: FrameBuffer::new(), next_id: 1000 }
    }

    /// Send `cmd` and wait up to `wait` for its reply: the reply's data on
    /// success, the compositor's error message (or a description of what went
    /// wrong with the connection) otherwise. Unrelated messages that arrive
    /// meanwhile are skipped.
    pub fn request(&mut self, cmd: &str, args: serde_json::Value, wait: Duration) -> Result<Option<serde_json::Value>, String> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = ClientMessage::Cmd { id, cmd: cmd.to_string(), args };
        let bytes = veil_ipc::encode(&msg).map_err(|e| format!("could not encode {cmd}: {e}"))?;
        self.stream.write_all(&bytes).map_err(|e| format!("lost the compositor connection: {e}"))?;

        let deadline = Instant::now() + wait;
        let mut buf = [0u8; 4096];
        let result = loop {
            loop {
                match self.frames.next_frame::<ServerMessage>() {
                    Ok(Some(ServerMessage::Reply { id: got, ok, error, data })) if got == id => {
                        return self.finish(if ok { Ok(data) } else { Err(error.unwrap_or_else(|| format!("{cmd} failed"))) });
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => return self.finish(Err(format!("unreadable reply from the compositor: {e}"))),
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err("the compositor did not answer".to_string());
            }
            let _ = self.stream.set_read_timeout(Some(left));
            match self.stream.read(&mut buf) {
                Ok(0) => break Err("the compositor closed the connection".to_string()),
                Ok(n) => {
                    if let Err(e) = self.frames.push(&buf[..n]) {
                        break Err(format!("unreadable reply from the compositor: {e}"));
                    }
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => {}
                Err(e) => break Err(format!("lost the compositor connection: {e}")),
            }
        };
        self.finish(result)
    }

    fn finish<T>(&mut self, r: T) -> T {
        let _ = self.stream.set_read_timeout(None);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    #[test]
    fn no_compositor_gives_defaults_and_no_stream() {
        let l = connect_to(Path::new("/definitely/not/a/socket"), "t", Duration::from_millis(10));
        assert!(l.stream.is_none() && l.font.is_none());
    }

    #[test]
    fn takes_palette_and_font_from_the_hello_and_can_send_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.contains(r#""name":"launch""#), "{line}");
            let reply = ServerMessage::Hello {
                version: veil_ipc::PROTOCOL_VERSION,
                palette: serde_json::json!({ "accent": "#112233" }),
                config: serde_json::json!({ "font": { "size": 18.0 } }),
            };
            conn.write_all(&veil_ipc::encode(&reply).unwrap()).unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            line
        });

        let mut link = connect_to(&path, "launch", Duration::from_millis(500));
        assert_eq!(link.font.as_ref().map(|f| f.size), Some(18.0));
        assert_ne!(link.tokens, Tokens::default(), "palette taken from the reply");
        let stream = link.stream.as_mut().unwrap();
        assert!(send_cmd(stream, "launch", serde_json::json!({ "exec": "foot" })));
        let got = server.join().unwrap();
        assert!(got.contains(r#""cmd":"launch""#) && got.contains(r#""exec":"foot""#), "{got}");
    }

    #[test]
    fn a_silent_compositor_does_not_hang_the_component() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let t = std::time::Instant::now();
        let l = connect_to(&path, "t", Duration::from_millis(50));
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(l.stream.is_some(), "still usable for sending");
    }

    /// A fake compositor: for each command line it gets, run `answer(id, line)`
    /// and write back whatever messages that returns.
    fn serve(listener: UnixListener, answer: impl Fn(u64, &str) -> Vec<ServerMessage> + Send + 'static) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                let id = v["id"].as_u64().unwrap();
                for m in answer(id, &line) {
                    conn.write_all(&veil_ipc::encode(&m).unwrap()).unwrap();
                }
                line.clear();
            }
        })
    }

    fn session_pair(tag: &str) -> (tempfile::TempDir, UnixListener, Session) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{tag}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        let session = Session::new(UnixStream::connect(&path).unwrap());
        (dir, listener, session)
    }

    #[test]
    fn a_request_returns_the_reply_data_and_skips_unrelated_messages() {
        let (_dir, listener, mut s) = session_pair("ok");
        serve(listener, |id, line| {
            assert!(line.contains(r#""cmd":"config.set""#), "{line}");
            vec![
                ServerMessage::Event { topic: "focus".into(), data: serde_json::json!({}) },
                ServerMessage::Reply { id: id + 7, ok: false, error: Some("someone else's".into()), data: None },
                ServerMessage::Reply { id, ok: true, error: None, data: Some(serde_json::json!({ "path": "/x" })) },
            ]
        });
        let got = s.request("config.set", serde_json::json!({ "set": { "fps": 60 } }), Duration::from_secs(2)).unwrap();
        assert_eq!(got, Some(serde_json::json!({ "path": "/x" })));
    }

    #[test]
    fn a_failed_command_returns_the_compositors_message() {
        let (_dir, listener, mut s) = session_pair("err");
        serve(listener, |id, _| vec![ServerMessage::Reply { id, ok: false, error: Some("`fps` must be 0 to 1000".into()), data: None }]);
        let err = s.request("config.set", serde_json::json!({}), Duration::from_secs(2)).unwrap_err();
        assert_eq!(err, "`fps` must be 0 to 1000");
    }

    #[test]
    fn requests_can_follow_each_other_on_one_connection() {
        let (_dir, listener, mut s) = session_pair("seq");
        serve(listener, |id, _| vec![ServerMessage::Reply { id, ok: true, error: None, data: Some(serde_json::json!(id)) }]);
        let a = s.request("a", serde_json::json!({}), Duration::from_secs(2)).unwrap();
        let b = s.request("b", serde_json::json!({}), Duration::from_secs(2)).unwrap();
        assert_ne!(a, b, "each request gets its own id");
    }

    #[test]
    fn a_silent_compositor_times_out_instead_of_hanging() {
        let (_dir, listener, mut s) = session_pair("quiet");
        serve(listener, |_, _| vec![]);
        let t = Instant::now();
        let err = s.request("config.set", serde_json::json!({}), Duration::from_millis(80)).unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_closed_connection_is_reported() {
        let (_dir, listener, mut s) = session_pair("closed");
        drop(listener);
        // The peer end was never accepted; dropping the listener closes the queued connection.
        let err = s.request("x", serde_json::json!({}), Duration::from_millis(200)).unwrap_err();
        assert!(!err.is_empty());
    }
}
