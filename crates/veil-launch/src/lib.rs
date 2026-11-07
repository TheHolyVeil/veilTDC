//! Supervised app launcher for the Veil desktop shell (`veil shell launch`).
//!
//! Built in layers, each testable without a display:
//! - `desktop`: what can be launched (XDG `.desktop` discovery and `Exec=`)
//! - `search`: ranking those apps for what was typed
//! - `calc`: the calculator row

pub mod calc;
pub mod desktop;
pub mod search;
