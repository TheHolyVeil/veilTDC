//! Supervised desktop bar for the Veil window manager.

pub mod bar;
pub mod widgets;

pub use bar::BarLayout;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use veil_ipc::{ClientMessage, FrameBuffer, ServerMessage};
use veil_ui::{LayerShellClient, SurfacePosition};
use smithay_client_toolkit::shell::wlr_layer::Layer;

/// Run the bar main loop.
pub fn run_bar() -> Result<(), String> {
    tracing::info!("Starting veil-bar...");

    let mut bar = BarLayout::new(36);

    // 1. Initialize Wayland Layer-Shell Client
    let mut client = LayerShellClient::new(
        "veil-bar",
        Layer::Top,
        SurfacePosition::Top,
        36,
        true, // Exclusive zone = 36px
    )?;

    // 2. Connect to Veil IPC Socket
    let socket_path = std::env::var("VEIL_IPC_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| veil_ipc::socket_path());

    let mut ipc_stream = match UnixStream::connect(&socket_path) {
        Ok(s) => {
            let _ = s.set_nonblocking(true);
            Some(s)
        }
        Err(e) => {
            tracing::warn!("Could not connect to Veil IPC socket at {}: {e}", socket_path.display());
            None
        }
    };

    // 3. Send Hello & Subscriptions over IPC
    if let Some(stream) = &mut ipc_stream {
        let hello = ClientMessage::Hello {
            version: veil_ipc::PROTOCOL_VERSION,
            name: "bar".into(),
        };
        let _ = stream.write_all(&veil_ipc::encode(&hello).unwrap_or_default());

        let sub = ClientMessage::Subscribe {
            topics: vec![
                "workspaces".into(),
                "focus".into(),
                "volume".into(),
                "palette".into(),
            ],
        };
        let _ = stream.write_all(&veil_ipc::encode(&sub).unwrap_or_default());
    }

    let mut frame_buf = FrameBuffer::new();
    let mut read_buf = [0u8; 4096];
    let mut last_tick = Instant::now();
    let mut needs_redraw = true;

    // Initial render
    client.present_for_each(|w, h| bar.render(w, h));

    loop {
        // Dispatch Wayland events
        if let Err(e) = client.dispatch() {
            tracing::error!("Wayland dispatch error: {e}");
            break;
        }

        // Process IPC messages
        if let Some(stream) = &mut ipc_stream {
            loop {
                match stream.read(&mut read_buf) {
                    Ok(0) => {
                        tracing::warn!("IPC socket closed by compositor");
                        break;
                    }
                    Ok(n) => {
                        let _ = frame_buf.push(&read_buf[..n]);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("IPC read error: {e}");
                        break;
                    }
                }
            }

            while let Ok(Some(msg)) = frame_buf.next_frame::<ServerMessage>() {
                match msg {
                    ServerMessage::Event { topic, data } => {
                        if topic == "workspaces" {
                            let active = data.get("active").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                            let occupied = data
                                .get("occupied")
                                .and_then(|v| v.as_array())
                                .map(|arr| arr.iter().filter_map(|v| v.as_u64().map(|n| n as usize)).collect())
                                .unwrap_or_default();
                            bar.on_workspaces(active, occupied);
                            needs_redraw = true;
                        } else if topic == "focus" {
                            let title = data
                                .get("title")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            bar.on_focus(title);
                            needs_redraw = true;
                        } else if topic == "volume" {
                            let vol = data.get("volume").and_then(|v| v.as_u64()).unwrap_or(50) as u8;
                            let muted = data.get("muted").and_then(|v| v.as_bool()).unwrap_or(false);
                            bar.on_volume(vol, muted);
                            needs_redraw = true;
                        }
                    }
                    ServerMessage::Pong => {
                        // Pong received, healthy
                    }
                    _ => {}
                }
            }
        }

        // Check widget tick updates
        let now = Instant::now();
        if now.duration_since(last_tick) >= Duration::from_millis(500) {
            last_tick = now;
            if bar.update(now) {
                needs_redraw = true;
            }
        }

        // Redraw when state changed
        if needs_redraw {
            needs_redraw = false;
            client.present_for_each(|w, h| bar.render(w, h));
        }

        // Sleep to cap wakeups
        std::thread::sleep(Duration::from_millis(50));
    }

    Ok(())
}
