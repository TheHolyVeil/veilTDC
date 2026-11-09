//! Supervised power menu overlay for the Veil desktop shell.

pub mod panel;
pub mod surface;

use surface::PowerApp;

/// Entry point for `veil shell power`.
pub fn run_power() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init().ok();

    let link = veil_ui::link::connect("power");
    let app = PowerApp::new(link.tokens, link.font.as_ref(), link.stream);
    veil_ui::modal::run(Box::new(app), veil_ui::modal::Options { namespace: "veil-power" })
}
