//! Supervised power menu overlay for the Veil desktop shell.

pub mod panel;
pub mod surface;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use surface::PowerSurface;
use veil_ipc::{ClientMessage, FrameBuffer, ServerMessage};
use veil_palette::Tokens;
use veil_text::FontConfig;

/// Entry point for `veil shell power`.
pub fn run_power() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let (ipc_stream, tokens, font_cfg) = connect_ipc();

    let mut surf = PowerSurface::new(tokens, font_cfg.as_ref(), ipc_stream)?;
    while !surf.tick()? {}

    Ok(())
}

fn connect_ipc() -> (Option<UnixStream>, Tokens, Option<FontConfig>) {
    let socket_path = std::env::var("VEIL_IPC_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| veil_ipc::socket_path());

    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("veil-power: IPC socket connect failed: {e}");
            return (None, Tokens::default(), None);
        }
    };

    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));

    let hello = ClientMessage::Hello {
        version: veil_ipc::PROTOCOL_VERSION,
        name: "power".into(),
    };
    if let Ok(bytes) = veil_ipc::encode(&hello) {
        let _ = stream.write_all(&bytes);
    }

    let mut frames = FrameBuffer::new();
    let mut buf = [0u8; 4096];
    let mut tokens = Tokens::default();
    let mut font_cfg = None;

    if let Ok(n) = stream.read(&mut buf) {
        let _ = frames.push(&buf[..n]);
        while let Ok(Some(msg)) = frames.next_frame::<ServerMessage>() {
            if let ServerMessage::Hello { palette, config, .. } = msg {
                if let Some(obj) = palette.as_object() {
                    tokens = Tokens::from_hex_lookup(|k| obj.get(k).and_then(|v| v.as_str()));
                }
                if let Some(f) = config.get("font") {
                    font_cfg = Some(FontConfig::from_json(f));
                }
                break;
            }
        }
    }

    (Some(stream), tokens, font_cfg)
}
