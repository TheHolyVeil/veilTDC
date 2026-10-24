//! veil: Multi-call binary for the Veil desktop ecosystem.
//!
//! Handles:
//! - Default / `start` / `run`: Master Wayland compositor
//! - `shell <component>`: Supervised child components (bar, notify, osd, etc.)
//! - `ctl <command> [args]`: Unix IPC control CLI for scripts and settings

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // 1. Check for `shell <component>`
    if args.len() > 1 && args[1] == "shell" {
        #[cfg(feature = "shell")]
        {
            let component = args.get(2).map(|s| s.as_str()).unwrap_or("");
            match component {
                "bar" => {
                    if let Err(e) = veil_bar::run_bar() {
                        eprintln!("[veil shell bar] Error: {e}");
                        std::process::exit(1);
                    }
                    return Ok(());
                }
                other => {
                    eprintln!("Unknown shell component: {other}");
                    eprintln!("Available components: bar");
                    std::process::exit(1);
                }
            }
        }
        #[cfg(not(feature = "shell"))]
        {
            eprintln!("Shell feature is disabled in this build");
            std::process::exit(1);
        }
    }

    // 2. Check for `ctl <command>`
    if args.len() > 1 && args[1] == "ctl" {
        return handle_ctl(&args[2..]);
    }

    // 3. Fallback: launch compositor via veil-host
    veil_host::cli::run_cli()
}

fn handle_ctl(args: &[String]) -> std::io::Result<()> {
    if args.is_empty() || args[0] == "--help" {
        println!("veil ctl — command line IPC control for Veil");
        println!();
        println!("USAGE:");
        println!("  veil ctl <command> [args...]");
        println!();
        println!("COMMANDS:");
        println!("  ping                    Send ping to compositor");
        println!("  reload                  Reload config.lua");
        println!("  restart <component>     Restart a supervised component");
        println!("  workspace <num>         Switch to workspace number");
        return Ok(());
    }

    let sock_path = std::env::var("VEIL_IPC_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| veil_ipc::socket_path());

    let mut stream = UnixStream::connect(&sock_path)?;

    // Send Hello first
    let hello = veil_ipc::ClientMessage::Hello {
        version: veil_ipc::PROTOCOL_VERSION,
        name: "veil-ctl".into(),
    };
    stream.write_all(&veil_ipc::encode(&hello).map_err(|e| std::io::Error::other(e.to_string()))?)?;

    let cmd_name = &args[0];
    let msg = match cmd_name.as_str() {
        "ping" => veil_ipc::ClientMessage::Ping,
        "reload" => veil_ipc::ClientMessage::Cmd {
            id: 1,
            cmd: "config.reload".into(),
            args: serde_json::Value::Null,
        },
        "restart" => veil_ipc::ClientMessage::Cmd {
            id: 1,
            cmd: "component.restart".into(),
            args: serde_json::json!({ "name": args.get(1).unwrap_or(&"".into()) }),
        },
        "workspace" => {
            let n: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
            veil_ipc::ClientMessage::Cmd {
                id: 1,
                cmd: "workspace.switch".into(),
                args: serde_json::json!({ "n": n }),
            }
        }
        custom => veil_ipc::ClientMessage::Cmd {
            id: 1,
            cmd: custom.into(),
            args: serde_json::Value::Null,
        },
    };

    stream.write_all(&veil_ipc::encode(&msg).map_err(|e| std::io::Error::other(e.to_string()))?)?;

    // Read response
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf)?;
    let reply = String::from_utf8_lossy(&buf[..n]);
    println!("{}", reply.trim());

    Ok(())
}
