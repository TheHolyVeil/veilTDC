//! Supervised app launcher for the Veil desktop shell (`veil shell launch`).
//!
//! Built in layers, each testable without a display:
//! - `desktop`: what can be launched (XDG `.desktop` discovery and `Exec=`)
//! - `search`: ranking those apps for what was typed
//! - `calc`: the calculator row
//! - `input`: the search box's text and cursor
//! - `history`: what is launched often, so it comes first
//! - `icons`: finding and drawing each app's icon
//! - `app`: state, keys, pointer and drawing, as a `veil_ui::modal` app

pub mod app;
pub mod calc;
pub mod desktop;
pub mod history;
pub mod icons;
pub mod input;
pub mod search;

use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use app::{LauncherApp, Run};
use history::History;

/// Entry point for `veil shell launch`.
pub fn run_launch() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init().ok();

    let link = veil_ui::link::connect("launch");
    let apps = desktop::scan(&desktop::ScanEnv::from_env());
    let history = History::default_path().map(|p| History::load(&p)).unwrap_or_else(History::in_memory);

    let mut stream = link.stream;
    let mut app = LauncherApp::new(apps, history, link.tokens, link.font.as_ref(), Box::new(move |run| execute(&mut stream, run)));
    app.set_icons(icons::IconCache::new(icons::IconEnv::from_env()));
    app.prewarm();
    veil_ui::modal::run(Box::new(app), veil_ui::modal::Options { namespace: "veil-launch" })
}

/// Carry out a chosen row. Commands go through the compositor, which sets the
/// Wayland and XWayland environment and owns the child; only without one is
/// the process started here, in its own process group so it outlives us.
fn execute(stream: &mut Option<UnixStream>, run: Run) {
    match run {
        Run::Exec(exec) => {
            if let Some(s) = stream.as_mut() {
                if veil_ui::link::send_cmd(s, "launch", serde_json::json!({ "exec": exec })) {
                    return;
                }
            }
            let _ = Command::new("sh").arg("-c").arg(exec).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn();
        }
        Run::Copy(text) => {
            // wl-copy keeps serving the selection from its own background process.
            let _ = Command::new("wl-copy").arg("--").arg(text).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        }
    }
}
