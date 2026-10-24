//! Layer-shell management using Smithay's wlr-layer-shell implementation.
//!
//! Handles layer surface lifecycle, exclusive zones (affecting tiling work areas),
//! keyboard interactivity modes, draw-ordering, and security/trust validation.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use smithay::desktop::layer_map_for_output;
use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_shell_v1::ZwlrLayerShellV1;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_surface_v1::KeyboardInteractivity;
use smithay::reexports::wayland_server::{DisplayHandle, GlobalDispatch};
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::shell::wlr_layer::{
    Layer, WlrLayerShellGlobalData, WlrLayerShellHandler, WlrLayerShellState,
};

pub const EXCLUSIVE_KEYBOARD_WATCHDOG: Duration = Duration::from_secs(2);

/// Trust policy and tracking for layer shell surfaces.
#[derive(Debug, Clone)]
pub struct LayerPolicy {
    pub trusted_namespaces: HashSet<String>,
}

impl Default for LayerPolicy {
    fn default() -> Self {
        let mut trusted = HashSet::new();
        trusted.insert("veil-bar".into());
        trusted.insert("veil-notify".into());
        trusted.insert("veil-osd".into());
        trusted.insert("veil-launch".into());
        trusted.insert("veil-power".into());
        trusted.insert("veil-settings".into());
        trusted.insert("veil-lock".into());
        trusted.insert("veil-screenshot".into());
        Self {
            trusted_namespaces: trusted,
        }
    }
}

impl LayerPolicy {
    /// Section 15 trust rule:
    /// Any client of the same user may use top and bottom layers (third-party bar support).
    /// Exclusive keyboard and overlay layer are honored only for supervised/trusted components.
    pub fn sanitize_request(
        &self,
        namespace: &str,
        requested_layer: Layer,
        requested_kbd: KeyboardInteractivity,
    ) -> (Layer, KeyboardInteractivity) {
        let is_trusted = self.trusted_namespaces.contains(namespace);

        let sanitized_layer = match requested_layer {
            Layer::Overlay if !is_trusted => {
                tracing::warn!("Untrusted namespace '{namespace}' requested Overlay layer; downgrading to Top");
                Layer::Top
            }
            other => other,
        };

        let sanitized_kbd = match requested_kbd {
            KeyboardInteractivity::Exclusive if !is_trusted => {
                tracing::warn!("Untrusted namespace '{namespace}' requested Exclusive keyboard; downgrading to OnDemand");
                KeyboardInteractivity::OnDemand
            }
            other => other,
        };

        (sanitized_layer, sanitized_kbd)
    }
}

/// Watchdog for tracking exclusive keyboard grabs to avoid lockouts.
#[derive(Debug)]
pub struct KeyboardGrabWatchdog {
    pub surface_id: usize,
    pub grabbed_at: Instant,
    pub last_activity: Instant,
}

impl KeyboardGrabWatchdog {
    pub fn new(surface_id: usize, now: Instant) -> Self {
        Self {
            surface_id,
            grabbed_at: now,
            last_activity: now,
        }
    }

    pub fn record_activity(&mut self, now: Instant) {
        self.last_activity = now;
    }

    pub fn is_stalled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_activity) > EXCLUSIVE_KEYBOARD_WATCHDOG
    }
}

/// Layer-shell manager for the compositor.
pub struct LayerShellManager {
    pub state: WlrLayerShellState,
    pub policy: LayerPolicy,
    pub active_watchdog: Option<KeyboardGrabWatchdog>,
}

impl LayerShellManager {
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrLayerShellV1, WlrLayerShellGlobalData>
            + WlrLayerShellHandler
            + 'static,
    {
        Self {
            state: WlrLayerShellState::new::<D>(dh),
            policy: LayerPolicy::default(),
            active_watchdog: None,
        }
    }

    /// Compute usable non-exclusive work area for an output.
    pub fn work_area_for_output(output: &Output, fallback_w: u32, fallback_h: u32) -> Rectangle<i32, Logical> {
        let map = layer_map_for_output(output);
        let zone = map.non_exclusive_zone();
        if zone.size.w > 0 && zone.size.h > 0 {
            zone
        } else {
            Rectangle::new(Point::from((0, 0)), Size::from((fallback_w as i32, fallback_h as i32)))
        }
    }

    /// Whether fullscreen suppresses surfaces on a given layer.
    /// Fullscreen hides Background, Bottom, and Top. Overlay remains visible.
    pub fn is_layer_visible_with_fullscreen(layer: Layer, has_fullscreen: bool) -> bool {
        if !has_fullscreen {
            return true;
        }
        matches!(layer, Layer::Overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_policy_permits_trusted_overlay_and_exclusive() {
        let policy = LayerPolicy::default();
        let (layer, kbd) = policy.sanitize_request(
            "veil-launch",
            Layer::Overlay,
            KeyboardInteractivity::Exclusive,
        );
        assert_eq!(layer, Layer::Overlay);
        assert_eq!(kbd, KeyboardInteractivity::Exclusive);
    }

    #[test]
    fn trust_policy_downgrades_untrusted_overlay_and_exclusive() {
        let policy = LayerPolicy::default();
        let (layer, kbd) = policy.sanitize_request(
            "hostile-app",
            Layer::Overlay,
            KeyboardInteractivity::Exclusive,
        );
        assert_eq!(layer, Layer::Top);
        assert_eq!(kbd, KeyboardInteractivity::OnDemand);
    }

    #[test]
    fn trust_policy_allows_untrusted_top_and_bottom_for_bars() {
        let policy = LayerPolicy::default();
        let (top, _) = policy.sanitize_request(
            "waybar",
            Layer::Top,
            KeyboardInteractivity::None,
        );
        let (bottom, _) = policy.sanitize_request(
            "some-dock",
            Layer::Bottom,
            KeyboardInteractivity::None,
        );
        assert_eq!(top, Layer::Top);
        assert_eq!(bottom, Layer::Bottom);
    }

    #[test]
    fn fullscreen_layer_visibility_rules() {
        // Without fullscreen, everything visible
        assert!(LayerShellManager::is_layer_visible_with_fullscreen(Layer::Background, false));
        assert!(LayerShellManager::is_layer_visible_with_fullscreen(Layer::Bottom, false));
        assert!(LayerShellManager::is_layer_visible_with_fullscreen(Layer::Top, false));
        assert!(LayerShellManager::is_layer_visible_with_fullscreen(Layer::Overlay, false));

        // With fullscreen, only Overlay remains visible
        assert!(!LayerShellManager::is_layer_visible_with_fullscreen(Layer::Background, true));
        assert!(!LayerShellManager::is_layer_visible_with_fullscreen(Layer::Bottom, true));
        assert!(!LayerShellManager::is_layer_visible_with_fullscreen(Layer::Top, true));
        assert!(LayerShellManager::is_layer_visible_with_fullscreen(Layer::Overlay, true));
    }

    #[test]
    fn watchdog_detects_stalled_client() {
        let t0 = Instant::now();
        let mut watchdog = KeyboardGrabWatchdog::new(1, t0);
        assert!(!watchdog.is_stalled(t0 + Duration::from_millis(500)));

        // Advance beyond 2 seconds without activity
        assert!(watchdog.is_stalled(t0 + Duration::from_millis(2500)));

        // Heartbeat resets
        watchdog.record_activity(t0 + Duration::from_millis(2400));
        assert!(!watchdog.is_stalled(t0 + Duration::from_millis(2500)));
    }
}
