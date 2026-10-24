//! Wayland layer-shell surface client runtime.
//!
//! Wraps `smithay-client-toolkit` to create, configure, and manage layer-shell
//! surfaces across all connected outputs with SHM slot-pool presentation.

use std::sync::mpsc::{channel, Receiver, Sender};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemedPointer, ThemeSpec},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler,
            LayerSurface as SctLayerSurface, LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface},
    Connection, EventQueue, Proxy, QueueHandle,
};

#[derive(Debug, Clone)]
pub enum UiInputEvent {
    Motion { x: f64, y: f64 },
    Press { x: f64, y: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfacePosition {
    Top,
    Bottom,
    CenterOverlay,
}

struct OutputSurface {
    output_id: u32,
    layer_surface: SctLayerSurface,
    pool: SlotPool,
    width: u32,
    height: u32,
    configured: bool,
}

pub struct LayerShellClient {
    queue: EventQueue<ClientState>,
    state: ClientState,
    pub input_rx: Receiver<UiInputEvent>,
}

impl LayerShellClient {
    pub fn new(
        namespace: &str,
        layer: Layer,
        position: SurfacePosition,
        height: u32,
        exclusive: bool,
    ) -> Result<Self, String> {
        let conn = Connection::connect_to_env()
            .map_err(|e| format!("Failed to connect to Wayland display: {e}"))?;
        let (globals, queue) = registry_queue_init::<ClientState>(&conn)
            .map_err(|e| format!("Failed to init Wayland registry: {e}"))?;
        let qh = queue.handle();

        let compositor = CompositorState::bind(&globals, &qh)
            .map_err(|e| format!("wl_compositor missing: {e}"))?;
        let layer_shell = LayerShell::bind(&globals, &qh)
            .map_err(|e| format!("wlr-layer-shell missing: {e}"))?;
        let shm = Shm::bind(&globals, &qh)
            .map_err(|e| format!("wl_shm missing: {e}"))?;
        let seat_state = SeatState::new(&globals, &qh);

        let (tx, rx) = channel();

        let state = ClientState {
            registry: RegistryState::new(&globals),
            compositor,
            output_state: OutputState::new(&globals, &qh),
            seat_state,
            shm,
            layer_shell,
            namespace: namespace.to_string(),
            layer,
            position,
            height,
            exclusive,
            surfaces: Vec::new(),
            pointer: None,
            input_tx: tx,
            mouse_x: 0.0,
            mouse_y: 0.0,
        };

        let mut client = Self {
            queue,
            state,
            input_rx: rx,
        };
        let _ = client.queue.roundtrip(&mut client.state);
        Ok(client)
    }

    pub fn dispatch(&mut self) -> Result<(), String> {
        let _ = self.queue.flush();
        self.queue.dispatch_pending(&mut self.state)
            .map_err(|e| format!("Dispatch error: {e}"))?;
        if self.state.surfaces.iter().any(|s| !s.configured) {
            let _ = self.queue.roundtrip(&mut self.state);
        }
        Ok(())
    }

    /// Render and commit pixels to each configured output surface.
    pub fn present_for_each<F: FnMut(u32, u32) -> Vec<u8>>(&mut self, mut render: F) {
        for surf in &mut self.state.surfaces {
            if surf.width == 0 || surf.height == 0 {
                continue;
            }
            let pixels = render(surf.width, surf.height);
            let stride = surf.width * 4;
            if let Ok((buffer, canvas)) = surf.pool.create_buffer(
                surf.width as i32,
                surf.height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
            ) {
                let len = canvas.len().min(pixels.len());
                canvas[..len].copy_from_slice(&pixels[..len]);
                let wl_surf = surf.layer_surface.wl_surface();
                let _ = buffer.attach_to(wl_surf);
                wl_surf.damage_buffer(0, 0, surf.width as i32, surf.height as i32);
                surf.layer_surface.commit();
            }
        }
    }
}

struct ClientState {
    registry: RegistryState,
    compositor: CompositorState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    layer_shell: LayerShell,
    namespace: String,
    layer: Layer,
    position: SurfacePosition,
    height: u32,
    exclusive: bool,
    surfaces: Vec<OutputSurface>,
    pointer: Option<ThemedPointer>,
    input_tx: Sender<UiInputEvent>,
    mouse_x: f64,
    mouse_y: f64,
}

impl ClientState {
    fn add_output(&mut self, qh: &QueueHandle<Self>, output: &wl_output::WlOutput) {
        let output_id = output.id().protocol_id();
        if self.surfaces.iter().any(|s| s.output_id == output_id) {
            return;
        }

        let surface = self.compositor.create_surface(qh);
        let layer_surface = self.layer_shell.create_layer_surface(
            qh,
            surface,
            self.layer,
            Some(&self.namespace),
            Some(output),
        );

        let anchor = match self.position {
            SurfacePosition::Top => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            SurfacePosition::Bottom => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            SurfacePosition::CenterOverlay => Anchor::empty(),
        };

        layer_surface.set_anchor(anchor);
        if self.exclusive {
            layer_surface.set_exclusive_zone(self.height as i32);
        } else {
            layer_surface.set_exclusive_zone(0);
        }
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer_surface.set_size(0, self.height);
        layer_surface.commit();

        let pool = match SlotPool::new(8 * 1024 * 1024, &self.shm) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("SHM slot pool error for output {output_id}: {e}");
                return;
            }
        };

        self.surfaces.push(OutputSurface {
            output_id,
            layer_surface,
            pool,
            width: 0,
            height: 0,
            configured: false,
        });
    }
}

impl CompositorHandler for ClientState {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for ClientState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_output(qh, &output);
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let id = output.id().protocol_id();
        self.surfaces.retain(|s| s.output_id != id);
    }
}

impl SeatHandler for ClientState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Pointer && self.pointer.is_none() {
            let cs = self.compositor.create_surface(qh);
            if let Ok(p) = self.seat_state.get_pointer_with_theme(
                qh,
                &seat,
                self.shm.wl_shm(),
                cs,
                ThemeSpec::System,
            ) {
                self.pointer = Some(p);
            }
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer {
            self.pointer = None;
        }
    }
}

impl PointerHandler for ClientState {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            match ev.kind {
                PointerEventKind::Enter { .. } => {
                    if let Some(p) = &self.pointer {
                        let _ = p.set_cursor(conn, CursorIcon::Default);
                    }
                    self.mouse_x = ev.position.0;
                    self.mouse_y = ev.position.1;
                    let _ = self.input_tx.send(UiInputEvent::Motion {
                        x: self.mouse_x,
                        y: self.mouse_y,
                    });
                }
                PointerEventKind::Motion { .. } => {
                    self.mouse_x = ev.position.0;
                    self.mouse_y = ev.position.1;
                    let _ = self.input_tx.send(UiInputEvent::Motion {
                        x: self.mouse_x,
                        y: self.mouse_y,
                    });
                }
                PointerEventKind::Press { button, .. } => {
                    if button == 272 {
                        // Left mouse button
                        let _ = self.input_tx.send(UiInputEvent::Press {
                            x: self.mouse_x,
                            y: self.mouse_y,
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

impl LayerShellHandler for ClientState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &SctLayerSurface) {}

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer_surface: &SctLayerSurface,
        cfg: LayerSurfaceConfigure,
        _: u32,
    ) {
        for s in &mut self.surfaces {
            if s.layer_surface.wl_surface() == layer_surface.wl_surface() {
                if cfg.new_size.0 > 0 {
                    s.width = cfg.new_size.0;
                }
                if cfg.new_size.1 > 0 {
                    s.height = cfg.new_size.1;
                }
                s.configured = true;
                break;
            }
        }
    }
}

impl ShmHandler for ClientState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl wayland_client::Dispatch<wl_region::WlRegion, ()> for ClientState {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl ProvidesRegistryState for ClientState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_compositor!(ClientState);
smithay_client_toolkit::delegate_output!(ClientState);
smithay_client_toolkit::delegate_seat!(ClientState);
smithay_client_toolkit::delegate_pointer!(ClientState);
smithay_client_toolkit::delegate_layer!(ClientState);
smithay_client_toolkit::delegate_shm!(ClientState);
smithay_client_toolkit::delegate_registry!(ClientState);
