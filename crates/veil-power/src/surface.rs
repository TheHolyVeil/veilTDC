//! Wayland fullscreen overlay surface for veil-power.
//! Uses KeyboardInteractivity::Exclusive and binds ext-background-effect-v1 when available.

use std::os::unix::net::UnixStream;
use std::process::Command;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyboardHandler, KeyEvent, Keysym, Modifiers, RawModifiers, RepeatInfo},
        pointer::{PointerEvent, PointerEventKind, PointerHandler, ThemedPointer, ThemeSpec},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface},
    Connection, EventQueue, QueueHandle,
};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::{self, ExtBackgroundEffectSurfaceV1},
};

use crate::panel::{Action, ClickZone, Panel};
use veil_palette::Tokens;
use veil_text::FontConfig;

pub struct PowerSurface {
    queue: EventQueue<State>,
    state: State,
}

impl PowerSurface {
    pub fn new(tokens: Tokens, font_cfg: Option<&FontConfig>, ipc: Option<UnixStream>) -> Result<Self, Box<dyn std::error::Error>> {
        let conn = Connection::connect_to_env()?;
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();

        let compositor = CompositorState::bind(&globals, &qh)?;
        let layer_shell = LayerShell::bind(&globals, &qh)?;
        let shm = Shm::bind(&globals, &qh)?;
        let seat_state = SeatState::new(&globals, &qh);

        // Check for ext-background-effect-v1
        let bg_effect_manager = match globals.bind::<ExtBackgroundEffectManagerV1, _, _>(&qh, 1..=1, ()) {
            Ok(mgr) => {
                tracing::info!("veil-power: ext-background-effect-v1 manager bound successfully");
                Some(mgr)
            }
            Err(e) => {
                tracing::debug!("veil-power: ext-background-effect-v1 not available ({e})");
                None
            }
        };

        let state = State {
            registry: RegistryState::new(&globals),
            compositor,
            output_state: OutputState::new(&globals, &qh),
            seat_state,
            shm,
            layer_shell,
            bg_effect_manager,
            bg_effect_surface: None,
            surface: None,
            pool: None,
            sw: 0,
            sh: 0,
            configured: false,
            should_close: false,
            mouse_x: 0.0,
            mouse_y: 0.0,
            pressed: false,
            pointer: None,
            click_zones: Vec::new(),
            panel: Panel::new(tokens, font_cfg),
            dirty: true,
            ipc,
        };

        let mut s = Self { queue, state };
        s.queue.roundtrip(&mut s.state)?;
        Ok(s)
    }

    pub fn tick(&mut self) -> Result<bool, Box<dyn std::error::Error>> {
        use rustix::event::{poll, PollFd, PollFlags};
        use rustix::fd::AsFd;
        use rustix::time::Timespec;
        use std::os::unix::io::AsRawFd;

        if let Err(e) = self.queue.flush() {
            tracing::debug!("veil-power flush: {e}");
        }

        if let Some(guard) = self.queue.prepare_read() {
            let raw = self.queue.as_fd().as_raw_fd();
            let bfd = unsafe { rustix::fd::BorrowedFd::borrow_raw(raw) };
            let mut pfd = PollFd::new(&bfd, PollFlags::IN);
            let ts = Timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000, // 10ms
            };
            let ready = poll(std::slice::from_mut(&mut pfd), Some(&ts)).unwrap_or(0);
            if ready > 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;

        if self.state.should_close {
            return Ok(true);
        }

        if self.state.configured && self.state.dirty {
            self.state.dirty = false;
            self.repaint()?;
        }
        Ok(false)
    }

    fn repaint(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let s = &mut self.state;
        if s.sw == 0 || s.sh == 0 {
            return Ok(());
        }
        let Some(ref ls) = s.surface else { return Ok(()); };
        let Some(ref mut pool) = s.pool else { return Ok(()); };

        let (pixels, zones) = s.panel.render(s.sw, s.sh);
        s.click_zones = zones;

        let stride = s.sw * 4;
        let (buffer, canvas) = pool.create_buffer(
            s.sw as i32,
            s.sh as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
        )?;

        let n = canvas.len().min(pixels.len());
        canvas[..n].copy_from_slice(&pixels[..n]);

        let surf = ls.wl_surface();
        buffer.attach_to(surf).ok();
        surf.damage_buffer(0, 0, s.sw as i32, s.sh as i32);
        ls.commit();
        Ok(())
    }
}

// ── Wayland state ─────────────────────────────────────────────────────────────

struct State {
    registry: RegistryState,
    compositor: CompositorState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    layer_shell: LayerShell,
    bg_effect_manager: Option<ExtBackgroundEffectManagerV1>,
    bg_effect_surface: Option<ExtBackgroundEffectSurfaceV1>,
    surface: Option<LayerSurface>,
    pool: Option<SlotPool>,
    sw: u32,
    sh: u32,
    configured: bool,
    should_close: bool,
    mouse_x: f64,
    mouse_y: f64,
    pressed: bool,
    pointer: Option<ThemedPointer>,
    click_zones: Vec<ClickZone>,
    panel: Panel,
    dirty: bool,
    ipc: Option<UnixStream>,
}

impl State {
    fn create_surface(&mut self, qh: &QueueHandle<Self>, output: &wl_output::WlOutput) {
        if self.surface.is_some() {
            return;
        }

        let surface = self.compositor.create_surface(qh);
        let ls = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("veil-power"),
            Some(output),
        );

        // Full-screen: anchor all edges
        ls.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        ls.set_exclusive_zone(-1);
        ls.set_size(0, 0);
        ls.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);

        // If ext-background-effect-v1 is available, request background blur
        if let Some(ref mgr) = self.bg_effect_manager {
            let effect_surface = mgr.get_background_effect(ls.wl_surface(), qh, ());
            let region = self.compositor.wl_compositor().create_region(qh, ());
            // Default initial size covers typical output; configure updates it
            region.add(0, 0, i32::MAX / 2, i32::MAX / 2);
            effect_surface.set_blur_region(Some(&region));
            region.destroy();
            self.bg_effect_surface = Some(effect_surface);
        }

        ls.commit();

        let pool = SlotPool::new(1920 * 1080 * 4, &self.shm).expect("veil-power: pool");
        self.surface = Some(ls);
        self.pool = Some(pool);
    }

    fn update_blur_region(&self, qh: &QueueHandle<Self>, w: u32, h: u32) {
        if let Some(ref effect_surface) = self.bg_effect_surface {
            let region = self.compositor.wl_compositor().create_region(qh, ());
            region.add(0, 0, w as i32, h as i32);
            effect_surface.set_blur_region(Some(&region));
            region.destroy();
        }
    }

    fn execute_action(&mut self, action: Action) {
        tracing::info!("veil-power: executing action {:?}", action);
        // Try IPC first
        let cmd = match action {
            Action::Lock => Some("session.lock"),
            Action::Logout => Some("session.logout"),
            Action::Suspend => Some("session.suspend"),
            Action::Hibernate => None,
            Action::Reboot => Some("session.reboot"),
            Action::Shutdown => Some("session.poweroff"),
        };

        let mut ipc_succeeded = false;
        if let (Some(cmd_name), Some(ref mut stream)) = (cmd, &mut self.ipc) {
            use std::io::Write;
            let msg = veil_ipc::ClientMessage::Cmd {
                id: 1,
                cmd: cmd_name.to_string(),
                args: serde_json::Value::Null,
            };
            if let Ok(bytes) = veil_ipc::encode(&msg) {
                if stream.write_all(&bytes).is_ok() {
                    ipc_succeeded = true;
                }
            }
        }

        if !ipc_succeeded {
            // Standalone fallback
            match action {
                Action::Lock => {
                    let _ = Command::new("veil-lock")
                        .spawn()
                        .or_else(|_| Command::new("swaylock").arg("-f").spawn());
                }
                Action::Logout => {
                    let _ = Command::new("pkill").args(["-TERM", "veil"]).spawn();
                }
                Action::Suspend => {
                    let _ = Command::new("systemctl").arg("suspend").spawn();
                }
                Action::Hibernate => {
                    let _ = Command::new("systemctl").arg("hibernate").spawn();
                }
                Action::Reboot => {
                    let _ = Command::new("systemctl").arg("reboot").spawn();
                }
                Action::Shutdown => {
                    let _ = Command::new("systemctl").arg("poweroff").spawn();
                }
            }
        }
    }

    fn handle_key(&mut self, sym: Keysym) -> bool {
        match sym {
            Keysym::Escape => {
                let close = self.panel.key_escape();
                self.dirty = true;
                close
            }
            Keysym::Return | Keysym::KP_Enter => {
                let (close, action) = self.panel.key_enter();
                self.dirty = true;
                if let Some(act) = action {
                    self.execute_action(act);
                }
                close
            }
            Keysym::Left | Keysym::h => {
                self.panel.nav_prev();
                self.dirty = true;
                false
            }
            Keysym::Right | Keysym::l => {
                self.panel.nav_next();
                self.dirty = true;
                false
            }
            Keysym::Up | Keysym::k => {
                self.panel.nav_up();
                self.dirty = true;
                false
            }
            Keysym::Down | Keysym::j => {
                self.panel.nav_down();
                self.dirty = true;
                false
            }
            Keysym::_1 => self.handle_num(0),
            Keysym::_2 => self.handle_num(1),
            Keysym::_3 => self.handle_num(2),
            Keysym::_4 => self.handle_num(3),
            Keysym::_5 => self.handle_num(4),
            Keysym::_6 => self.handle_num(5),
            _ => false,
        }
    }

    fn handle_num(&mut self, n: usize) -> bool {
        let (close, action) = self.panel.select_number(n);
        self.dirty = true;
        if let Some(act) = action {
            self.execute_action(act);
        }
        close
    }
}

// ── Handler impls ─────────────────────────────────────────────────────────────

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.create_surface(qh, &output);
    }
    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_output::WlOutput,
    ) {
    }
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_output::WlOutput,
    ) {
    }
}

impl SeatHandler for State {
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
            if let Ok(p) = self
                .seat_state
                .get_pointer_with_theme(qh, &seat, self.shm.wl_shm(), cs, ThemeSpec::System)
            {
                self.pointer = Some(p);
            }
        }
        if cap == Capability::Keyboard {
            let _ = self.seat_state.get_keyboard(qh, &seat, None);
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Pointer {
            self.pointer = None;
        }
    }
}

impl KeyboardHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.handle_key(event.keysym) {
            self.should_close = true;
        }
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        match event.keysym {
            Keysym::Left
            | Keysym::Right
            | Keysym::Up
            | Keysym::Down
            | Keysym::h
            | Keysym::j
            | Keysym::k
            | Keysym::l
                if self.handle_key(event.keysym) =>
            {
                self.should_close = true;
            }
            _ => {}
        }
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }

    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: RepeatInfo,
    ) {
    }
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            match ev.kind {
                PointerEventKind::Motion { .. } => {
                    self.mouse_x = ev.position.0;
                    self.mouse_y = ev.position.1;
                    if self
                        .panel
                        .hover(self.mouse_x as f32, self.mouse_y as f32, &self.click_zones)
                    {
                        self.dirty = true;
                    }
                }
                PointerEventKind::Press { button: 0x110, .. } => {
                    self.pressed = true;
                }
                PointerEventKind::Release { button: 0x110, .. } if self.pressed => {
                    self.pressed = false;
                    let (close, action) = self.panel.click(
                        self.mouse_x as f32,
                        self.mouse_y as f32,
                        &self.click_zones,
                    );
                    self.dirty = true;
                    if let Some(act) = action {
                        self.execute_action(act);
                    }
                    if close {
                        self.should_close = true;
                    }
                }
                _ => {}
            }
        }
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.should_close = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        ls: &LayerSurface,
        cfg: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(ref s) = self.surface {
            if s.wl_surface() == ls.wl_surface() {
                if cfg.new_size.0 > 0 {
                    self.sw = cfg.new_size.0;
                }
                if cfg.new_size.1 > 0 {
                    self.sh = cfg.new_size.1;
                }
                self.configured = true;
                self.dirty = true;
                self.update_blur_region(qh, self.sw, self.sh);
            }
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl wayland_client::Dispatch<wl_region::WlRegion, ()> for State {
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

impl wayland_client::Dispatch<ExtBackgroundEffectManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtBackgroundEffectManagerV1,
        event: ext_background_effect_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        tracing::debug!("veil-power: ext_background_effect_manager_v1 event: {:?}", event);
    }
}

impl wayland_client::Dispatch<ExtBackgroundEffectSurfaceV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtBackgroundEffectSurfaceV1,
        _: ext_background_effect_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_compositor!(State);
smithay_client_toolkit::delegate_output!(State);
smithay_client_toolkit::delegate_seat!(State);
smithay_client_toolkit::delegate_pointer!(State);
smithay_client_toolkit::delegate_keyboard!(State);
smithay_client_toolkit::delegate_layer!(State);
smithay_client_toolkit::delegate_shm!(State);
smithay_client_toolkit::delegate_registry!(State);
