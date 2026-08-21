//! Smithay-based nested compositor: socket, globals, dispatch loop.
//!
//! v1 scope: single client, single fullscreen toplevel, shm + dmabuf buffers,
//! keyboard + pointer + scroll, wl_output advertisement, xdg_activation stub.
//! dmabuf: single-plane linear ARGB/XRGB/ABGR/XBGR 8888 accepted via mmap;
//! tiled/compressed modifiers fall back to shm gracefully.

use std::collections::HashMap;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use calloop::{
    generic::{FdWrapper, Generic},
    timer::{TimeoutAction, Timer},
    EventLoop, Interest, Mode as CMode, PostAction,
};

use smithay::{
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_dmabuf,
    delegate_fractional_scale, delegate_idle_inhibit, delegate_keyboard_shortcuts_inhibit,
    delegate_output, delegate_pointer_constraints, delegate_presentation,
    delegate_primary_selection, delegate_relative_pointer, delegate_tablet_manager,
    delegate_seat, delegate_shm, delegate_text_input_manager, delegate_viewporter,
    delegate_xdg_activation, delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{PopupKind, PopupManager},
    input::{
        keyboard::{keysyms, FilterResult, KeyboardHandle, KeysymHandle, ModifiersState, XkbConfig},
        pointer::{
            AxisFrame, ButtonEvent, CursorImageAttributes, CursorImageStatus, MotionEvent,
            PointerHandle,
        },
        Seat, SeatHandler, SeatState,
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel},
    reexports::wayland_server::{
        backend::{ClientData, ClientId, DisconnectReason, ObjectId},
        protocol::{wl_buffer, wl_seat, wl_shm, wl_surface::WlSurface},
        Client, Display, DisplayHandle, Resource,
    },
    utils::{Logical, Point, Serial, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            with_states, with_surface_tree_downward, BufferAssignment,
            CompositorClientState, CompositorHandler, CompositorState, SubsurfaceCachedState,
            SurfaceAttributes, TraversalAction,
        },
        dmabuf::{
            get_dmabuf, DmabufFeedback, DmabufFeedbackBuilder,
            DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
        },
        fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState},
        output::{OutputHandler, OutputManagerState},
        presentation::PresentationState,
        selection::{SelectionHandler, SelectionSource, SelectionTarget},
        selection::data_device::{
            ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            request_data_device_client_selection, set_data_device_focus, set_data_device_selection,
        },
        shell::xdg::{
            decoration::{XdgDecorationHandler, XdgDecorationState},
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        cursor_shape::CursorShapeManagerState,
        idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState},
        tablet_manager::{TabletManagerState, TabletSeatHandler},
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState,
            KeyboardShortcutsInhibitor,
        },
        pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState},
        relative_pointer::RelativePointerManagerState,
        selection::primary_selection::{PrimarySelectionHandler, PrimarySelectionState},
        shm::{with_buffer_contents, ShmHandler, ShmState},
        text_input::{TextInputManagerState, TextInputSeat},
        viewporter::ViewporterState,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        socket::ListeningSocketSource,
    },
    xwayland::{XWayland, XWaylandClientData, XWaylandEvent},
};
use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use wayland_protocols::xdg::shell::server::xdg_toplevel;

use smithay::backend::allocator::{
    Buffer as AllocBuffer, Fourcc, Modifier,
    dmabuf::{Dmabuf, DmabufMappingMode},
};

use crate::{input::InputCmd, sink::Frame};
use crate::layout::{Layout, Rect};
use crate::detile::GpuImporter;
use crate::launcher::Launcher;

// ─── State ────────────────────────────────────────────────────────────────────

pub struct State {
    pub compositor_state:  CompositorState,
    pub xdg_shell_state:   XdgShellState,
    pub shm_state:         ShmState,
    pub seat_state:        SeatState<Self>,
    pub xdg_activation:    XdgActivationState,
    pub output_manager:    OutputManagerState,
    pub dmabuf_state:      DmabufState,
    pub _dmabuf_global:    DmabufGlobal,
    /// GPU dmabuf importer for tiled/non-linear client buffers. `None` when no
    /// render node / EGL is available — veil then stays CPU-only + linear-only.
    pub gpu:               Option<GpuImporter>,
    pub _data_device:      DataDeviceState,
    pub _xdg_decoration:        XdgDecorationState,
    pub _viewporter:            ViewporterState,
    pub _fractional:            FractionalScaleManagerState,
    pub _presentation:          PresentationState,
    pub _text_input:            TextInputManagerState,
    pub _primary_sel:           PrimarySelectionState,
    pub _cursor_shape:          CursorShapeManagerState,
    pub _pointer_constraints:   PointerConstraintsState,
    pub _relative_pointer:      RelativePointerManagerState,
    pub _idle_inhibit:          IdleInhibitManagerState,
    pub _kb_inhibit:            KeyboardShortcutsInhibitState,
    pub _tablet:                TabletManagerState,
    pub seat:              Seat<Self>,
    pub keyboard:          KeyboardHandle<Self>,
    pub pointer:           PointerHandle<Self>,
    pub output:            Output,
    pub output_w:          u32,
    pub output_h:          u32,
    /// Last absolute pointer position; pointer.motion() needs an absolute
    /// location, so we keep track of it across button/scroll events.
    pub pointer_pos:       (f64, f64),
    pub toplevels:         Vec<ToplevelSurface>,
    /// Dwindle tiling state (focus + split orientation).
    pub layout:            Layout,
    /// Per-window rects, indexed to match the live-toplevel order. Recomputed
    /// by `relayout` whenever the window set or output size changes.
    pub layout_rects:      Vec<Rect>,
    /// Parsed `keybinds` config (Combo 4). Super+/ (hardcoded, not itself
    /// configurable) toggles `show_help`.
    pub keybinds:          veil_config::Keybinds,
    pub show_help:         bool,
    /// Bare background color (RGBA, alpha always 255) — fills the composite
    /// buffer before any window blit, so uncovered space is a solid color
    /// instead of black.
    pub background:        [u8; 4],
    /// `<mod_key>+D` app launcher — `Some` while the modal is open. See
    /// `crate::launcher`. Not itself a `keybinds` config entry, same as help.
    pub launcher:          Option<Launcher>,
    /// Own Wayland socket name, so launcher-spawned clients can connect back
    /// into us (`WAYLAND_DISPLAY=<this>`).
    pub socket_name:       String,
    pub popups:            PopupManager,
    pub surface_buffers:   HashMap<ObjectId, SurfaceBuf>,
    pub cursor_status:     CursorImageStatus,
    pub dirty:             bool,
    pub last_composite:    Option<Instant>,
    pub frame_tx:          mpsc::Sender<Frame>,
    pub serial_counter:    u32,
    pub frame_serial:      u64,
    pub running:           bool,
    /// Same `Arc<AtomicBool>` `run()` was handed for external shutdown
    /// (Ctrl-C/SIGTERM/`veil-host stop`) — Shift+Alt+E sets it from the
    /// inside too, same graceful-quit path either direction.
    pub stop:              Arc<AtomicBool>,
    pub start_time:        Instant,
    pub display_handle:       DisplayHandle,
    pub host_clipboard:       Option<String>,
    pub clipboard_rx:         mpsc::Receiver<String>,
    pub pending_copy_out:     bool,
    pub client_has_selection: bool,
    pub composite_buf:        Vec<u8>,
}

/// Per-surface RGBA cache entry. We re-blit these every dirty tick.
pub struct SurfaceBuf {
    pub rgba: Vec<u8>,
    pub w:    u32,
    pub h:    u32,
}

impl State {
    fn next_serial(&mut self) -> Serial {
        self.serial_counter = self.serial_counter.wrapping_add(1);
        Serial::from(self.serial_counter)
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) { tracing::debug!("client connected"); }
    fn disconnected(&self, _id: ClientId, _r: DisconnectReason) { tracing::debug!("client gone"); }
}

// ─── Handler impls ────────────────────────────────────────────────────────────

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState { &mut self.compositor_state }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // XWayland clients have their own client-data type. Try both.
        if let Some(d) = client.get_data::<ClientState>() {
            return &d.compositor_state;
        }
        if let Some(d) = client.get_data::<XWaylandClientData>() {
            return &d.compositor_state;
        }
        eprintln!("[veil-host] unknown client type — disconnecting");
        static FALLBACK: std::sync::OnceLock<CompositorClientState> = std::sync::OnceLock::new();
        FALLBACK.get_or_init(CompositorClientState::default)
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Pull the newly-attached buffer (if any) and cache it as RGBA
        // keyed by surface id. We re-composite from all caches on tick.
        enum Assign { New(wl_buffer::WlBuffer), Removed, None }
        let assign = with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            match guard.current().buffer.take() {
                Some(BufferAssignment::NewBuffer(b)) => Assign::New(b),
                Some(BufferAssignment::Removed)      => Assign::Removed,
                None                                 => Assign::None,
            }
        });

        match assign {
            Assign::New(buffer) => {
                // Try shm first, then dmabuf.
                let imported = with_buffer_contents(&buffer, |ptr, len, data| {
                    tracing::info!("commit {} shm {}x{} fmt={:?}", surface.id(), data.width, data.height, data.format);
                    let raw = unsafe { std::slice::from_raw_parts(ptr, len) };
                    shm_to_rgba(raw, &data)
                })
                .ok()
                .flatten()
                .or_else(|| {
                    let dmabuf = get_dmabuf(&buffer).ok()?;
                    tracing::info!("commit {} dma {}x{} fmt={:?} mod={:?}",
                        surface.id(), dmabuf.width(), dmabuf.height(),
                        dmabuf.format().code, dmabuf.format().modifier);
                    // Linear → CPU mmap (fast, no GPU roundtrip). Anything else
                    // (tiled / implicit modifier) → GPU detile via EGLImage, if
                    // an importer is up; otherwise unsupported → blank.
                    if dmabuf.format().modifier == Modifier::Linear {
                        import_dmabuf(dmabuf)
                    } else if let Some(gpu) = self.gpu.as_mut() {
                        gpu.import(dmabuf)
                    } else {
                        None
                    }
                });

                if let Some((rgba, w, h)) = imported {
                    tracing::info!("commit {} → surface_buffers {}x{}", surface.id(), w, h);
                    self.surface_buffers.insert(surface.id(), SurfaceBuf { rgba, w, h });
                    self.dirty = true;
                } else {
                    tracing::warn!("commit {} — unsupported buffer type, skipping", surface.id());
                }
                buffer.release();
            }
            Assign::Removed => {
                self.surface_buffers.remove(&surface.id());
                self.dirty = true;
            }
            Assign::None => {
                // Pure state commit (geometry, role config). Still mark
                // dirty so subsurface-offset changes get re-composited.
                self.dirty = true;
            }
        }

        // Let PopupManager update its internal book-keeping.
        self.popups.commit(surface);
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        if self.surface_buffers.remove(&surface.id()).is_some() {
            self.dirty = true;
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState { &self.shm_state }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState { &mut self.xdg_shell_state }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // We DON'T force Fullscreen — weston-terminal, thunar and friends gate
        // input/decoration on !Fullscreen. The tiled size is sent by relayout.
        let wl = surface.wl_surface().clone();
        let serial = self.next_serial();
        let kb = self.keyboard.clone();
        kb.set_focus(self, Some(wl), serial);

        self.toplevels.push(surface);
        // New window takes focus; retile so every window gets its rect + size.
        let n = self.toplevels.iter().filter(|t| t.alive()).count();
        self.layout.focused = n.saturating_sub(1);
        relayout(self);
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        surface.with_pending_state(|s| {
            s.geometry = positioner.get_geometry();
            s.positioner = positioner;
        });
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("track_popup failed: {e:?}");
        }
    }
    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}
    fn reposition_request(&mut self, _s: PopupSurface, _p: PositionerState, _t: u32) {}
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus  = WlSurface;
    type TouchFocus    = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> { &mut self.seat_state }
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|s| s.client());
        set_data_device_focus::<State>(&self.display_handle, seat, client);
        // Text input focus tracks keyboard focus — required for Chromium text fields.
        seat.text_input().set_focus(focused.cloned());
    }
    fn cursor_image(&mut self, _s: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.dirty = true;
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();

    fn new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>, _seat: Seat<Self>) {
        if ty != SelectionTarget::Clipboard { return; }
        self.client_has_selection = source.is_some();
        if source.is_some() {
            // Schedule a deferred read: Smithay updates seat_data AFTER this callback returns,
            // so we request the data on the next tick when seat_data is current.
            self.pending_copy_out = true;
        }
    }

    fn send_selection(&mut self, ty: SelectionTarget, _mime_type: String, fd: OwnedFd, _seat: Seat<Self>, _user_data: &()) {
        if ty != SelectionTarget::Clipboard { return; }
        let Some(text) = self.host_clipboard.clone() else { return; };
        std::thread::spawn(move || {
            use std::io::Write;
            let mut f: std::fs::File = fd.into();
            let _ = f.write_all(text.as_bytes());
        });
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState { &self._data_device }
}
impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

impl OutputHandler for State {}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState { &self._primary_sel }
}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {}
    fn cursor_position_hint(&mut self, _: &WlSurface, _: &PointerHandle<Self>, _: Point<f64, Logical>) {}
}

impl IdleInhibitHandler for State {
    fn inhibit(&mut self, _surface: WlSurface) {}
    fn uninhibit(&mut self, _surface: WlSurface) {}
}

impl TabletSeatHandler for State {}

impl KeyboardShortcutsInhibitHandler for State {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self._kb_inhibit
    }
    // Always grant inhibition — we have no keyboard shortcuts of our own to protect.
    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState { &mut self.dmabuf_state }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let fmt = dmabuf.format();

        // Fast path: single-plane LINEAR in a format our CPU mmap converter
        // handles (see import_dmabuf) — no GPU needed.
        let cpu_ok = dmabuf.num_planes() == 1
            && fmt.modifier == Modifier::Linear
            && matches!(
                fmt.code,
                Fourcc::Argb8888 | Fourcc::Xrgb8888 | Fourcc::Abgr8888 | Fourcc::Xbgr8888
            );

        if cpu_ok {
            let _ = notifier.successful::<State>();
            return;
        }

        // GPU path: any tiled / non-linear buffer, imported as an EGLImage and
        // read back linear (see detile::GpuImporter). Actually trial-import it
        // now so we only accept what we can genuinely detile.
        if let Some(gpu) = self.gpu.as_mut() {
            if gpu.can_import(&dmabuf) {
                let _ = notifier.successful::<State>();
                return;
            }
        }

        // No GPU, or the buffer won't import → reject. The client falls back to
        // shm (software) rendering, which we always handle, instead of handing
        // us a buffer we'd render blank or freeze trying to CPU-map.
        notifier.failed();
    }
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // We don't draw decorations; ask client to do it itself.
        toplevel.with_pending_state(|s| {
            s.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ClientSide);
        });
        toplevel.send_configure();
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: zxdg_toplevel_decoration_v1::Mode) {
        toplevel.with_pending_state(|s| {
            s.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ClientSide);
        });
        toplevel.send_configure();
    }
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| {
            s.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ClientSide);
        });
        toplevel.send_configure();
    }
}

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, _surface: WlSurface) {}
}

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState { &mut self.xdg_activation }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _data:  XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Simple policy: always grant — focus the requesting surface for keyboard.
        let serial = self.next_serial();
        let kb = self.keyboard.clone();
        kb.set_focus(self, Some(surface), serial);
    }
}

delegate_compositor!(State);
delegate_cursor_shape!(State);
delegate_data_device!(State);
delegate_idle_inhibit!(State);
delegate_keyboard_shortcuts_inhibit!(State);
delegate_pointer_constraints!(State);
delegate_primary_selection!(State);
delegate_relative_pointer!(State);
delegate_shm!(State);
delegate_tablet_manager!(State);
delegate_text_input_manager!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
delegate_output!(State);
delegate_xdg_activation!(State);
delegate_dmabuf!(State);
delegate_xdg_decoration!(State);
delegate_viewporter!(State);
delegate_fractional_scale!(State);
delegate_presentation!(State);

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Build the default dmabuf feedback advertised to clients via linux-dmabuf-v4.
///
/// We claim the first accessible DRM render node as the main device. The linear
/// single-plane formats are always advertised — our map_plane mmap path imports
/// those with no GPU. When a [`GpuImporter`] is up we additionally advertise the
/// render node's full format+modifier set, so GPU compositors (niri, Hyprland)
/// and GL/Vulkan apps allocate their native *tiled* buffers, which we detile via
/// EGLImage. Chromium (feedback-aware) still picks linear and takes the CPU path.
fn build_dmabuf_feedback(gpu: &Option<GpuImporter>) -> DmabufFeedback {
    use std::os::unix::fs::MetadataExt;
    use smithay::backend::allocator::Format;

    // Walk render nodes to find the first accessible one. dev_t tells clients
    // which GPU device to allocate on (must match the node it opens).
    let dev_t: libc::dev_t = (128..=135u32)
        .map(|n| format!("/dev/dri/renderD{n}"))
        .find_map(|path| std::fs::metadata(&path).ok().map(|m| m.rdev()))
        .unwrap_or(0);

    if dev_t == 0 {
        eprintln!("[veil-host] dmabuf: no DRM render node found, feedback dev_t=0");
    } else {
        eprintln!("[veil-host] dmabuf feedback: dev_t={dev_t:#x} (renderD{})", (dev_t & 0xFF));
    }

    // Always-importable linear formats (CPU mmap path).
    let mut formats: Vec<Format> = vec![
        Format { code: Fourcc::Argb8888, modifier: Modifier::Linear },
        Format { code: Fourcc::Xrgb8888, modifier: Modifier::Linear },
        Format { code: Fourcc::Abgr8888, modifier: Modifier::Linear },
        Format { code: Fourcc::Xbgr8888, modifier: Modifier::Linear },
    ];

    // Everything the render node can import (tiled modifiers included).
    if let Some(g) = gpu {
        for f in g.formats() {
            if !formats.contains(&f) {
                formats.push(f);
            }
        }
        eprintln!("[veil-host] dmabuf feedback: {} formats (GPU detile enabled)", formats.len());
    } else {
        eprintln!("[veil-host] dmabuf feedback: linear-only (no GPU importer)");
    }

    DmabufFeedbackBuilder::new(dev_t, formats)
        .build()
        .expect("[veil-host] failed to build dmabuf feedback")
}

/// Import a linear dmabuf by mmapping plane 0 and converting pixels to RGBA.
/// Only single-plane ARGB/XRGB/ABGR/XBGR 8888 with linear layout are supported.
fn import_dmabuf(dmabuf: &Dmabuf) -> Option<(Vec<u8>, u32, u32)> {
    if dmabuf.num_planes() != 1 { return None; }
    let fmt    = dmabuf.format();
    // Backstop: this is the CPU fast path — only ever mmap a LINEAR buffer.
    // Tiled / non-linear buffers go through the GPU detile path (commit routes
    // them to GpuImporter); a stray one reaching map_plane here would freeze the
    // compositor thread on an uncached/detiled CPU read, so bail.
    if fmt.modifier != Modifier::Linear { return None; }
    let w      = dmabuf.width()  as usize;
    let h      = dmabuf.height() as usize;
    let stride = dmabuf.strides().next()? as usize;
    let offset = dmabuf.offsets().next()? as usize;

    if stride < w * 4 { return None; }

    let mapping = dmabuf.map_plane(0, DmabufMappingMode::READ).ok()?;
    let raw = unsafe { std::slice::from_raw_parts(mapping.ptr() as *const u8, mapping.length()) };

    let pixel_data = raw.get(offset..)?;
    if pixel_data.len() < stride * h { return None; }

    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = &pixel_data[y * stride .. y * stride + w * 4];
        for px in row.chunks_exact(4) {
            // DRM stores as little-endian u32: ARGB8888 = B,G,R,A in memory
            let (r, g, b) = match fmt.code {
                Fourcc::Argb8888 | Fourcc::Xrgb8888 => (px[2], px[1], px[0]),
                Fourcc::Abgr8888 | Fourcc::Xbgr8888 => (px[0], px[1], px[2]),
                _ => return None,
            };
            out.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Some((out, w as u32, h as u32))
}

fn shm_to_rgba(raw: &[u8], data: &smithay::wayland::shm::BufferData) -> Option<(Vec<u8>, u32, u32)> {
    let w = data.width  as usize;
    let h = data.height as usize;
    let stride = data.stride as usize;
    if stride < w * 4 || raw.len() < stride * h { return None; }

    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row_start = data.offset as usize + y * stride;
        let row = &raw[row_start .. row_start + w * 4];
        for px in row.chunks_exact(4) {
            let (r, g, b) = match data.format {
                wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => (px[0], px[1], px[2]),
                wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => (px[2], px[1], px[0]),
                _ => return None,
            };
            out.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Some((out, w as u32, h as u32))
}

/// Alpha-over blit `src` onto `back` at `(x, y)`. Clips to back bounds.
fn blit(back: &mut [u8], back_w: u32, back_h: u32, src: &SurfaceBuf, x: i32, y: i32) {
    let bw = back_w as i32;
    let bh = back_h as i32;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + src.w as i32).min(bw);
    let y1 = (y + src.h as i32).min(bh);
    if x0 >= x1 || y0 >= y1 { return; }
    for dy in y0..y1 {
        let sy = (dy - y) as u32;
        let drow = (dy as u32 * back_w * 4) as usize;
        let srow = (sy * src.w * 4) as usize;
        for dx in x0..x1 {
            let sx  = (dx - x) as u32;
            let di  = drow + (dx as u32 * 4) as usize;
            let si  = srow + (sx * 4) as usize;
            let a   = src.rgba[si + 3] as u32;
            if a == 255 {
                back[di..di + 4].copy_from_slice(&src.rgba[si..si + 4]);
            } else if a > 0 {
                let inv = 255 - a;
                back[di]     = ((src.rgba[si]     as u32 * a + back[di]     as u32 * inv) / 255) as u8;
                back[di + 1] = ((src.rgba[si + 1] as u32 * a + back[di + 1] as u32 * inv) / 255) as u8;
                back[di + 2] = ((src.rgba[si + 2] as u32 * a + back[di + 2] as u32 * inv) / 255) as u8;
                back[di + 3] = 255;
            }
        }
    }
}

/// 12×16 white-on-black arrow. '#' = white opaque, '.' = black opaque,
/// ' ' = transparent. Hotspot at (0, 0) = top-left, matching X11 default.
const ARROW: &[&[u8; 12]; 16] = &[
    b"#           ",
    b"##          ",
    b"#.#         ",
    b"#..#        ",
    b"#...#       ",
    b"#....#      ",
    b"#.....#     ",
    b"#......#    ",
    b"#.......#   ",
    b"#........#  ",
    b"#.....#####.",
    b"#..#..#     ",
    b"#.# #..#    ",
    b"##  #..#    ",
    b"#    #..#   ",
    b"     ####   ",
];

fn draw_fallback_cursor(back: &mut [u8], back_w: u32, back_h: u32, x: i32, y: i32) {
    for (dy, row) in ARROW.iter().enumerate() {
        let py = y + dy as i32;
        if py < 0 || py as u32 >= back_h { continue; }
        for (dx, &ch) in row.iter().enumerate() {
            let px = x + dx as i32;
            if px < 0 || px as u32 >= back_w { continue; }
            let rgb = match ch {
                b'#' => Some([255u8, 255, 255]),
                b'.' => Some([0u8, 0, 0]),
                _    => None,
            };
            if let Some(c) = rgb {
                let i = ((py as u32 * back_w + px as u32) * 4) as usize;
                back[i]     = c[0];
                back[i + 1] = c[1];
                back[i + 2] = c[2];
                back[i + 3] = 255;
            }
        }
    }
}

/// Walk `root`'s surface tree, blitting each surface's cached buffer
/// into `back` at its accumulated subsurface offset (added to `origin`).
fn blit_subtree(
    back:   &mut [u8],
    back_w: u32,
    back_h: u32,
    cache:  &HashMap<ObjectId, SurfaceBuf>,
    root:   &WlSurface,
    origin: (i32, i32),
) {
    with_surface_tree_downward(
        root,
        origin,
        |surface, states, &parent_origin: &(i32, i32)| {
            // Root has no SubsurfaceCachedState; its offset is (0,0).
            // Children are positioned at parent_origin + subsurface.location.
            let here = if surface == root {
                parent_origin
            } else {
                let mut g = states.cached_state.get::<SubsurfaceCachedState>();
                let loc = g.current().location;
                (parent_origin.0 + loc.x, parent_origin.1 + loc.y)
            };
            if let Some(buf) = cache.get(&surface.id()) {
                blit(back, back_w, back_h, buf, here.0, here.1);
            }
            TraversalAction::DoChildren(here)
        },
        |_, _, _| {},
        |_, _, _| true,
    );
}

/// Lowercased ASCII char for a keysym, ignoring Shift — so a configured
/// `"h"` bind matches whether or not the modifier chord also holds Shift.
fn keysym_char(keysym: KeysymHandle<'_>) -> Option<char> {
    let cp = smithay::input::keyboard::xkb::keysym_to_utf32(keysym.modified_sym());
    char::from_u32(cp).map(|c| c.to_ascii_lowercase())
}

/// Unconditionally seed native Wayland flags so browsers (Helium, Chrome),
/// Electron apps, Firefox, Qt, GTK, and SDL apps run natively under Wayland
/// without requiring special command-line flags.
pub fn apply_wayland_env(cmd: &mut Command, socket_name: &str) {
    cmd.env("WAYLAND_DISPLAY", socket_name);
    cmd.env("XDG_SESSION_TYPE", "wayland");
    cmd.env("XDG_CURRENT_DESKTOP", "veil");
    cmd.env("XDG_SESSION_DESKTOP", "veil");
    cmd.env("ELECTRON_OZONE_PLATFORM_HINT", "wayland");
    cmd.env("OZONE_PLATFORM", "wayland");
    cmd.env("MOZ_ENABLE_WAYLAND", "1");
    cmd.env("QT_QPA_PLATFORM", "wayland");
    cmd.env("GDK_BACKEND", "wayland");
    cmd.env("SDL_VIDEODRIVER", "wayland");
}

fn spawn_command(socket_name: &str, exec: &str) {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(exec);
    apply_wayland_env(&mut cmd, socket_name);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            tracing::info!("spawned {exec:?}");
            std::thread::spawn(move || { let _ = child.wait(); });
        }
        Err(e) => tracing::error!("spawn {exec:?} failed: {e}"),
    }
}

/// Run a Combo-4 keybind action against the live layout, then re-tile and
/// re-focus so the client sees the result immediately.
fn dispatch_action(state: &mut State, action: veil_config::Action) {
    use veil_config::Action::*;
    match action {
        FocusLeft  => state.layout.focus(&state.layout_rects, crate::layout::Dir::Left),
        FocusRight => state.layout.focus(&state.layout_rects, crate::layout::Dir::Right),
        FocusUp    => state.layout.focus(&state.layout_rects, crate::layout::Dir::Up),
        FocusDown  => state.layout.focus(&state.layout_rects, crate::layout::Dir::Down),
        Swap => {
            // swap_next's indices are positions among LIVE toplevels; map them
            // back to real Vec indices in case a dead-but-unpruned entry sits
            // between live ones.
            let live_idx: Vec<usize> = state.toplevels.iter().enumerate()
                .filter(|(_, t)| t.alive())
                .map(|(i, _)| i)
                .collect();
            if let Some((a, b)) = state.layout.swap_next(live_idx.len()) {
                state.toplevels.swap(live_idx[a], live_idx[b]);
            }
        }
        Rotate => state.layout.rotate_split(),
        Close => {
            if let Some(tl) = state.toplevels.iter().filter(|t| t.alive()).nth(state.layout.focused) {
                tl.send_close();
            }
        }
        ResizeGrow   => state.layout.resize_grow(),
        ResizeShrink => state.layout.resize_shrink(),
        Launch(cmd)  => spawn_command(&state.socket_name, &cmd),
    }
    relayout(state);
    refocus_keyboard(state);
}

/// Per-keystroke handling while the launcher modal is open. Navigation
/// (Escape/Enter/Backspace/Up/Down) is xkb-keysym-coded; anything else that
/// decodes to a printable Unicode codepoint is appended to the query as
/// typed (shift-aware, NOT lowercased — unlike `keysym_char`, since command
/// text is case-sensitive).
fn handle_launcher_key(state: &mut State, mods: &ModifiersState, keysym: KeysymHandle<'_>) {
    let sym = keysym.modified_sym().raw();

    // <mod_key>+D closes the launcher too — same chord opens and closes it.
    let mod_held = match state.keybinds.mod_key {
        veil_config::ModKey::Super => mods.logo,
        veil_config::ModKey::Ctrl  => mods.ctrl,
        veil_config::ModKey::Alt   => mods.alt,
        veil_config::ModKey::Shift => mods.shift,
    };
    if mod_held && matches!(sym, keysyms::KEY_d | keysyms::KEY_D) {
        state.launcher = None;
        state.dirty = true;
        return;
    }

    if sym == keysyms::KEY_Escape {
        state.launcher = None;
        state.dirty = true;
        return;
    }
    if sym == keysyms::KEY_Return || sym == keysyms::KEY_KP_Enter {
        launch_selected(state);
        return;
    }
    if sym == keysyms::KEY_BackSpace {
        if let Some(l) = state.launcher.as_mut() {
            l.query.pop();
            l.selected = 0;
            state.dirty = true;
        }
        return;
    }
    if sym == keysyms::KEY_Up {
        if let Some(l) = state.launcher.as_mut() {
            l.selected = l.selected.saturating_sub(1);
            state.dirty = true;
        }
        return;
    }
    if sym == keysyms::KEY_Down {
        if let Some(l) = state.launcher.as_mut() {
            let count = l.matches().len();
            if count > 0 { l.selected = (l.selected + 1).min(count - 1); }
            state.dirty = true;
        }
        return;
    }

    let cp = smithay::input::keyboard::xkb::keysym_to_utf32(keysym.modified_sym());
    if let Some(c) = char::from_u32(cp) {
        if !c.is_control() {
            if let Some(l) = state.launcher.as_mut() {
                l.query.push(c);
                l.selected = 0;
                state.dirty = true;
            }
        }
    }
}

/// Run whatever's selected: the highlighted `.desktop` match if there is
/// one, else the raw typed query as a shell command. Fire-and-forget — the
/// child is reaped on its own thread but never ties into veil's own
/// lifetime (that coupling, on the ORIGINAL `run` spawn, is what stranded
/// abyss when the last window closed).
fn launch_selected(state: &mut State) {
    let Some(launcher) = state.launcher.take() else { return };
    state.dirty = true;

    let matches = launcher.matches();
    let exec = if !matches.is_empty() {
        matches[launcher.selected.min(matches.len() - 1)].exec.clone()
    } else if !launcher.query.trim().is_empty() {
        launcher.query.clone()
    } else {
        return;
    };

    spawn_command(&state.socket_name, &exec);
}

/// Recompute the dwindle tiling and push each toplevel its new size. Call
/// whenever the live window set or the output size changes. Also marks the
/// focused window Activated (others deactivated) so clients render focus state.
fn relayout(state: &mut State) {
    let n = state.toplevels.iter().filter(|t| t.alive()).count();
    if state.layout.focused >= n {
        state.layout.focused = n.saturating_sub(1);
    }
    let focused = state.layout.focused;
    let rects = state.layout.rects(n, state.output_w, state.output_h);

    for (i, tl) in state.toplevels.iter().filter(|t| t.alive()).enumerate() {
        let r = rects[i];
        tl.with_pending_state(|s| {
            s.size = Some((r.w as i32, r.h as i32).into());
            if i == focused {
                s.states.set(xdg_toplevel::State::Activated);
            } else {
                s.states.unset(xdg_toplevel::State::Activated);
            }
        });
        tl.send_configure();
    }

    state.layout_rects = rects;
    state.dirty = true;
}

/// Point the keyboard at whichever live toplevel is currently focused (or
/// nothing, if there are no windows left).
fn refocus_keyboard(state: &mut State) {
    let target = state.toplevels.iter()
        .filter(|t| t.alive())
        .nth(state.layout.focused)
        .map(|t| t.wl_surface().clone());
    let serial = state.next_serial();
    let kb = state.keyboard.clone();
    kb.set_focus(state, target, serial);
}

/// Composite all live toplevels + their popups + the cursor into a single
/// RGBA frame and ship it. Called from the periodic tick when `dirty`.
fn composite_and_send(state: &mut State, composite_interval: Duration) {
    if !state.dirty { return; }
    let now = Instant::now();
    if let Some(t) = state.last_composite {
        if now.duration_since(t) < composite_interval { return; }
    }
    state.last_composite = Some(now);
    state.dirty = false;
    tracing::info!("compositing frame (buffers={})", state.surface_buffers.len());

    let w = state.output_w;
    let h = state.output_h;
    let needed = (w as usize) * (h as usize) * 4;
    if state.composite_buf.len() != needed {
        state.composite_buf.resize(needed, 0);
    }

    // Fast background fill: write the 4-byte RGBA color as a u32 across the
    // whole buffer in one pass — avoids the overhead of chunks_exact_mut(4).
    let bg = state.background;
    let bg_u32 = u32::from_ne_bytes(bg);
    // SAFETY: composite_buf is aligned to at least 1; u32 requires 4-byte
    // alignment and Vec<u8> guarantees that its allocation is at least
    // max_align_t-aligned (≥ 8 bytes on all supported platforms), so the
    // cast is safe for the in-bounds slice. bytemuck would be cleaner but
    // this avoids an extra dep; the debug assert catches any future breakage.
    debug_assert!(state.composite_buf.as_ptr().align_offset(4) == 0);
    {
        let (pre, u32s, post) = unsafe { state.composite_buf.align_to_mut::<u32>() };
        for b in pre.chunks_exact_mut(4) { b.copy_from_slice(&bg); }
        u32s.fill(bg_u32);
        for b in post.chunks_exact_mut(4) { b.copy_from_slice(&bg); }
    }

    let show_help = state.show_help;
    let launcher_present = state.launcher.is_some();

    let back = &mut state.composite_buf;

    // Toplevels (root buffer + subsurfaces) then their popups, each at its
    // tiled rect origin. Popups are positioned relative to their toplevel.
    let toplevels: Vec<WlSurface> = state.toplevels.iter()
        .filter(|t| t.alive())
        .map(|t| t.wl_surface().clone())
        .collect();
    for (i, surf) in toplevels.iter().enumerate() {
        let r = state.layout_rects.get(i).copied()
            .unwrap_or(Rect { x: 0, y: 0, w, h });
        blit_subtree(back, w, h, &state.surface_buffers, surf, (r.x, r.y));
        for (popup, off) in PopupManager::popups_for_surface(surf) {
            let ps = popup.wl_surface().clone();
            blit_subtree(back, w, h, &state.surface_buffers, &ps, (r.x + off.x, r.y + off.y));
        }
    }

    // Cursor on top.
    match &state.cursor_status {
        CursorImageStatus::Surface(cs) => {
            let hotspot = with_states(cs, |s| {
                s.data_map.get::<std::sync::Mutex<CursorImageAttributes>>()
                    .map(|m| m.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            let cx = state.pointer_pos.0 as i32 - hotspot.x;
            let cy = state.pointer_pos.1 as i32 - hotspot.y;
            blit_subtree(back, w, h, &state.surface_buffers, cs, (cx, cy));
        }
        CursorImageStatus::Named(_) => {
            // Client wants a themed cursor (default arrow etc) — we don't
            // load themes. Draw a tiny built-in arrow so the user can see
            // where their pointer is.
            draw_fallback_cursor(
                back, w, h,
                state.pointer_pos.0 as i32,
                state.pointer_pos.1 as i32,
            );
        }
        CursorImageStatus::Hidden => {}
    }

    // Only clone keybinds/launcher when the overlays are actually on screen.
    if show_help {
        let keybinds = state.keybinds.clone();
        draw_help_overlay(&keybinds, back, w, h);
    }
    if launcher_present {
        if let Some(ref l) = state.launcher {
            let mod_key = state.keybinds.mod_key;
            // Borrow checker: we need to call draw_launcher_overlay with `back`
            // already mutably borrowed. Clone the launcher (it's tiny) rather
            // than fighting the borrow checker with unsafe aliasing.
            let l = l.clone();
            draw_launcher_overlay(&l, mod_key, back, w, h);
        }
    }

    state.frame_serial = state.frame_serial.wrapping_add(1);

    // Zero-copy dispatch: swap composite_buf with a fresh Vec so the
    // compositor can immediately reuse the allocation for the next frame,
    // while the render thread independently holds the Arc-wrapped old buffer.
    let mut outgoing = Vec::new();
    std::mem::swap(&mut state.composite_buf, &mut outgoing);
    let _ = state.frame_tx.send(crate::sink::Frame {
        rgba: Arc::new(outgoing), width: w, height: h, serial: state.frame_serial,
    });
    // Put a correctly-sized buffer back for next frame. If the channel
    // consumer drained quickly the Vec may come back via Arc::try_unwrap on
    // the next composite; for now just allocate a new one (still cheaper than
    // the old .clone() — only one allocation per frame vs a full memcpy).
    state.composite_buf = Vec::with_capacity(needed);

    // Fire frame callbacks now that we've consumed and displayed this frame.
    // Chromium uses these as vsync: it won't submit the next buffer until
    // it receives one. Firing here (after composite) caps Chromium's render
    // rate to our composite_interval instead of the 8ms tick rate.
    let time = state.start_time.elapsed().as_millis() as u32;
    let surfaces: Vec<WlSurface> = state.toplevels.iter()
        .filter(|t| t.alive())
        .map(|t| t.wl_surface().clone())
        .collect();
    for s in &surfaces {
        send_frame_callbacks(s, time);
        for (popup, _) in PopupManager::popups_for_surface(s) {
            send_frame_callbacks(popup.wl_surface(), time);
        }
    }
}

/// `<mod_key>+/` help overlay: dumps the parsed `keybinds` config as an
/// on-screen box, stamped directly into the composited RGBA frame with the
/// built-in 5x7 font ([`crate::font5x7`]) — veil-host has no other text
/// rendering.
fn draw_help_overlay(keybinds: &veil_config::Keybinds, back: &mut [u8], w: u32, h: u32) {
    use crate::font5x7::{draw_text, fill_rect, GLYPH_H, GLYPH_W};

    let scale = 2u32;
    let advance = (GLYPH_W + 1) * scale;
    let line_h = (GLYPH_H + 3) * scale;
    let pad = 12i32;

    let mod_label = keybinds.mod_key.label().to_ascii_uppercase();
    let mut lines: Vec<String> = vec!["KEYBINDS".to_string(), String::new()];
    for (key, action) in &keybinds.binds {
        lines.push(format!("{mod_label}+{}  {}", key.to_ascii_uppercase(), action.label().to_ascii_uppercase()));
    }
    lines.push(String::new());
    lines.push(format!("{mod_label}+/  TOGGLE THIS MENU"));
    lines.push(format!("{mod_label}+D  APP LAUNCHER"));
    lines.push("SHIFT+ALT+E  QUIT (GRACEFUL)".to_string());

    let text_cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u32;
    let box_w = text_cols * advance + pad as u32 * 2;
    let box_h = lines.len() as u32 * line_h + pad as u32 * 2;
    let x0 = ((w as i32 - box_w as i32) / 2).max(0);
    let y0 = ((h as i32 - box_h as i32) / 2).max(0);

    let border = 2i32;
    fill_rect(back, w, h, x0 - border, y0 - border, box_w + (border as u32 * 2), box_h + (border as u32 * 2), [199, 146, 234, 255]);
    fill_rect(back, w, h, x0, y0, box_w, box_h, [10, 0, 16, 245]); // dark purple background
    for (i, line) in lines.iter().enumerate() {
        let ty = y0 + pad + i as i32 * line_h as i32;
        let color = if i == 0 { [255, 215, 0, 255] } else { [199, 146, 234, 255] };
        draw_text(back, w, h, x0 + pad, ty, scale, line, color);
    }
}

/// `<mod_key>+D` launcher modal: query box + top matches, same font/box
/// style as the help overlay. Selected row gets a highlight bar.
fn draw_launcher_overlay(launcher: &Launcher, mod_key: veil_config::ModKey, back: &mut [u8], w: u32, h: u32) {
    use crate::font5x7::{draw_text, fill_rect, GLYPH_H, GLYPH_W};

    const MAX_ROWS: usize = 8;
    let scale = 2u32;
    let advance = (GLYPH_W + 1) * scale;
    let line_h = (GLYPH_H + 3) * scale;
    let pad = 12i32;

    let mod_label = mod_key.label().to_ascii_uppercase();
    let matches = launcher.matches();
    let selected = launcher.selected.min(matches.len().saturating_sub(1));

    let mut lines: Vec<String> = vec![
        format!("LAUNCHER  ({mod_label}+D CLOSE, ENTER RUN, ESC CANCEL)"),
        format!("> {}_", launcher.query),
        String::new(),
    ];
    let header_rows = lines.len();
    if matches.is_empty() {
        lines.push(if launcher.query.trim().is_empty() {
            "NO APPS FOUND — TYPE A COMMAND".to_string()
        } else {
            "NO MATCH — ENTER RUNS AS SHELL COMMAND".to_string()
        });
    } else {
        for m in matches.iter().take(MAX_ROWS) {
            lines.push(m.name.clone());
        }
        if matches.len() > MAX_ROWS {
            lines.push(format!("... {} MORE", matches.len() - MAX_ROWS));
        }
    }

    let text_cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(40) as u32;
    let box_w = text_cols * advance + pad as u32 * 2;
    let box_h = lines.len() as u32 * line_h + pad as u32 * 2;
    let x0 = ((w as i32 - box_w as i32) / 2).max(0);
    let y0 = ((h as i32 - box_h as i32) / 2).max(0);

    let border = 2i32;
    fill_rect(back, w, h, x0 - border, y0 - border, box_w + (border as u32 * 2), box_h + (border as u32 * 2), [199, 146, 234, 255]);
    fill_rect(back, w, h, x0, y0, box_w, box_h, [10, 0, 16, 245]);

    // Highlight bar behind the selected match row, drawn before the text.
    if !matches.is_empty() {
        let row = header_rows + selected;
        let ry = y0 + pad + row as i32 * line_h as i32 - 2;
        fill_rect(back, w, h, x0 + 2, ry, box_w - 4, line_h, [65, 35, 85, 255]);
    }

    for (i, line) in lines.iter().enumerate() {
        let ty = y0 + pad + i as i32 * line_h as i32;
        let color = if i == 0 {
            [255, 215, 0, 255] // Gold header
        } else if i == 1 {
            [128, 222, 234, 255] // Cyan query text
        } else {
            [199, 146, 234, 255]
        };
        draw_text(back, w, h, x0 + pad, ty, scale, line, color);
    }
}

fn send_frame_callbacks(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surf, states, &()| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            for cb in guard.current().frame_callbacks.drain(..) {
                cb.done(time);
            }
        },
        |_, _, &()| true,
    );
}

/// Which live toplevel's tiled rect contains `(x, y)`, if any. Returns a
/// live-order index — matches `layout_rects`/`layout.focused`, NOT a raw
/// `toplevels` Vec index (see the `live_idx` mapping in `dispatch_action`
/// and the click-to-focus handling in `apply_input`).
fn toplevel_at(state: &State, x: f64, y: f64) -> Option<usize> {
    let xi = x as i32;
    let yi = y as i32;
    state.layout_rects.iter().position(|r| {
        xi >= r.x && yi >= r.y && xi < r.x + r.w as i32 && yi < r.y + r.h as i32
    })
}

/// Walk all toplevels' popups (newest first) then the toplevel root.
/// Return the first surface whose cached buffer rect contains (x, y),
/// along with the cursor's surface-local coordinates.
fn pick_focus(state: &State, x: f64, y: f64) -> Option<(WlSurface, smithay::utils::Point<f64, smithay::utils::Logical>)> {
    let xi = x as i32;
    let yi = y as i32;

    // Live toplevels paired with their tiled rect, topmost (last) first.
    let live: Vec<WlSurface> = state.toplevels.iter()
        .filter(|t| t.alive())
        .map(|t| t.wl_surface().clone())
        .collect();
    for (i, root) in live.iter().enumerate().rev() {
        let r = state.layout_rects.get(i).copied()
            .unwrap_or(Rect { x: 0, y: 0, w: state.output_w, h: state.output_h });

        // Popups (per-toplevel) — last-added wins on overlap. Positioned at
        // the toplevel rect origin + the popup's toplevel-relative offset.
        let popups: Vec<_> = PopupManager::popups_for_surface(root).collect();
        for (popup, off) in popups.iter().rev() {
            let ps = popup.wl_surface();
            if let Some(buf) = state.surface_buffers.get(&ps.id()) {
                let bx = r.x + off.x;
                let by = r.y + off.y;
                if xi >= bx && yi >= by
                    && xi < bx + buf.w as i32 && yi < by + buf.h as i32
                {
                    // loc = surface origin in compositor space; Smithay
                    // computes surface-local as event.location - loc.
                    return Some((ps.clone(), (bx as f64, by as f64).into()));
                }
            }
        }

        // Toplevel root sits at its rect origin.
        if let Some(buf) = state.surface_buffers.get(&root.id()) {
            if xi >= r.x && yi >= r.y
                && xi < r.x + buf.w as i32 && yi < r.y + buf.h as i32
            {
                return Some((root.clone(), (r.x as f64, r.y as f64).into()));
            }
        }
    }
    None
}

fn apply_input(state: &mut State, cmd: InputCmd) {
    use smithay::backend::input::{ButtonState as BState, KeyState};
    let serial = state.next_serial();
    let time   = state.start_time.elapsed().as_millis() as u32;

    match cmd {
        InputCmd::Key { keycode, pressed, .. } => {
            let ks = if pressed { KeyState::Pressed } else { KeyState::Released };
            // xkbcommon Keycode is an X11 keycode = evdev + 8.
            let kb = state.keyboard.clone();
            kb.input::<(), _>(
                state, (keycode + 8).into(), ks, serial, time,
                |st, mods: &ModifiersState, keysym: KeysymHandle<'_>| {
                    // Launcher modal: swallow ALL key input while it's open
                    // (typed query text, arrow-key selection, Enter/Escape),
                    // so the hosted client (if any) never sees it and text
                    // typed into the query box can't leak through as
                    // keystrokes to whatever's focused underneath.
                    if st.launcher.is_some() {
                        if pressed {
                            handle_launcher_key(st, mods, keysym);
                        }
                        return FilterResult::Intercept(());
                    }

                    if !pressed {
                        return FilterResult::Forward;
                    }
                    let Some(ch) = keysym_char(keysym) else {
                        return FilterResult::Forward;
                    };

                    // Shift+Alt+E: graceful quit. Fixed, not `keybinds.mod_key`
                    // scaled — same reasoning as Ctrl+C always being Ctrl
                    // regardless of mod_key: your escape hatch shouldn't move
                    // just because you remapped the everyday chord.
                    if mods.shift && mods.alt && ch == 'e' {
                        st.stop.store(true, Ordering::Relaxed);
                        return FilterResult::Intercept(());
                    }

                    let mod_held = match st.keybinds.mod_key {
                        veil_config::ModKey::Super => mods.logo,
                        veil_config::ModKey::Ctrl  => mods.ctrl,
                        veil_config::ModKey::Alt   => mods.alt,
                        veil_config::ModKey::Shift => mods.shift,
                    };
                    if mod_held {
                        // Help overlay and the launcher both ride whatever
                        // mod_key is configured rather than a hardcoded
                        // Super — neither is a `keybinds` config entry, but
                        // NOT literally Super: crossterm terminal mode can
                        // never see the Logo key (the WM eats it before it
                        // reaches the terminal), so a hardcoded Super+key
                        // would be dead in terminal mode. Following mod_key
                        // means it works in whatever mode's actually in use.
                        if ch == '/' {
                            st.show_help = !st.show_help;
                            st.dirty = true;
                            return FilterResult::Intercept(());
                        }
                        if ch == 'd' {
                            st.launcher = Some(Launcher::new());
                            st.dirty = true;
                            return FilterResult::Intercept(());
                        }
                        if let Some(action) = st.keybinds.action_for(ch) {
                            dispatch_action(st, action);
                            return FilterResult::Intercept(());
                        }
                    }
                    FilterResult::Forward
                },
            );
        }

        InputCmd::PointerMotionAbs { x, y, width, height } => {
            // Caller works in (width × height) pixel space; rescale to our output.
            let nx = if width  > 0 { x as f64 * state.output_w as f64 / width  as f64 } else { x as f64 };
            let ny = if height > 0 { y as f64 * state.output_h as f64 / height as f64 } else { y as f64 };
            state.pointer_pos = (nx, ny);
            state.dirty = true;

            // Resolve focus: prefer the topmost popup under the cursor,
            // else the toplevel. Surface-local coords are (global - origin).
            let focus = pick_focus(state, nx, ny);
            if focus.is_none() {
                tracing::debug!("motion ({:.0},{:.0}) → no focus (toplevels={}, buffers={})",
                    nx, ny, state.toplevels.len(), state.surface_buffers.len());
            }
            let ptr = state.pointer.clone();
            ptr.motion(state, focus, &MotionEvent {
                location: (nx, ny).into(), serial, time,
            });
            ptr.frame(state);
        }

        InputCmd::PointerButton { button, pressed } => {
            let bs = if pressed { BState::Pressed } else { BState::Released };

            // Left-click on a tile makes it the focused one — just follows
            // click focus, no position rearranging (that's what Alt+S is
            // for). Keeps `layout.focused` in sync with clicks so keyboard
            // nav/close (Alt+H/J/K/L/Q) act on whatever you last clicked,
            // not whatever keyboard nav last visited. A click inside the
            // already-focused tile hits `idx == layout.focused` and no-ops,
            // so ordinary clicks/typing inside an app are unaffected.
            const BTN_LEFT: u32 = 0x110;
            if pressed && button == BTN_LEFT {
                if let Some(idx) = toplevel_at(state, state.pointer_pos.0, state.pointer_pos.1) {
                    if idx != state.layout.focused {
                        state.layout.focused = idx;
                        relayout(state);
                        refocus_keyboard(state);
                    }
                }
            }

            let focus = state.pointer.current_focus();
            tracing::info!(
                "button 0x{:x} pressed={} pos=({:.0},{:.0}) focus={:?}",
                button, pressed, state.pointer_pos.0, state.pointer_pos.1,
                focus.as_ref().map(|s| s.id()),
            );
            let ptr = state.pointer.clone();
            ptr.button(state, &ButtonEvent { button, state: bs, serial, time });
            ptr.frame(state);
        }

        InputCmd::Resize { width, height } => {
            // Rescale existing pointer position into the new pixel space so the
            // cursor doesn't jump on resize.
            if state.output_w > 0 && state.output_h > 0 {
                state.pointer_pos.0 = state.pointer_pos.0 * width  as f64 / state.output_w as f64;
                state.pointer_pos.1 = state.pointer_pos.1 * height as f64 / state.output_h as f64;
            }
            state.output_w = width;
            state.output_h = height;
            let mode = OutputMode {
                size: (width as i32, height as i32).into(),
                refresh: 60_000,
            };
            state.output.change_current_state(Some(mode), None, None, None);
            // Retile everyone into the new output extent.
            relayout(state);
        }

        InputCmd::Scroll { v120 } => {
            // v120 = 120 per notch (Windows convention). Convert to a 15px-per-notch
            // continuous value as well; clients pick whichever they understand.
            let notches = v120 as f64 / 120.0;
            let mut f = AxisFrame::new(time)
                .source(smithay::backend::input::AxisSource::Wheel);
            f.axis = (0.0, notches * 15.0);
            f.v120 = Some((0, v120));
            let ptr = state.pointer.clone();
            ptr.axis(state, f);
            ptr.frame(state);
        }
    }
}

// ─── Run loop ─────────────────────────────────────────────────────────────────

/// Bundle of state passed through calloop callbacks.
pub struct LoopData {
    pub state:   State,
    pub display: Display<State>,
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    socket_name: &str,
    width:  u32,
    height: u32,
    fps:    u32,
    spawn:  Option<Vec<String>>,
    wayland_debug: bool,
    frame_tx: mpsc::Sender<Frame>,
    input_rx: mpsc::Receiver<InputCmd>,
    stop: Arc<AtomicBool>,
    keybinds: veil_config::Keybinds,
    background: [u8; 3],
) -> io::Result<()> {
    let composite_interval = Duration::from_millis(1000 / fps.max(1) as u64);
    let display: Display<State> = Display::new()
        .map_err(|e| io::Error::other(format!("display: {e}")))?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<State>(&dh);
    let shm_state        = ShmState::new::<State>(&dh, vec![]);
    let xdg_shell_state  = XdgShellState::new::<State>(&dh);
    let xdg_activation   = XdgActivationState::new::<State>(&dh);
    let output_manager   = OutputManagerState::new_with_xdg_output::<State>(&dh);
    // Bring up the GPU dmabuf importer (EGL/GLES on the render node). None if
    // there's no GPU / EGL — veil then stays CPU-only + linear-only. Created
    // before the dmabuf global so feedback can advertise its formats.
    let gpu = GpuImporter::new();

    // v4 dmabuf with default feedback: advertise the render device + the formats
    // we can import (linear always; tiled too when the GPU importer is up).
    // Feedback-aware clients allocate accordingly; we detile tiled buffers.
    let mut dmabuf_state  = DmabufState::new();
    let dmabuf_feedback   = build_dmabuf_feedback(&gpu);
    let _dmabuf_global    = dmabuf_state.create_global_with_default_feedback::<State>(&dh, &dmabuf_feedback);
    let _data_device          = DataDeviceState::new::<State>(&dh);
    let _xdg_decoration       = XdgDecorationState::new::<State>(&dh);
    let _viewporter           = ViewporterState::new::<State>(&dh);
    let _fractional           = FractionalScaleManagerState::new::<State>(&dh);
    // clk_id 1 = CLOCK_MONOTONIC.
    let _presentation         = PresentationState::new::<State>(&dh, 1);
    let _text_input           = TextInputManagerState::new::<State>(&dh);
    let _primary_sel          = PrimarySelectionState::new::<State>(&dh);
    let _cursor_shape         = CursorShapeManagerState::new::<State>(&dh);
    let _pointer_constraints  = PointerConstraintsState::new::<State>(&dh);
    let _relative_pointer     = RelativePointerManagerState::new::<State>(&dh);
    let _idle_inhibit         = IdleInhibitManagerState::new::<State>(&dh);
    let _kb_inhibit           = KeyboardShortcutsInhibitState::new::<State>(&dh);
    let _tablet               = TabletManagerState::new::<State>(&dh);
    let mut seat_state   = SeatState::<State>::new();
    let mut seat         = seat_state.new_wl_seat(&dh, "veil-seat");
    let keyboard = seat
        .add_keyboard(XkbConfig::default(), 200, 16)
        .map_err(|e| io::Error::other(format!("keyboard: {e}")))?;
    let pointer = seat.add_pointer();

    let output = Output::new("veil-host-0".into(), PhysicalProperties {
        size: (0, 0).into(),
        subpixel: Subpixel::Unknown,
        make:  "veil".into(),
        model: "host".into(),
    });
    let mode = OutputMode {
        size:    (width as i32, height as i32).into(),
        refresh: 60_000,
    };
    output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
    output.set_preferred(mode);
    let _output_global = output.create_global::<State>(&dh);

    // Clipboard: poll the host compositor for clipboard changes every second.
    // Only offers host content when the hosted client has no active selection.
    let (clipboard_tx, clipboard_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::Read;
        use wl_clipboard_rs::paste::{get_contents, ClipboardType, MimeType, Seat as PasteSeat};
        let mut last = String::new();
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            if let Ok((mut reader, _)) = get_contents(ClipboardType::Regular, PasteSeat::Unspecified, MimeType::Text) {
                let mut text = String::new();
                if reader.read_to_string(&mut text).is_ok() && !text.is_empty() && text != last {
                    last = text.clone();
                    if clipboard_tx.send(text).is_err() { break; }
                }
            }
        }
    });

    let state = State {
        compositor_state, xdg_shell_state, shm_state, seat_state,
        xdg_activation, output_manager,
        dmabuf_state, _dmabuf_global,
        gpu,
        _data_device,
        _xdg_decoration, _viewporter, _fractional, _presentation,
        _text_input, _primary_sel, _cursor_shape,
        _pointer_constraints, _relative_pointer, _idle_inhibit, _kb_inhibit, _tablet,
        seat, keyboard, pointer,
        output,
        output_w: width, output_h: height,
        pointer_pos: (0.0, 0.0),
        toplevels: Vec::new(),
        layout: Layout::default(),
        layout_rects: Vec::new(),
        keybinds,
        show_help: false,
        background: [background[0], background[1], background[2], 255],
        // No anchor client to spawn into (`veil-host start`) → open straight
        // to the launcher instead of an empty screen with no hint of what to press.
        launcher: if spawn.is_none() { Some(Launcher::new()) } else { None },
        socket_name: socket_name.to_string(),
        popups:           PopupManager::default(),
        surface_buffers:  HashMap::new(),
        cursor_status:    CursorImageStatus::default_named(),
        dirty:            false,
        last_composite:   None,
        frame_tx,
        serial_counter: 0,
        frame_serial:   0,
        running:        true,
        stop:           stop.clone(),
        start_time:     Instant::now(),
        display_handle:       dh.clone(),
        host_clipboard:       None,
        clipboard_rx,
        pending_copy_out:     false,
        client_has_selection: false,
        composite_buf:        Vec::new(),
    };

    let mut data = LoopData { state, display };

    // ── Calloop event loop ────────────────────────────────────────────────────
    let mut event_loop: EventLoop<'static, LoopData> = EventLoop::try_new()
        .map_err(|e| io::Error::other(format!("event_loop: {e}")))?;
    let handle = event_loop.handle();
    // Grabbed so the stop checks below can actually end `event_loop.run()` —
    // `calloop::EventLoop::run` only returns once this is told to stop; the
    // `stop`/`data.state.running` checks alone don't do that on their own,
    // they just decide whether to call it.
    let loop_signal = event_loop.get_signal();

    // 1. Wayland listening socket → accept clients.
    let listener_source = ListeningSocketSource::with_name(socket_name)
        .map_err(|e| io::Error::other(format!("bind {socket_name}: {e}")))?;
    let bound_name = listener_source.socket_name().to_os_string();
    handle.insert_source(listener_source, |stream, _, data| {
        let _ = data.display.handle()
            .insert_client(stream, Arc::new(ClientState::default()));
    }).map_err(|e| io::Error::other(format!("insert listener: {e}")))?;
    tracing::info!("listening on WAYLAND_DISPLAY={:?}", bound_name);

    // 2. Wayland display fd → dispatch protocol messages.
    let wl_fd = data.display.backend().poll_fd().as_raw_fd();
    let wl_src = Generic::new(
        unsafe { FdWrapper::new(wl_fd) }, Interest::READ, CMode::Level,
    );
    handle.insert_source(wl_src, |_, _, data| {
        data.display.dispatch_clients(&mut data.state)
            .map_err(|e| { tracing::error!("dispatch: {e}"); e })?;
        Ok(PostAction::Continue)
    }).map_err(|e| io::Error::other(format!("insert display: {e}")))?;

    // 3. XWayland — spawn it and listen for the Ready event so we can set
    //    DISPLAY for any X11 children we later spawn. Failure is non-fatal:
    //    if Xwayland isn't installed, we just lose X11 compat.
    let xwayland_display: Arc<std::sync::Mutex<Option<u32>>> = Arc::new(std::sync::Mutex::new(None));
    match XWayland::spawn(
        &data.display.handle(),
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_user_data| {},
    ) {
        Ok((xwayland, _x_client)) => {
            let xd = xwayland_display.clone();
            handle.insert_source(xwayland, move |event, _, _data| {
                match event {
                    XWaylandEvent::Ready { x11_socket: _, display_number } => {
                        tracing::info!("XWayland ready on DISPLAY=:{display_number}");
                        *xd.lock().unwrap() = Some(display_number);
                        std::env::set_var("DISPLAY", format!(":{display_number}"));
                    }
                    XWaylandEvent::Error => {
                        tracing::error!("XWayland startup failed");
                    }
                }
            }).map_err(|e| io::Error::other(format!("insert xwayland: {e}")))?;
        }
        Err(e) => tracing::warn!("XWayland unavailable: {e} — X11 apps will not work"),
    }

    // 4. Periodic timer: drain input channel, send frame callbacks,
    //    flush clients, check stop flag. 8 ms tick = ~120 Hz ceiling.
    let socket_name_owned = socket_name.to_string();
    let stop_t = stop.clone();
    let loop_signal_t = loop_signal.clone();
    let tick = Timer::immediate();
    handle.insert_source(tick, move |_, _, data| {
        // Prune toplevels that the client has destroyed. If any closed, retile
        // the survivors and move keyboard focus onto one of them.
        let before = data.state.toplevels.len();
        data.state.toplevels.retain(|t| t.alive());
        if data.state.toplevels.len() != before {
            relayout(&mut data.state);
            refocus_keyboard(&mut data.state);
        }

        // Drain input cmds.
        while let Ok(cmd) = input_rx.try_recv() {
            apply_input(&mut data.state, cmd);
        }

        // Copy-out: hosted client set clipboard → push to host compositor.
        // Deferred one tick because Smithay updates seat_data after new_selection returns.
        if data.state.pending_copy_out {
            data.state.pending_copy_out = false;
            let seat = data.state.seat.clone();
            for &mime in &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"] {
                let mut fds = [-1i32; 2];
                let ok = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) == 0 };
                if !ok { break; }
                let read_fd  = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
                if request_data_device_client_selection::<State>(&seat, mime.to_string(), write_fd).is_ok() {
                    std::thread::spawn(move || {
                        use std::io::Read;
                        let mut f: std::fs::File = read_fd.into();
                        let mut buf = Vec::new();
                        if f.read_to_end(&mut buf).is_err() || buf.is_empty() { return; }
                        let _ = wl_clipboard_rs::copy::Options::new().copy(
                            wl_clipboard_rs::copy::Source::Bytes(buf.into_boxed_slice()),
                            wl_clipboard_rs::copy::MimeType::Text,
                        );
                    });
                    break;
                }
                // read_fd closes here if request failed; write_fd was consumed by the call
            }
        }

        // Paste-in: host clipboard changed → offer it to the hosted client.
        // Only when the client has no active selection of its own.
        if !data.state.client_has_selection {
            let mut latest: Option<String> = None;
            while let Ok(text) = data.state.clipboard_rx.try_recv() { latest = Some(text); }
            if let Some(text) = latest {
                if data.state.host_clipboard.as_deref() != Some(&text) {
                    data.state.host_clipboard = Some(text);
                    let dh = data.display.handle();
                    let seat = data.state.seat.clone();
                    set_data_device_selection::<State>(
                        &dh, &seat,
                        vec!["text/plain;charset=utf-8".into(), "text/plain".into()],
                        (),
                    );
                }
            }
        }

        // Composite all dirty surfaces into one RGBA frame and ship it.
        // Frame callbacks are fired inside composite_and_send after the frame is sent.
        composite_and_send(&mut data.state, composite_interval);

        // Flush outgoing wayland messages.
        let _ = data.display.flush_clients();

        if stop_t.load(Ordering::Relaxed) || !data.state.running {
            loop_signal_t.stop();
            TimeoutAction::Drop
        } else {
            TimeoutAction::ToDuration(Duration::from_millis(8))
        }
    }).map_err(|e| io::Error::other(format!("insert tick: {e}")))?;

    // 5. Spawn the hosted client after the socket is live.
    if let Some(argv) = spawn {
        if !argv.is_empty() {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]);
            apply_wayland_env(&mut cmd, &socket_name_owned);
            if wayland_debug {
                cmd.env("WAYLAND_DEBUG", "1");
            }
            match cmd.spawn() {
                Ok(mut child) => {
                    tracing::info!("spawned: {:?}", argv);
                    // Reap on exit but DON'T touch `stop` — this was the
                    // "abyss" bug: veil used to tear the WHOLE compositor
                    // down the instant this one (anchor) client exited, even
                    // with other windows still open (from the Alt+D launcher
                    // or `run -a`), which could stutter/freeze/crash instead
                    // of a clean exit. Made sense for the old one-shot `run
                    // firefox`-and-wait model; doesn't anymore now that Alt+D
                    // makes this a persistent session. Only HOME/Ctrl-C
                    // should ever stop veil now.
                    std::thread::spawn(move || {
                        match child.wait() {
                            Ok(s)  => tracing::info!("anchor client exited: {s} (veil keeps running)"),
                            Err(e) => tracing::error!("anchor client wait: {e}"),
                        }
                    });
                }
                Err(e) => tracing::error!("spawn {:?} failed: {e}", argv),
            }
        }
    }

    // ── Drive the loop ────────────────────────────────────────────────────────
    event_loop.run(Some(Duration::from_millis(16)), &mut data, |data| {
        // Post-dispatch hook: flush again to push anything generated during dispatch.
        let _ = data.display.flush_clients();
        if stop.load(Ordering::Relaxed) || !data.state.running {
            data.state.running = false;
            loop_signal.stop();
        }
    }).map_err(|e| io::Error::other(format!("run: {e}")))?;

    // Belt-and-suspenders: every intentional shutdown path (HOME, Ctrl-C)
    // already calls this directly, and crashes go through the panic
    // hook/signal handlers — but cover whatever exit route got us here too.
    // Idempotent; unlinking an already-gone socket is a harmless no-op.
    crate::vt::emergency_restore();

    Ok(())
}
