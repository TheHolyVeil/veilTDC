//! The one-time handshake an on-demand shell component does with the
//! compositor: connect, say hello, take the palette and font from the reply.
//!
//! Without a compositor (the component run on its own for a quick look) every
//! part has a default, so the component still works, just unthemed and with
//! no IPC to send commands to.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

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
}
