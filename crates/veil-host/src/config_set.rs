//! `config.set` wire format: JSON in, edits out. The parser lives in
//! `veil-config` so the settings app, which saves directly when there is no
//! compositor, accepts exactly what the compositor does.

pub use veil_config::wire::edits_from_args;
