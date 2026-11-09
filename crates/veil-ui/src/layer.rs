//! Wayland layer-shell surface client runtime.
//!
//! Wraps `smithay-client-toolkit` to create, configure, and manage layer-shell
//! surfaces across all connected outputs with SHM slot-pool presentation.

use std::sync::mpsc::{channel, Receiver, Sender};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
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
    /// `output` is the wl_output name of the surface the pointer is over
    /// ("veil-host-1" under veil), so a multi-monitor client knows which
    /// surface's layout `x`/`y` belong to.
    Motion { x: f64, y: f64, output: String },
    Press { x: f64, y: f64, output: String },
    /// Vertical scroll: negative is up, positive is down.
    Scroll { x: f64, y: f64, dy: f64, output: String },
    /// Right mouse button (the left one is `Press`).
    PressRight { x: f64, y: f64, output: String },
    /// The pointer left the surface of `output`.
    Leave { output: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfacePosition {
    Top,
    Bottom,
    CenterOverlay,
}

/// Everything a layer-shell client chooses about its surface. `new` covers
/// the bar's needs; tests and other components use `with_options`.
/// Which outputs get a surface.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OutputFilter {
    /// Every output (a bar).
    #[default]
    All,
    /// Only the first output the compositor announces (a toast on a primary monitor).
    First,
    /// Only the output with this name.
    Named(String),
}

#[derive(Debug, Clone)]
pub struct LayerOptions {
    pub namespace: String,
    pub layer: Layer,
    pub anchor: Anchor,
    /// Requested size; 0 on an axis means "stretch to the anchors".
    pub size: (u32, u32),
    pub exclusive_zone: i32,
    pub keyboard: KeyboardInteractivity,
    /// Margins (top, right, bottom, left), measured from the compositor's usable
    /// area, so a bar's exclusive zone is already excluded.
    pub margin: (i32, i32, i32, i32),
    pub output: OutputFilter,
}

struct OutputSurface {
    output_id: u32,
    /// wl_output name ("veil-host-1" under veil), "" if the compositor sent none.
    name: String,
    layer_surface: SctLayerSurface,
    pool: SlotPool,
    width: u32,
    height: u32,
    configured: bool,
}

pub struct LayerShellClient {
    conn: Connection,
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
        let anchor = match position {
            SurfacePosition::Top => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            SurfacePosition::Bottom => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            SurfacePosition::CenterOverlay => Anchor::empty(),
        };
        Self::with_options(
            LayerOptions {
                namespace: namespace.to_string(),
                layer,
                anchor,
                size: (0, height),
                exclusive_zone: if exclusive { height as i32 } else { 0 },
                keyboard: KeyboardInteractivity::None,
                margin: (0, 0, 0, 0),
                output: OutputFilter::All,
            },
            None,
        )
    }

    /// Full control over the surface. `stream` connects to a specific
    /// compositor socket instead of `$WAYLAND_DISPLAY`, so a test can talk to
    /// an in-process compositor without touching the process environment.
    pub fn with_options(
        opts: LayerOptions,
        stream: Option<std::os::unix::net::UnixStream>,
    ) -> Result<Self, String> {
        let conn = match stream {
            Some(s) => Connection::from_socket(s),
            None => Connection::connect_to_env(),
        }
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
            opts,
            surfaces: Vec::new(),
            pointer: None,
            input_tx: tx,
            mouse_x: 0.0,
            mouse_y: 0.0,
            hover: String::new(),
            reconfigured: false,
        };

        let mut client = Self {
            conn,
            queue,
            state,
            input_rx: rx,
        };
        let _ = client.queue.roundtrip(&mut client.state);
        Ok(client)
    }

    pub fn dispatch(&mut self) -> Result<(), String> {
        let _ = self.queue.flush();
        // Pull whatever the compositor sent off the socket (non-blocking).
        // `dispatch_pending` alone only handles events something else already
        // read, so without this the bar never saw a later configure/resize,
        // a monitor appearing, or any pointer event after the first roundtrip.
        if let Some(guard) = self.queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("Wayland read error: {e}")),
            }
        }
        self.queue.dispatch_pending(&mut self.state)
            .map_err(|e| format!("Dispatch error: {e}"))?;
        if self.state.surfaces.iter().any(|s| !s.configured) {
            let _ = self.queue.roundtrip(&mut self.state);
        }
        Ok(())
    }

    /// True once since the last call if a surface was configured or resized
    /// by the compositor. Redraw when it is: the committed buffer is still the
    /// old size until you do.
    /// Change the surface's geometry on every output at runtime (bar height
    /// or edge changed, font size changed). The compositor answers with a
    /// configure and adjusts the exclusive zone.
    pub fn reconfigure(&mut self, anchor: Anchor, size: (u32, u32), exclusive_zone: i32) {
        self.state.opts.anchor = anchor;
        self.state.opts.size = size;
        self.state.opts.exclusive_zone = exclusive_zone;
        for s in &self.state.surfaces {
            s.layer_surface.set_anchor(anchor);
            s.layer_surface.set_size(size.0, size.1);
            s.layer_surface.set_exclusive_zone(exclusive_zone);
            s.layer_surface.commit();
        }
    }

    /// Margins (top, right, bottom, left) on every surface.
    pub fn set_margin(&mut self, margin: (i32, i32, i32, i32)) {
        self.state.opts.margin = margin;
        for s in &self.state.surfaces {
            s.layer_surface.set_margin(margin.0, margin.1, margin.2, margin.3);
            s.layer_surface.commit();
        }
    }

    /// Restrict pointer input on `output`'s surface to `rects` (x, y, width,
    /// height in surface coordinates). An empty slice makes the whole surface
    /// click-through. Commits by itself; unconfigured surfaces are skipped.
    pub fn set_input_region(&mut self, output: &str, rects: &[(i32, i32, i32, i32)]) {
        for s in self.state.surfaces.iter().filter(|s| s.name == output && s.configured) {
            match Region::new(&self.state.compositor) {
                Ok(region) => {
                    for &(x, y, w, h) in rects {
                        if w > 0 && h > 0 {
                            region.add(x, y, w, h);
                        }
                    }
                    s.layer_surface.wl_surface().set_input_region(Some(region.wl_region()));
                    s.layer_surface.commit();
                }
                Err(e) => tracing::warn!("input region for {output}: {e}"),
            }
        }
    }

    /// Names of the outputs that have a surface (as the compositor reports them).
    pub fn output_names(&self) -> Vec<String> {
        self.state.surfaces.iter().map(|s| s.name.clone()).collect()
    }

    /// The Wayland socket's fd, so a caller can `poll(2)` it together with its
    /// other sources instead of waking on a timer. Valid while `self` lives.
    pub fn wayland_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.conn.backend().poll_fd().as_raw_fd()
    }

    pub fn take_reconfigured(&mut self) -> bool {
        std::mem::take(&mut self.state.reconfigured)
    }

    /// Render and commit pixels to each configured output surface.
    /// `render` gets the output's name so one process can draw something
    /// different on each monitor.
    pub fn present_for_each<F: FnMut(&str, u32, u32) -> Vec<u8>>(&mut self, mut render: F) {
        for surf in &mut self.state.surfaces {
            if surf.width == 0 || surf.height == 0 {
                continue;
            }
            let pixels = render(&surf.name, surf.width, surf.height);
            let stride = surf.width * 4;
            if let Ok((buffer, canvas)) = surf.pool.create_buffer(
                surf.width as i32,
                surf.height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
            ) {
                let len = canvas.len().min(pixels.len());
                // tiny-skia hands back premultiplied R,G,B,A bytes; wl_shm
                // Argb8888 is B,G,R,A in memory (little-endian). Copying
                // straight swaps red and blue on screen.
                let (dst_px, _) = canvas[..len].as_chunks_mut::<4>();
                let (src_px, _) = pixels[..len].as_chunks::<4>();
                for (dst, src) in dst_px.iter_mut().zip(src_px) {
                    dst[0] = src[2];
                    dst[1] = src[1];
                    dst[2] = src[0];
                    dst[3] = src[3];
                }
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
    opts: LayerOptions,
    surfaces: Vec<OutputSurface>,
    pointer: Option<ThemedPointer<()>>,
    input_tx: Sender<UiInputEvent>,
    mouse_x: f64,
    mouse_y: f64,
    /// Output name of the surface the pointer last entered.
    hover: String,
    /// Set when any surface was (re)configured; the app must redraw at the
    /// new size. Read and cleared by `LayerShellClient::take_reconfigured`.
    reconfigured: bool,
}

impl ClientState {
    fn output_name_of(&self, wl: &wl_surface::WlSurface) -> String {
        self.surfaces
            .iter()
            .find(|s| s.layer_surface.wl_surface() == wl)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }

    fn add_output(&mut self, qh: &QueueHandle<Self>, output: &wl_output::WlOutput) {
        let output_id = output.id().protocol_id();
        if self.surfaces.iter().any(|s| s.output_id == output_id) {
            return;
        }

        let name = self
            .output_state
            .info(output)
            .and_then(|i| i.name)
            .unwrap_or_default();
        match &self.opts.output {
            OutputFilter::All => {}
            OutputFilter::First if !self.surfaces.is_empty() => return,
            OutputFilter::First => {}
            OutputFilter::Named(want) if *want != name => return,
            OutputFilter::Named(_) => {}
        }
        let surface = self.compositor.create_surface(qh);
        let layer_surface = self.layer_shell.create_layer_surface(
            qh,
            surface,
            self.opts.layer,
            Some(&self.opts.namespace),
            Some(output),
        );

        layer_surface.set_anchor(self.opts.anchor);
        layer_surface.set_exclusive_zone(self.opts.exclusive_zone);
        layer_surface.set_keyboard_interactivity(self.opts.keyboard);
        layer_surface.set_size(self.opts.size.0, self.opts.size.1);
        let (mt, mr, mb, ml) = self.opts.margin;
        layer_surface.set_margin(mt, mr, mb, ml);
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
            name,
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
            if let Ok(p) = self.seat_state.get_pointer_with_theme_and_data(
                qh,
                &seat,
                self.shm.wl_shm(),
                cs,
                ThemeSpec::System,
                (),
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
                    self.hover = self.output_name_of(&ev.surface);
                    let _ = self.input_tx.send(UiInputEvent::Motion {
                        x: self.mouse_x,
                        y: self.mouse_y,
                        output: self.hover.clone(),
                    });
                }
                PointerEventKind::Leave { .. } => {
                    let output = self.output_name_of(&ev.surface);
                    let _ = self.input_tx.send(UiInputEvent::Leave { output });
                }
                PointerEventKind::Motion { .. } => {
                    self.mouse_x = ev.position.0;
                    self.mouse_y = ev.position.1;
                    let _ = self.input_tx.send(UiInputEvent::Motion {
                        x: self.mouse_x,
                        y: self.mouse_y,
                        output: self.hover.clone(),
                    });
                }
                PointerEventKind::Axis { vertical, .. } => {
                    // Wheels report whole notches; touchpads report distance.
                    let dy = if vertical.discrete != 0 {
                        vertical.discrete as f64
                    } else {
                        vertical.absolute.signum()
                    };
                    if dy != 0.0 {
                        let _ = self.input_tx.send(UiInputEvent::Scroll {
                            x: self.mouse_x,
                            y: self.mouse_y,
                            dy,
                            output: self.hover.clone(),
                        });
                    }
                }
                PointerEventKind::Press { button: 273, .. } => {
                    let _ = self.input_tx.send(UiInputEvent::PressRight {
                        x: self.mouse_x,
                        y: self.mouse_y,
                        output: self.hover.clone(),
                    });
                }
                PointerEventKind::Press { button: 272, .. } => {
                    // Left mouse button
                        let _ = self.input_tx.send(UiInputEvent::Press {
                            x: self.mouse_x,
                            y: self.mouse_y,
                            output: self.hover.clone(),
                        });
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
                self.reconfigured = true;
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

smithay_client_toolkit::delegate_dispatch2!(ClientState);
smithay_client_toolkit::delegate_registry!(ClientState);
