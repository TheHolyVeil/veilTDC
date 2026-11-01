//! `wlr-output-management-unstable-v1`, phase 1: read-only.
//!
//! Advertises every DRM head with its full mode list, so `wlr-randr`,
//! `kanshi`, `wdisplays` and friends can see the outputs. The global only
//! exists when the backend reports at least one head (DRM); terminal mode
//! never advertises it, since it couldn't honor a configuration anyway.
//!
//! `apply`/`test` answer `failed` until the apply path exists, and a stale
//! serial gets `cancelled` as the protocol requires. Honest, not stubbed:
//! nothing here claims a capability it doesn't have.
//!
//! Data flow: `DrmOutput::heads()` (main thread) -> `InputCmd::SetHeads`
//! -> `State::output_mgmt.set_heads()` (compositor thread).

use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use wayland_server::{
    backend::GlobalId, protocol::wl_output::Transform, Client, DataInit, Dispatch, DisplayHandle,
    GlobalDispatch, New, Resource,
};

use crate::server::State;

/// Highest protocol version we implement (4 adds adaptive_sync, which we
/// report as disabled).
const VERSION: u32 = 4;

#[derive(Clone, Debug, PartialEq)]
pub struct ModeInfo {
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
    pub preferred: bool,
}

/// Backend-side snapshot of one output.
#[derive(Clone, Debug, PartialEq)]
pub struct HeadInfo {
    pub name: String,
    pub description: String,
    /// Empty = unknown (not sent to clients).
    pub make: String,
    pub model: String,
    pub phys_mm: (i32, i32),
    pub modes: Vec<ModeInfo>,
    /// Index into `modes` of the mode currently scanned out.
    pub current: usize,
    /// Filled in by the compositor from `State.monitors[i].rect`.
    pub position: (i32, i32),
}

struct ClientHead {
    head: ZwlrOutputHeadV1,
    modes: Vec<ZwlrOutputModeV1>,
}

struct ClientManager {
    manager: ZwlrOutputManagerV1,
    heads: Vec<ClientHead>,
}

#[derive(Default)]
pub struct OutputManagement {
    global: Option<GlobalId>,
    heads: Vec<HeadInfo>,
    serial: u32,
    clients: Vec<ClientManager>,
}

/// Per-configuration user data: the serial the client built it against.
pub struct ConfigData {
    serial: u32,
}

fn send_head(
    dh: &DisplayHandle,
    client: &Client,
    manager: &ZwlrOutputManagerV1,
    info: &HeadInfo,
) -> Option<ClientHead> {
    let version = manager.version();
    let head = client
        .create_resource::<ZwlrOutputHeadV1, (), State>(dh, version, ())
        .ok()?;
    manager.head(&head);
    head.name(info.name.clone());
    head.description(info.description.clone());
    if info.phys_mm.0 > 0 && info.phys_mm.1 > 0 {
        head.physical_size(info.phys_mm.0, info.phys_mm.1);
    }

    let mut modes = Vec::with_capacity(info.modes.len());
    for m in &info.modes {
        // All-or-nothing: `info.current` indexes this vec, so a skipped mode
        // would silently shift it.
        let mode = client
            .create_resource::<ZwlrOutputModeV1, (), State>(dh, version, ())
            .ok()?;
        head.mode(&mode);
        mode.size(m.width, m.height);
        mode.refresh(m.refresh_mhz);
        if m.preferred {
            mode.preferred();
        }
        modes.push(mode);
    }

    head.enabled(1);
    if let Some(cur) = modes.get(info.current) {
        head.current_mode(cur);
    }
    head.position(info.position.0, info.position.1);
    head.transform(Transform::Normal);
    head.scale(1.0);
    if version >= 2 {
        if !info.make.is_empty() {
            head.make(info.make.clone());
        }
        if !info.model.is_empty() {
            head.model(info.model.clone());
        }
    }
    if version >= 4 {
        head.adaptive_sync(zwlr_output_head_v1::AdaptiveSyncState::Disabled);
    }
    Some(ClientHead { head, modes })
}

fn retire_head(h: ClientHead) {
    for m in &h.modes {
        m.finished();
    }
    h.head.finished();
}

impl OutputManagement {
    /// Replace the advertised head list. Creates the global on first
    /// non-empty call, removes it if the list goes empty, and re-announces
    /// to every bound client with a fresh serial.
    pub fn set_heads(&mut self, dh: &DisplayHandle, heads: Vec<HeadInfo>) {
        if self.global.is_some() && heads == self.heads {
            return;
        }
        self.heads = heads;
        self.clients.retain(|c| c.manager.is_alive());

        if self.heads.is_empty() {
            if let Some(g) = self.global.take() {
                dh.remove_global::<State>(g);
            }
            for c in &mut self.clients {
                for h in c.heads.drain(..) {
                    retire_head(h);
                }
            }
            return;
        }

        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        if self.global.is_none() {
            self.global = Some(dh.create_global::<State, ZwlrOutputManagerV1, ()>(VERSION, ()));
        }
        for c in &mut self.clients {
            for h in c.heads.drain(..) {
                retire_head(h);
            }
            let Some(client) = c.manager.client() else {
                continue;
            };
            c.heads = self
                .heads
                .iter()
                .filter_map(|i| send_head(dh, &client, &c.manager, i))
                .collect();
            c.manager.done(serial);
        }
    }
}

impl GlobalDispatch<ZwlrOutputManagerV1, (), State> for State {
    fn bind(
        state: &mut State,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        let manager = data_init.init(resource, ());
        let om = &mut state.output_mgmt;
        let heads = om
            .heads
            .iter()
            .filter_map(|i| send_head(dh, client, &manager, i))
            .collect();
        manager.done(om.serial);
        om.clients.push(ClientManager { manager, heads });
    }
}

impl Dispatch<ZwlrOutputManagerV1, (), State> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(id, ConfigData { serial });
            }
            zwlr_output_manager_v1::Request::Stop => {
                manager.finished();
                state.output_mgmt.clients.retain(|c| &c.manager != manager);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ConfigData, State> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        config: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &ConfigData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head: _ } => {
                data_init.init(id, ());
            }
            zwlr_output_configuration_v1::Request::Apply
            | zwlr_output_configuration_v1::Request::Test => {
                if data.serial != state.output_mgmt.serial {
                    config.cancelled();
                } else {
                    // Phase 1: no apply path yet.
                    config.failed();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, (), State> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _head: &ZwlrOutputConfigurationHeadV1,
        _request: zwlr_output_configuration_head_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        // Phase 1: settings are accepted and ignored; apply always fails.
    }
}

impl Dispatch<ZwlrOutputHeadV1, (), State> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _head: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
    }
}

impl Dispatch<ZwlrOutputModeV1, (), State> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _mode: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
    }
}
