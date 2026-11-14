//! `ext-background-effect-v1` implementation for Veil.
//!
//! Provides `ext_background_effect_manager_v1` global to allow surfaces to request
//! background effects such as background blur.

use std::collections::HashMap;

use smithay::reexports::wayland_protocols::ext::background_effect::v1::server::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::{self, ExtBackgroundEffectSurfaceV1},
};
use smithay::reexports::wayland_server::{
    backend::ObjectId,
    protocol::wl_surface::WlSurface,
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::{get_region_attributes, RectangleKind, RegionAttributes};

use crate::layout::Rect;
use crate::server::State;

/// A region as plain numbers, so two of them can be compared.
fn flat(region: &Option<RegionAttributes>) -> Vec<(bool, i32, i32, i32, i32)> {
    region.as_ref().map_or_else(Vec::new, |a| {
        a.rects
            .iter()
            .map(|(k, r)| (matches!(k, RectangleKind::Add), r.loc.x, r.loc.y, r.size.w, r.size.h))
            .collect()
    })
}

#[derive(Debug, Clone, Default)]
pub struct BlurRegionState {
    pub pending: Option<RegionAttributes>,
    pub current: Option<RegionAttributes>,
}

pub struct BackgroundEffectState {
    effects: HashMap<ObjectId, BlurRegionState>,
    surface_objects: HashMap<ObjectId, ExtBackgroundEffectSurfaceV1>,
    object_to_surface: HashMap<ObjectId, (ObjectId, WlSurface)>,
}

impl Default for BackgroundEffectState {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundEffectState {
    pub const VERSION: u32 = 1;

    pub fn new() -> Self {
        Self {
            effects: HashMap::new(),
            surface_objects: HashMap::new(),
            object_to_surface: HashMap::new(),
        }
    }

    pub fn register_global(dh: &DisplayHandle) {
        dh.create_global::<State, ExtBackgroundEffectManagerV1, ()>(Self::VERSION, ());
    }

    /// Called on `wl_surface.commit` to apply double-buffered blur state.
    /// Returns true when the committed region actually changed, so the
    /// caller can repaint (a steady blurred bar commits every second and must
    /// not look like a change).
    pub fn commit(&mut self, surface: &WlSurface) -> bool {
        let Some(state) = self.effects.get_mut(&surface.id()) else {
            return false;
        };
        let changed = flat(&state.current) != flat(&state.pending);
        state.current = state.pending.clone();
        changed
    }

    /// The blur rectangles of `surface` in output space: its committed region
    /// moved by `origin` (where the surface's (0,0) lands), with subtractions
    /// applied, clipped to `bounds` and the `w` x `h` output.
    pub fn rects_in(&self, surface: &WlSurface, origin: (i32, i32), bounds: Rect, w: u32, h: u32) -> Vec<Rect> {
        let Some(attrs) = self.blur_region(surface) else {
            return Vec::new();
        };
        let ops: Vec<(bool, Rect)> = attrs
            .rects
            .iter()
            .map(|(kind, r)| {
                let rect = Rect {
                    x: origin.0 + r.loc.x,
                    y: origin.1 + r.loc.y,
                    w: r.size.w.max(0) as u32,
                    h: r.size.h.max(0) as u32,
                };
                (matches!(kind, RectangleKind::Add), rect)
            })
            .collect();
        crate::blur::region_rects(&ops)
            .into_iter()
            .map(|r| r.intersect(&bounds).clamp_to(w, h))
            .filter(|r| !r.is_empty())
            .collect()
    }

    /// Check if a surface has an active blur region.
    pub fn blur_region(&self, surface: &WlSurface) -> Option<&RegionAttributes> {
        self.effects
            .get(&surface.id())
            .and_then(|st| st.current.as_ref())
    }

    /// Check if a surface has registered for background effect.
    pub fn has_effect(&self, surface: &WlSurface) -> bool {
        self.effects.contains_key(&surface.id())
    }

    /// Remove state associated with a destroyed surface object.
    pub fn remove_effect_object(&mut self, object_id: &ObjectId) {
        if let Some((surf_id, _)) = self.object_to_surface.remove(object_id) {
            self.effects.remove(&surf_id);
            self.surface_objects.remove(&surf_id);
        }
    }

    /// Clean up state if surface was destroyed.
    pub fn cleanup_surface(&mut self, surface_id: &ObjectId) {
        if let Some(obj) = self.surface_objects.remove(surface_id) {
            self.object_to_surface.remove(&obj.id());
        }
        self.effects.remove(surface_id);
    }
}

impl GlobalDispatch<ExtBackgroundEffectManagerV1, (), State> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtBackgroundEffectManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        let manager = data_init.init(resource, ());
        manager.capabilities(ext_background_effect_manager_v1::Capability::Blur);
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, (), State> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        res: &ExtBackgroundEffectManagerV1,
        request: ext_background_effect_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            ext_background_effect_manager_v1::Request::GetBackgroundEffect { id, surface } => {
                let surf_id = surface.id();
                if state.background_effect.effects.contains_key(&surf_id) {
                    res.post_error(
                        ext_background_effect_manager_v1::Error::BackgroundEffectExists,
                        "surface already has a background effect object",
                    );
                    return;
                }

                let effect_res: ExtBackgroundEffectSurfaceV1 = data_init.init(id, ());
                let obj_id = effect_res.id();

                state
                    .background_effect
                    .effects
                    .insert(surf_id.clone(), BlurRegionState::default());
                state
                    .background_effect
                    .surface_objects
                    .insert(surf_id.clone(), effect_res);
                state
                    .background_effect
                    .object_to_surface
                    .insert(obj_id, (surf_id, surface));
            }
            ext_background_effect_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, (), State> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        res: &ExtBackgroundEffectSurfaceV1,
        request: ext_background_effect_surface_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            ext_background_effect_surface_v1::Request::SetBlurRegion { region } => {
                let obj_id = res.id();
                let Some((surf_id, surface)) =
                    state.background_effect.object_to_surface.get(&obj_id)
                else {
                    return;
                };

                if !surface.is_alive() {
                    res.post_error(
                        ext_background_effect_surface_v1::Error::SurfaceDestroyed,
                        "the associated surface has been destroyed",
                    );
                    return;
                }

                let attrs = region.as_ref().map(get_region_attributes);
                if let Some(entry) = state.background_effect.effects.get_mut(surf_id) {
                    entry.pending = attrs;
                }
            }
            ext_background_effect_surface_v1::Request::Destroy => {
                state.background_effect.remove_effect_object(&res.id());
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut State,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        res: &ExtBackgroundEffectSurfaceV1,
        _: &(),
    ) {
        state.background_effect.remove_effect_object(&res.id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_background_effect_initial_state() {
        let state = BackgroundEffectState::new();
        assert_eq!(BackgroundEffectState::VERSION, 1);
        assert!(state.effects.is_empty());
        assert!(state.surface_objects.is_empty());
        assert!(state.object_to_surface.is_empty());
    }

    #[test]
    fn test_blur_region_state_commit() {
        let mut blur_state = BlurRegionState::default();
        assert!(blur_state.pending.is_none());
        assert!(blur_state.current.is_none());

        blur_state.pending = Some(RegionAttributes {
            rects: vec![(
                smithay::wayland::compositor::RectangleKind::Add,
                smithay::utils::Rectangle::new((0, 0).into(), (100, 100).into()),
            )],
        });

        // Current should still be None until committed
        assert!(blur_state.current.is_none());

        blur_state.current = blur_state.pending.clone();
        assert!(blur_state.current.is_some());
        assert_eq!(blur_state.current.as_ref().unwrap().rects.len(), 1);
    }
}
