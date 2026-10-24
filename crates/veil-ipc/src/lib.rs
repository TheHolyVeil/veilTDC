//! Veil IPC: message types, newline-JSON codec, and protocol constants.
//!
//! Master and supervised shell components communicate over a Unix domain
//! socket using newline-delimited JSON.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_MESSAGE_SIZE: usize = 65536; // 64 KiB
pub const MAX_SUBSCRIBER_QUEUE: usize = 1024 * 1024; // 1 MiB

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("Message exceeds maximum allowed size ({0} > {MAX_MESSAGE_SIZE})")]
    MessageTooLarge(usize),
    #[error("Incompatible protocol version: client spoke {0}, server is {PROTOCOL_VERSION}")]
    VersionMismatch(u32),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid message format: {0}")]
    InvalidFormat(String),
}

/// Standard event topic names.
pub mod topics {
    pub const WORKSPACES: &str = "workspaces";
    pub const FOCUS: &str = "focus";
    pub const OUTPUTS: &str = "outputs";
    pub const PALETTE: &str = "palette";
    pub const CONFIG: &str = "config";
    pub const VOLUME: &str = "volume";
    pub const BRIGHTNESS: &str = "brightness";
    pub const NOTIFICATIONS: &str = "notifications";
}

/// Standard command names.
pub mod commands {
    pub const WORKSPACE_SWITCH: &str = "workspace.switch";
    pub const LAUNCH: &str = "launch";
    pub const CONFIG_SET: &str = "config.set";
    pub const CONFIG_RELOAD: &str = "config.reload";
    pub const WALLPAPER_SET: &str = "wallpaper.set";
    pub const VOLUME_STEP: &str = "volume.step";
    pub const BRIGHTNESS_STEP: &str = "brightness.step";
    pub const COMPONENT_TOGGLE: &str = "component.toggle";
    pub const COMPONENT_RESTART: &str = "component.restart";
    pub const SESSION_LOCK: &str = "session.lock";
    pub const SESSION_LOGOUT: &str = "session.logout";
    pub const SESSION_SUSPEND: &str = "session.suspend";
    pub const SESSION_REBOOT: &str = "session.reboot";
    pub const SESSION_POWEROFF: &str = "session.poweroff";
}

/// Messages sent from Client to Compositor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        version: u32,
        name: String,
    },
    Subscribe {
        topics: Vec<String>,
    },
    Cmd {
        id: u64,
        cmd: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    Ping,
}

/// Messages sent from Compositor to Client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        version: u32,
        #[serde(default)]
        palette: serde_json::Value,
        #[serde(default)]
        config: serde_json::Value,
    },
    Event {
        topic: String,
        data: serde_json::Value,
    },
    Reply {
        id: u64,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    Pong,
}

/// Default socket path inside `$XDG_RUNTIME_DIR/veil/ipc.sock`.
pub fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("veil").join("ipc.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/veil-{uid}")).join("ipc.sock")
    }
}

/// Palette file path inside `$XDG_RUNTIME_DIR/veil/palette.json`.
pub fn palette_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("veil").join("palette.json")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/veil-{uid}")).join("palette.json")
    }
}

/// Encode a message to a newline-terminated JSON byte vector, enforcing MAX_MESSAGE_SIZE.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, IpcError> {
    let mut bytes = serde_json::to_vec(msg)?;
    if bytes.len() + 1 > MAX_MESSAGE_SIZE {
        return Err(IpcError::MessageTooLarge(bytes.len() + 1));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Decode a message from a byte slice (with or without trailing newline).
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, IpcError> {
    if bytes.len() > MAX_MESSAGE_SIZE {
        return Err(IpcError::MessageTooLarge(bytes.len()));
    }
    let trimmed = if bytes.ends_with(b"\n") {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    Ok(serde_json::from_slice(trimmed)?)
}

/// Helper for reading newline-delimited frames from a stream.
#[derive(Debug)]
pub struct FrameBuffer {
    buffer: Vec<u8>,
}

impl Default for FrameBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameBuffer {
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(1024),
        }
    }

    /// Append incoming bytes from a socket read.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), IpcError> {
        if self.buffer.len() + bytes.len() > MAX_MESSAGE_SIZE * 2 {
            return Err(IpcError::MessageTooLarge(self.buffer.len() + bytes.len()));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// Extract the next newline-delimited frame, if any.
    pub fn next_frame<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        if let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let line = self.buffer.drain(..=pos).collect::<Vec<u8>>();
            let msg = decode(&line)?;
            Ok(Some(msg))
        } else {
            if self.buffer.len() > MAX_MESSAGE_SIZE {
                return Err(IpcError::MessageTooLarge(self.buffer.len()));
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_client_hello() {
        let msg = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            name: "bar".into(),
        };
        let encoded = encode(&msg).unwrap();
        assert!(encoded.ends_with(b"\n"));
        let decoded: ClientMessage = decode(&encoded).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_server_reply() {
        let msg = ServerMessage::Reply {
            id: 42,
            ok: true,
            error: None,
            data: Some(serde_json::json!({ "active": 3 })),
        };
        let encoded = encode(&msg).unwrap();
        let decoded: ServerMessage = decode(&encoded).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_ping_pong() {
        let ping = ClientMessage::Ping;
        let pong = ServerMessage::Pong;
        assert_eq!(decode::<ClientMessage>(&encode(&ping).unwrap()).unwrap(), ping);
        assert_eq!(decode::<ServerMessage>(&encode(&pong).unwrap()).unwrap(), pong);
    }

    #[test]
    fn frame_buffer_handles_fragmented_and_batched_frames() {
        let mut fb = FrameBuffer::new();
        let msg1 = ClientMessage::Ping;
        let msg2 = ClientMessage::Hello {
            version: 1,
            name: "test".into(),
        };

        let mut enc1 = encode(&msg1).unwrap();
        let enc2 = encode(&msg2).unwrap();

        // Push partial first message
        let split = enc1.len() / 2;
        fb.push(&enc1[..split]).unwrap();
        assert!(fb.next_frame::<ClientMessage>().unwrap().is_none());

        // Push rest of first message + part of second
        let mut combined = enc1.split_off(split);
        combined.extend_from_slice(&enc2);
        fb.push(&combined).unwrap();

        let res1 = fb.next_frame::<ClientMessage>().unwrap();
        assert_eq!(res1, Some(msg1));

        let res2 = fb.next_frame::<ClientMessage>().unwrap();
        assert_eq!(res2, Some(msg2));

        assert!(fb.next_frame::<ClientMessage>().unwrap().is_none());
    }

    #[test]
    fn enforce_max_message_size() {
        let big_payload = vec![b'a'; MAX_MESSAGE_SIZE + 10];
        assert!(encode(&big_payload).is_err());
    }
}
