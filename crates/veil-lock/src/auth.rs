//! PAM authentication for the lock screen.
//!
//! Authentication only: the session is already open, so there is no
//! `open_session` and no account-management step to trip over (an expired
//! password must not leave you unable to unlock your own screen).

use std::ffi::{CStr, CString};
use std::path::Path;

use pam_client2::{Context, ConversationHandler, ErrorCode, Flag};
use zeroize::Zeroizing;

/// PAM services tried in order: ours, then the two every distro ships.
const SERVICES: [&str; 3] = ["veil-lock", "login", "system-auth"];

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Granted,
    /// Wrong credentials (or a refusal such as a faillock lockout), with the
    /// text to show.
    Denied(String),
    /// PAM itself could not run.
    Error(String),
}

/// The account being unlocked.
pub fn current_user() -> Option<String> {
    uzers::get_current_username()
        .and_then(|n| n.into_string().ok())
        .or_else(|| std::env::var("USER").ok())
        .filter(|u| !u.is_empty())
}

/// First PAM service that has a config file. If none does PAM would fall back
/// to `other`, which denies everything: report that instead of a "wrong
/// password" the user cannot fix by retyping.
pub fn pam_service() -> Option<&'static str> {
    SERVICES.into_iter().find(|s| Path::new("/etc/pam.d").join(s).exists())
}

struct Conv {
    user: CString,
    password: CString,
    messages: Vec<String>,
}

impl ConversationHandler for Conv {
    fn prompt_echo_on(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
        Ok(self.user.clone())
    }

    fn prompt_echo_off(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
        Ok(self.password.clone())
    }

    // pam_u2f's "touch your security key" and faillock's notices.
    fn text_info(&mut self, msg: &CStr) {
        self.messages.push(msg.to_string_lossy().into_owned());
    }

    fn error_msg(&mut self, msg: &CStr) {
        self.messages.push(msg.to_string_lossy().into_owned());
    }
}

/// Check `password` for `user`. Blocks (PAM adds its own failure delay), so
/// call it off the UI thread.
pub fn authenticate(user: &str, password: Zeroizing<String>) -> Outcome {
    let Some(service) = pam_service() else {
        return Outcome::Error("No PAM service for the lock screen (install /etc/pam.d/veil-lock)".into());
    };
    let (Ok(user_c), Ok(pw_c)) = (CString::new(user), CString::new(password.as_bytes())) else {
        return Outcome::Denied("Wrong password".into());
    };
    let conv = Conv { user: user_c, password: pw_c, messages: Vec::new() };
    let mut ctx = match Context::new(service, Some(user), conv) {
        Ok(c) => c,
        Err(e) => return Outcome::Error(format!("PAM: {e}")),
    };
    match ctx.authenticate(Flag::NONE) {
        Ok(()) => Outcome::Granted,
        Err(e) => {
            let extra = ctx.conversation().messages.last().cloned();
            match e.code() {
                ErrorCode::AUTH_ERR | ErrorCode::USER_UNKNOWN => {
                    Outcome::Denied(extra.unwrap_or_else(|| "Wrong password".into()))
                }
                // Locked out by faillock, account restrictions and the like:
                // PAM's own words are the useful ones.
                _ => Outcome::Denied(extra.unwrap_or_else(|| e.to_string())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_with_a_nul_byte_is_denied_not_truncated() {
        // Truncating at the NUL would authenticate a different, shorter password.
        let pw = Zeroizing::new("abc\0def".to_string());
        let out = authenticate("nobody-at-all", pw);
        assert!(matches!(out, Outcome::Denied(_) | Outcome::Error(_)), "{out:?}");
    }

    #[test]
    fn the_service_list_ends_in_something_every_system_has() {
        assert_eq!(SERVICES[0], "veil-lock");
        assert!(SERVICES.contains(&"system-auth"));
    }
}
