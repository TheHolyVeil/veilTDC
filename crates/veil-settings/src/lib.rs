//! Settings app for the Veil desktop shell (`veil shell settings`).
//!
//! Built in layers, each testable without a display:
//! - `model`: which settings are on which tab, how they read, how a key
//!   changes them, and what is sent to save one
//! - `app`: state, keys, pointer and drawing, as a `veil_ui::modal` app
//!
//! It reads `config.toml` itself (`veil_config::load_user_config`) to show
//! what is set, and saves by asking the compositor (`config.set`), which
//! validates, edits the file in place so comments survive, and applies the
//! change at once. With no compositor to ask (run on its own) it makes the
//! same edit directly, with the same validation.

pub mod app;
pub mod model;

use std::time::Duration;

use app::{Loader, Sender, SettingsApp};
use serde_json::{json, Value};
use veil_ui::link::Session;

/// How long a save waits for the compositor before it is reported as failed.
const SAVE_TIMEOUT: Duration = Duration::from_secs(3);

/// Entry point for `veil shell settings`.
pub fn run_settings() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init().ok();

    let link = veil_ui::link::connect("settings");
    let mut session = link.stream.map(Session::new);
    let sender: Sender = Box::new(move |args| match session.as_mut() {
        Some(s) => s.request("config.set", args.clone(), SAVE_TIMEOUT),
        None => save_directly(args),
    });
    let loader: Loader = Box::new(veil_config::load_user_config);

    let mut app = SettingsApp::new(veil_config::load_user_config(), link.tokens, link.font.as_ref(), sender, loader);
    app.prewarm();
    veil_ui::modal::run(Box::new(app), veil_ui::modal::Options { namespace: "veil-settings" })
}

/// The compositor's `config.set`, without the compositor: same parser, same
/// schema checks, same writer.
fn save_directly(args: &Value) -> Result<Option<Value>, String> {
    let edits = veil_config::wire::edits_from_args(args)?;
    let report = veil_config::writer::apply(&edits).map_err(|e| e.to_string())?;
    Ok(Some(json!({
        "path": report.path.display().to_string(),
        "shadowed_by": report.shadowed_by.map(|p| p.display().to_string()),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_direct_save_rejects_what_the_compositor_would() {
        assert!(save_directly(&json!({})).is_err());
        assert!(save_directly(&json!({ "set": { "nope": 1 } })).unwrap_err().contains("unknown setting"));
        assert!(save_directly(&json!({ "set": { "bar.height": 9999 } })).unwrap_err().contains("24 to 64"));
    }
}
