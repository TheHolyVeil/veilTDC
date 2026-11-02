//! veil-notify: the Veil notification service.
//!
//! Phase 4 builds this crate in steps; each one compiles and passes on its own.
//! Steps 1 and 2 are the pure data, timing and geometry parts; step 3 is the
//! D-Bus service:
//!
//! - [`text`]: plain-text cleaning and safe truncation
//! - [`markup`]: the `<b> <i> <u>` body subset, one parser for input and storage
//! - [`model`]: [`model::Notification`], hints, actions, image validation
//! - [`store`]: bounded history, unread count, DND, atomic 0600 persistence
//! - [`rate`]: per-app sliding-window limiter with an injected clock
//! - [`timers`]: toast expiry, visibility cap, hover pause, next wake-up time
//! - [`layout`]: toast stack rectangles, input regions, click hit-testing
//! - [`ids`]: the shared, persisted id counter
//! - [`dbus`]: the `org.freedesktop.Notifications` service on a worker thread
//!
//! Later steps add IPC wiring (4), toasts (5), the history panel (6) and the
//! `veil shell notify` entry (7).

pub mod dbus;
pub mod ids;
pub mod layout;
pub mod markup;
pub mod model;
pub mod rate;
pub mod store;
pub mod text;
pub mod timers;
