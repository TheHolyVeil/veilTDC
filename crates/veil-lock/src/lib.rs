//! The Veil lock screen: an `ext-session-lock-v1` client (`veil shell lock`).
//!
//! The compositor does the locking (see `veil-host`'s `session_lock`); this
//! client shows the clock and password field, checks the password with PAM,
//! and asks the compositor to unlock.

pub mod auth;
pub mod avatar;
pub mod battery;
pub mod screen;
mod wayland;

use screen::LockScreen;

/// Entry point for `veil shell lock`.
pub fn run_lock() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init().ok();

    // The hello is what tells the supervisor this component is up, so it comes
    // first, and the stream stays open for as long as we run.
    let link = veil_ui::link::connect("lock");
    let user = auth::current_user().ok_or("cannot tell which account to unlock")?;
    if auth::pam_service().is_none() {
        tracing::error!("lock: no PAM service found; the password will never be accepted");
    }
    let screen = LockScreen::new(&link.tokens, link.font.as_ref(), &user, None);
    let avatar_rx = avatar::spawn_load(&user);
    // A second handle on the compositor connection, for the power menu.
    let ipc = link.stream.as_ref().and_then(|s| s.try_clone().ok());
    let result = wayland::run(screen, user, avatar_rx, ipc);
    drop(link);
    result
}
