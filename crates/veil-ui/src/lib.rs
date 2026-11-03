//! veil-ui: Drawing kit, theme tokens, widget trait, and layer-shell client runtime.

pub mod draw;
pub mod layer;
pub mod theme;
pub mod widget;

pub use draw::*;
pub use layer::{LayerOptions, LayerShellClient, OutputFilter, SurfacePosition, UiInputEvent};
// Re-exported so a client picks its anchor, layer and keyboard mode without
// depending on smithay-client-toolkit itself.
pub use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
pub use theme::{Theme, ThemeTokens};
pub use widget::{Action, Metrics, Widget};

// Re-export veil-text for convenience
pub use veil_text::{FontConfig, Role, TextSystem};
