//! Layer-shell management using Smithay's wlr-layer-shell implementation.
//!
//! Handles layer surface lifecycle, exclusive zones (affecting tiling work areas),
//! keyboard interactivity modes, draw-ordering, and security/trust validation.
//!
//! The pure decisions live here (trust, sanitising, the grab watchdog) so they
//! are testable with a fake clock; `server.rs` only calls them.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use smithay::desktop::layer_map_for_output;
use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_shell_v1::ZwlrLayerShellV1;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::{DisplayHandle, GlobalDispatch};
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, WlrLayerShellGlobalData, WlrLayerShellHandler, WlrLayerShellState,
};

/// Worst case between a client hanging and losing an exclusive grab:
/// up to `GRAB_IDLE_PROBE` of quiet before the compositor probes it, plus
/// `GRAB_PROBE_TIMEOUT` for the answer.
pub const EXCLUSIVE_KEYBOARD_WATCHDOG: Duration = Duration::from_secs(2);
/// A grab holder that has been silent this long gets a probe configure.
pub const GRAB_IDLE_PROBE: Duration = Duration::from_secs(1);
/// An unanswered probe older than this means the client is stalled.
pub const GRAB_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Trust policy for layer shell surfaces (section 15).
///
/// A client is trusted when the supervisor spawned it (checked by PID, never
/// by the namespace string, which any client can claim) or when its namespace
/// is listed in `shell.trusted`.
#[derive(Debug, Clone, Default)]
pub struct LayerPolicy {
    pub trusted_namespaces: HashSet<String>,
}

impl LayerPolicy {
    pub fn is_trusted(&self, namespace: &str, supervised: bool) -> bool {
        supervised || self.trusted_namespaces.contains(namespace)
    }

    /// Section 15 trust rule:
    /// Any client of the same user may use top and bottom layers (third-party bar support).
    /// Exclusive keyboard and the overlay layer are honoured only for trusted clients,
    /// and exclusive keyboard only on top and overlay (section 6.1).
    pub fn sanitize(
        &self,
        trusted: bool,
        namespace: &str,
        requested_layer: Layer,
        requested_kbd: KeyboardInteractivity,
    ) -> (Layer, KeyboardInteractivity) {
        let layer = match requested_layer {
            Layer::Overlay if !trusted => {
                tracing::warn!("Untrusted namespace '{namespace}' requested Overlay layer; downgrading to Top");
                Layer::Top
            }
            other => other,
        };

        let kbd = match requested_kbd {
            KeyboardInteractivity::Exclusive if !trusted => {
                tracing::warn!("Untrusted namespace '{namespace}' requested Exclusive keyboard; downgrading to OnDemand");
                KeyboardInteractivity::OnDemand
            }
            KeyboardInteractivity::Exclusive if matches!(layer, Layer::Background | Layer::Bottom) => {
                KeyboardInteractivity::OnDemand
            }
            other => other,
        };

        (layer, kbd)
    }
}

/// What the compositor should do about the current exclusive grab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabAction {
    /// Healthy, or still waiting for an answer.
    Idle,
    /// Send a probe configure, then call [`KeyboardGrabWatchdog::probe_sent`].
    Probe,
    /// The holder stopped answering: take the keyboard back.
    Revoke,
}

/// Watchdog for one exclusive keyboard grab, so a hung client can never
/// leave the user with no input. Liveness is a real round trip: a probe
/// `configure` the client has to `ack_configure`. Any commit or ack from the
/// holder counts as proof of life. The clock is an argument, so it is
/// testable without waiting.
#[derive(Debug)]
pub struct KeyboardGrabWatchdog<K> {
    pub key: K,
    pub grabbed_at: Instant,
    last_activity: Instant,
    /// (configure serial, when it was sent) of the probe still unanswered.
    probe: Option<(u32, Instant)>,
}

impl<K: PartialEq> KeyboardGrabWatchdog<K> {
    pub fn new(key: K, now: Instant) -> Self {
        Self {
            key,
            grabbed_at: now,
            last_activity: now,
            probe: None,
        }
    }

    /// The holder committed or acked something.
    pub fn record_activity(&mut self, now: Instant) {
        self.last_activity = now;
        self.probe = None;
    }

    pub fn probe_sent(&mut self, serial: u32, now: Instant) {
        self.probe = Some((serial, now));
    }

    /// The holder acked a configure. Returns true if it was our probe.
    pub fn on_ack(&mut self, serial: u32, now: Instant) -> bool {
        // Any ack is life; only the probe's own serial is "the" answer, but a
        // later serial acks everything before it, so treat all acks alike.
        let was_probe = self.probe.is_some_and(|(s, _)| s == serial);
        self.record_activity(now);
        was_probe
    }

    pub fn tick(&self, now: Instant) -> GrabAction {
        match self.probe {
            Some((_, sent)) if now.saturating_duration_since(sent) >= GRAB_PROBE_TIMEOUT => {
                GrabAction::Revoke
            }
            Some(_) => GrabAction::Idle,
            None if now.saturating_duration_since(self.last_activity) >= GRAB_IDLE_PROBE => {
                GrabAction::Probe
            }
            None => GrabAction::Idle,
        }
    }
}

/// Layer-shell manager for the compositor.
pub struct LayerShellManager {
    pub state: WlrLayerShellState,
    pub policy: LayerPolicy,
    /// The exclusive keyboard grab currently being watched, if any.
    pub active_watchdog: Option<KeyboardGrabWatchdog<ObjectId>>,
    /// Surfaces whose exclusive grab was revoked by the watchdog. They stay
    /// mapped but are treated as on-demand until they go away.
    pub revoked: HashSet<ObjectId>,
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
            revoked: HashSet::new(),
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

/// Wire name of a layer, for `layers.list`.
pub fn layer_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Background => "background",
        Layer::Bottom => "bottom",
        Layer::Top => "top",
        Layer::Overlay => "overlay",
    }
}

/// Wire name of a keyboard mode, for `layers.list`.
pub fn kbd_name(kbd: KeyboardInteractivity) -> &'static str {
    match kbd {
        KeyboardInteractivity::None => "none",
        KeyboardInteractivity::Exclusive => "exclusive",
        KeyboardInteractivity::OnDemand => "on_demand",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with(ns: &[&str]) -> LayerPolicy {
        LayerPolicy { trusted_namespaces: ns.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn supervised_client_keeps_overlay_and_exclusive() {
        let p = LayerPolicy::default();
        let trusted = p.is_trusted("veil-launch", true);
        let (layer, kbd) = p.sanitize(trusted, "veil-launch", Layer::Overlay, KeyboardInteractivity::Exclusive);
        assert_eq!((layer, kbd), (Layer::Overlay, KeyboardInteractivity::Exclusive));
    }

    #[test]
    fn namespace_alone_does_not_grant_trust() {
        // A hostile client can name itself anything. Only the supervisor's
        // PID check or an explicit `shell.trusted` entry counts.
        let p = LayerPolicy::default();
        let trusted = p.is_trusted("veil-launch", false);
        assert!(!trusted);
        let (layer, kbd) = p.sanitize(trusted, "veil-launch", Layer::Overlay, KeyboardInteractivity::Exclusive);
        assert_eq!((layer, kbd), (Layer::Top, KeyboardInteractivity::OnDemand));
    }

    #[test]
    fn listed_namespace_is_trusted_without_supervision() {
        let p = policy_with(&["my-lock"]);
        assert!(p.is_trusted("my-lock", false));
        assert!(!p.is_trusted("other", false));
    }

    #[test]
    fn untrusted_top_and_bottom_pass_for_third_party_bars() {
        let p = LayerPolicy::default();
        let (top, _) = p.sanitize(false, "waybar", Layer::Top, KeyboardInteractivity::None);
        let (bottom, _) = p.sanitize(false, "dock", Layer::Bottom, KeyboardInteractivity::None);
        assert_eq!((top, bottom), (Layer::Top, Layer::Bottom));
    }

    #[test]
    fn exclusive_only_on_top_and_overlay_even_when_trusted() {
        let p = LayerPolicy::default();
        for layer in [Layer::Background, Layer::Bottom] {
            let (l, k) = p.sanitize(true, "veil-bar", layer, KeyboardInteractivity::Exclusive);
            assert_eq!((l, k), (layer, KeyboardInteractivity::OnDemand));
        }
        for layer in [Layer::Top, Layer::Overlay] {
            let (_, k) = p.sanitize(true, "veil-bar", layer, KeyboardInteractivity::Exclusive);
            assert_eq!(k, KeyboardInteractivity::Exclusive);
        }
    }

    #[test]
    fn fullscreen_layer_visibility_rules() {
        for layer in [Layer::Background, Layer::Bottom, Layer::Top, Layer::Overlay] {
            assert!(LayerShellManager::is_layer_visible_with_fullscreen(layer, false));
        }
        assert!(!LayerShellManager::is_layer_visible_with_fullscreen(Layer::Background, true));
        assert!(!LayerShellManager::is_layer_visible_with_fullscreen(Layer::Bottom, true));
        assert!(!LayerShellManager::is_layer_visible_with_fullscreen(Layer::Top, true));
        assert!(LayerShellManager::is_layer_visible_with_fullscreen(Layer::Overlay, true));
    }

    #[test]
    fn healthy_holder_is_probed_when_idle_and_stays_grabbed_when_it_answers() {
        let t0 = Instant::now();
        let mut w = KeyboardGrabWatchdog::new(1u32, t0);
        assert_eq!(w.tick(t0 + Duration::from_millis(900)), GrabAction::Idle);
        // Quiet for a second: probe it.
        let t1 = t0 + Duration::from_millis(1000);
        assert_eq!(w.tick(t1), GrabAction::Probe);
        w.probe_sent(42, t1);
        // While the probe is out and young, nothing happens.
        assert_eq!(w.tick(t1 + Duration::from_millis(500)), GrabAction::Idle);
        // It answers: back to healthy, next probe a full idle period later.
        let t2 = t1 + Duration::from_millis(600);
        assert!(w.on_ack(42, t2));
        assert_eq!(w.tick(t2 + Duration::from_millis(900)), GrabAction::Idle);
        assert_eq!(w.tick(t2 + Duration::from_millis(1000)), GrabAction::Probe);
    }

    #[test]
    fn stalled_holder_is_revoked_within_the_two_second_budget() {
        let t0 = Instant::now();
        let mut w = KeyboardGrabWatchdog::new(1u32, t0);
        // Worst case: it hangs right after proving life at t0.
        let probe_at = t0 + GRAB_IDLE_PROBE;
        assert_eq!(w.tick(probe_at), GrabAction::Probe);
        w.probe_sent(7, probe_at);
        assert_eq!(w.tick(probe_at + GRAB_PROBE_TIMEOUT - Duration::from_millis(1)), GrabAction::Idle);
        let revoke_at = probe_at + GRAB_PROBE_TIMEOUT;
        assert_eq!(w.tick(revoke_at), GrabAction::Revoke);
        assert!(revoke_at.duration_since(t0) <= EXCLUSIVE_KEYBOARD_WATCHDOG);
    }

    #[test]
    fn a_commit_counts_as_life_and_cancels_the_probe() {
        let t0 = Instant::now();
        let mut w = KeyboardGrabWatchdog::new(1u32, t0);
        w.probe_sent(3, t0 + Duration::from_secs(1));
        w.record_activity(t0 + Duration::from_millis(1500));
        // The outstanding probe is gone (no Revoke at 2 s+), and the idle
        // clock restarted at 1.5 s.
        assert_eq!(w.tick(t0 + Duration::from_millis(2000)), GrabAction::Idle);
        assert_eq!(w.tick(t0 + Duration::from_millis(2500)), GrabAction::Probe);
    }
}
