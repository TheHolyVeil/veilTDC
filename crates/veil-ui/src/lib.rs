//! veil-ui: Drawing kit, theme tokens, widget trait, and layer-shell client runtime.

pub mod draw;
pub mod layer;
pub mod theme;
pub mod widget;

pub use draw::*;
pub use layer::{LayerShellClient, SurfacePosition, UiInputEvent};
pub use theme::{Theme, ThemeTokens};
pub use widget::Widget;

// Re-export veil-text for convenience
pub use veil_text::{FontConfig, Role, TextSystem};
