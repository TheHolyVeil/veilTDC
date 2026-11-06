//! veil-notify: the Veil notification service.
//!
//! Phase 4 builds this crate in steps; each one compiles and passes on its own.
//! Steps 1 and 2 are the pure data, timing and geometry parts; steps 3 and 4 are
//! the two connections (D-Bus and the Veil IPC):
//!
//! - [`text`]: plain-text cleaning and safe truncation
//! - [`markup`]: the `<b> <i> <u>` body subset, one parser for input and storage
//! - [`model`]: [`model::Notification`], hints, actions, image validation
//! - [`store`]: bounded history, unread count, DND, atomic 0600 persistence
//! - [`rate`]: per-app sliding-window limiter with an injected clock
//! - [`app`]: the process: surfaces, the poll loop, effects out
//! - [`engine`]: the notification logic (DND, rate limit, dismiss, expiry, bell) with no I/O
//! - [`timers`]: toast expiry, visibility cap, hover pause, next wake-up time
//! - [`panel`]: the history panel: layout, drawing, hit-testing
//! - [`toast`]: one toast: measure, draw, hit-test
//! - [`waker`]: the eventfd that lets background threads wake the main loop
//! - [`wrap`]: word wrapping of styled text for toasts and the panel
//! - [`layout`]: toast stack rectangles, input regions, click hit-testing
//! - [`ids`]: the shared, persisted id counter
//! - [`dbus`]: the `org.freedesktop.Notifications` service on a worker thread
//! - [`ipc`]: the Veil IPC client: bell state out, control events in
//!
//! Later steps add toasts (5), the history panel (6) and the
//! `veil shell notify` entry (7).

pub mod app;
pub mod dbus;
pub mod engine;
pub mod ids;
pub mod ipc;
pub mod layout;
pub mod markup;
pub mod model;
pub mod panel;
pub mod rate;
pub mod store;
pub mod text;
pub mod timers;
pub mod toast;
pub mod waker;
pub mod wrap;
