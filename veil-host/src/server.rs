//! Smithay-based nested compositor: socket, globals, dispatch loop.
//!
//! v1 scope: single client, single fullscreen toplevel, shm + dmabuf buffers,
//! keyboard + pointer + scroll, wl_output advertisement, xdg_activation stub.
//! dmabuf: single-plane linear ARGB/XRGB/ABGR/XBGR/RGBX/BGRX 8888 accepted via
//! mmap; tiled/non-linear and multi-plane buffers go through the GPU detile
//! path (`detile::GpuImporter`) when available, else fall back to shm.
//! Explicit sync (`linux-drm-syncobj-v1`) is honored on commit when the
//! render node supports it — see `DrmSyncobjHandler` below.

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
        protocol::{wl_buffer, wl_output, wl_seat, wl_shm, wl_surface::WlSurface},
        Client, Display, DisplayHandle, Resource,
    },
    utils::{DeviceFd, Logical, Point, Rectangle, Serial, Size, Transform},
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
        drm_syncobj::{
            supports_syncobj_eventfd, DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState,
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        XWayland, XWaylandClientData, XWaylandEvent, X11Surface, X11Wm, XwmHandler,
        xwm::{Reorder, ResizeEdge, X11Window, XwmId},
    },
    delegate_drm_syncobj, delegate_xwayland_shell,
    backend::drm::DrmDeviceFd,
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

/// Which protocol backs a managed toplevel — the payload half of `Window`,
/// split out so `Window` itself can carry the workspace tag alongside it
/// without every match arm below needing a third case.
pub enum WindowSurface {
    Xdg(ToplevelSurface),
    X11(X11Surface),
}

/// Unifies xdg-shell and X11 (XWayland) top-level windows so relayout,
/// composite, and input don't need to branch on protocol origin. Deliberately
/// NOT smithay::desktop::Window — that drags in Space assumptions this file
/// doesn't use (compositing here is manual, no Space).
pub struct Window {
    pub surface: WindowSurface,
    /// Which workspace this window is tiled on (0-indexed, 0..WORKSPACE_COUNT).
    /// Set at creation to whatever workspace was active at the time; moved
    /// only by future move-to-workspace keybinds (none bound yet).
    pub workspace: u8,
}

impl Window {
    pub fn new(surface: WindowSurface, workspace: u8) -> Self {
        Self { surface, workspace }
    }

    /// False once the client destroys it (xdg) or the X connection drops it.
    pub fn alive(&self) -> bool {
        match &self.surface {
            WindowSurface::Xdg(t) => t.alive(),
            WindowSurface::X11(x) => x.alive(),
        }
    }

    /// `None` for an X11 window created but not yet paired with its
    /// wl_surface (see XWaylandShellHandler::surface_associated) — callers
    /// must skip these the same way they already skip bufferless toplevels.
    pub fn wl_surface(&self) -> Option<WlSurface> {
        match &self.surface {
            WindowSurface::Xdg(t) => Some(t.wl_surface().clone()),
            WindowSurface::X11(x) => x.wl_surface(),
        }
    }

    /// Push a tiled rect + activation/fullscreen state. Xdg: async
    /// configure/ack via with_pending_state + send_configure. X11:
    /// configure() is synchronous and — unlike xdg — must carry absolute
    /// position, not just size. Safe to call before the window is
    /// paired/mapped; X11 configure doesn't need either.
    pub fn configure_size(&self, rect: Rect, activated: bool, fullscreen: bool) {
        match &self.surface {
            WindowSurface::Xdg(t) => {
                t.with_pending_state(|s| {
                    s.size = Some((rect.w as i32, rect.h as i32).into());
                    if activated {
                        s.states.set(xdg_toplevel::State::Activated);
                    } else {
                        s.states.unset(xdg_toplevel::State::Activated);
                    }
                    if fullscreen {
                        s.states.set(xdg_toplevel::State::Fullscreen);
                    } else {
                        s.states.unset(xdg_toplevel::State::Fullscreen);
                    }
                });
                t.send_configure();
            }
            WindowSurface::X11(surf) => {
                let _ = surf.configure(Rectangle::new(
                    Point::from((rect.x, rect.y)),
                    Size::from((rect.w as i32, rect.h as i32)),
                ));
                let _ = surf.set_activated(activated);
                // Updates _NET_WM_STATE so the client's own idea of its
                // fullscreen state matches ours — without this a well-behaved
                // client can get confused about whether its request "took"
                // and keep re-requesting.
                let _ = surf.set_fullscreen(fullscreen);
            }
        }
    }

    /// Ask the client to close. Xdg: xdg_toplevel.close event, client may
    /// ignore it. X11: sends WM_DELETE_WINDOW / kills the connection per
    /// smithay's close(), same "may be ignored" caveat applies.
    pub fn close_window(&self) {
        match &self.surface {
            WindowSurface::Xdg(t) => t.send_close(),
            WindowSurface::X11(x) => { let _ = x.close(); }
        }
    }
}

#[derive(Clone, Debug)]
pub struct OsdNotification {
    pub title: String,
    pub body: String,
    pub progress: Option<u8>,
    pub expires_at: Instant,
}

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
    /// Whether a DRM render node was found and imports work, per the startup
    /// probe — governs whether gpu_lazy() will attempt (re)creation at all.
    pub gpu_available:     bool,
    /// Whether a DRM render node was found and imports work, per the startup
    /// probe — governs whether gpu_lazy() will attempt (re)creation at all.
    /// `linux-drm-syncobj-v1` explicit sync. `None` when the render node's
    /// kernel/driver doesn't support `syncobj_eventfd` — commit() then skips
    /// the acquire-fence wait entirely and relies on implicit (kernel dma-buf)
    /// sync only, same behavior as before this existed.
    pub syncobj_state:     Option<DrmSyncobjState>,
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
    pub toplevels:         Vec<Window>,
    /// The one window currently fullscreen, if any — identified by
    /// wl_surface rather than a toplevels index, since indices shift on
    /// insert/remove but a WlSurface identity doesn't. relayout() looks up
    /// its current position each call rather than trusting a stored index.
    pub fullscreen:        Option<WlSurface>,
    /// Override-redirect X11 windows (menus, tooltips, Steam's own popups) —
    /// unmanaged, positioned absolutely by the client itself, never tiled.
    /// Painted last (topmost) every composite. See mapped_override_redirect_window.
    pub floating:          Vec<X11Surface>,
    /// X11 window manager for the XWayland connection — `None` until
    /// XWaylandEvent::Ready fires and start_wm succeeds (or if Xwayland
    /// isn't installed).
    pub xwm:               Option<X11Wm>,
    pub xwayland_shell_state: XWaylandShellState,
    /// Dwindle tiling state (focus + split orientation) for the ACTIVE
    /// workspace only. Swapped out to/from `workspace_layouts` on
    /// `SwitchWorkspace` — see `dispatch_action`. This split (rather than
    /// always indexing `workspace_layouts[active_workspace]` everywhere)
    /// keeps every existing `state.layout.*` call site working unchanged.
    pub layout:            Layout,
    /// Per-workspace tiling state for the 9 workspaces, INCLUDING the
    /// active one's slot (kept in sync on every switch, not read from
    /// directly while its workspace is active — `layout` above is the live
    /// copy then). Index 0 == workspace 1, matching `active_workspace`.
    pub workspace_layouts: [Layout; veil_config::WORKSPACE_COUNT as usize],
    /// 0-indexed active workspace (workspace 1 == 0). Super+1..9 switches
    /// this; new windows are tagged with whatever this is at creation time.
    pub active_workspace:  u8,
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
    /// Resolved color set for launcher/help/sidebar chrome — see
    /// `veil_config::Theme`. Swapped wholesale on `reload_config`.
    pub theme:              veil_config::Theme,
    pub bar:                veil_config::BarConfig,
    /// Screen-space rects of the bar's clickable app tiles, keyed to their
    /// `exec` command — rebuilt every time `draw_bar` runs (once per
    /// composite tick when the bar's on screen; cheap, a handful of rects).
    /// `PointerButton` consults this before falling through to normal
    /// click-to-focus/forwarding.
    pub bar_hitboxes:       Vec<(Rect, String)>,
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
    /// Union of changed regions since the last composite. `None` alongside
    /// `dirty == true` shouldn't happen in practice (every dirty=true site
    /// also sets this via `mark_dirty_rect`/`mark_dirty_full`) but composite
    /// treats `None` as "assume full frame" rather than panicking, so a
    /// future call site that forgets to set it degrades to the old
    /// always-full-redraw behavior instead of drawing nothing.
    pub damage:            Option<Rect>,
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
    /// The Arc we handed to frame_tx last time. Checked next composite via
    /// Arc::try_unwrap — if the render thread has already dropped its
    /// clone, we reclaim the allocation instead of allocating fresh.
    pub prev_frame:           Option<Arc<Vec<u8>>>,
    pub config_path:          Option<std::path::PathBuf>,
    pub config_mtime:         Option<std::time::SystemTime>,
    pub last_config_check:    Instant,
    pub composite_interval:   Duration,
    /// On-Screen Display (OSD) popup notification overlay.
    pub osd:                  Option<OsdNotification>,
}

/// Per-surface RGBA cache entry. We re-blit these every dirty tick.
pub struct SurfaceBuf {
    pub rgba: Vec<u8>,
    pub w:    u32,
    pub h:    u32,
}

impl State {
    pub fn show_osd(&mut self, title: impl Into<String>, body: impl Into<String>, progress: Option<u8>, duration: Duration) {
        self.osd = Some(OsdNotification {
            title: title.into(),
            body: body.into(),
            progress,
            expires_at: Instant::now() + duration,
        });
        mark_dirty_full(self);
    }
    fn next_serial(&mut self) -> Serial {
        self.serial_counter = self.serial_counter.wrapping_add(1);
        Serial::from(self.serial_counter)
    }

    /// The GPU importer's EGL/GLES context sits on the render node and costs
    /// real RSS (Mesa driver-side caches, shader compiler state) just by
    /// existing — so it's not built at startup. `gpu_available` (set once,
    /// from the startup probe used to build dmabuf feedback) tells us
    /// whether a render node was even found; if so, we stand the real
    /// context up here on the first commit that actually needs it, and keep
    /// it resident from then on. Sessions that never receive a non-linear
    /// dmabuf (plain shm clients, XWayland apps, linear-only GL apps) never
    /// pay for it at all.
    fn gpu_lazy(&mut self) -> Option<&mut GpuImporter> {
        if self.gpu.is_none() && self.gpu_available {
            self.gpu = GpuImporter::new();
            if self.gpu.is_none() {
                // Render node was there at boot but init failed now (e.g.
                // revoked) — stop retrying every commit.
                self.gpu_available = false;
            }
        }
        self.gpu.as_mut()
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
                // Explicit sync (linux-drm-syncobj-v1): if this commit carries
                // an acquire point (client set one via wp_linux_drm_syncobj_
                // surface_v1), block until the client's GPU signals it before
                // we touch the buffer's memory below. `.current()` already
                // reflects this commit — smithay merges the cached state
                // during its own commit resolution, before our handler runs.
                //
                // This is a synchronous, bounded wait on veil's single-
                // threaded event loop, not smithay's own async blocker
                // pattern (DrmSyncPoint::generate_blocker + compositor::
                // add_blocker registered on a calloop source). That's the
                // "correct" non-stalling approach; this is the pragmatic one.
                // If a GPU client ever visibly stalls the compositor here,
                // that's the upgrade path — not a rewrite of this.
                // No-op (both None) for any surface that never bound the
                // syncobj-surface protocol object, i.e. every client today.
                let (acquire_pt, release_pt) = with_states(surface, |states| {
                    let mut cached = states.cached_state.get::<DrmSyncobjCachedState>();
                    let cur = cached.current();
                    (cur.acquire_point.clone(), cur.release_point.clone())
                });
                if let Some(pt) = &acquire_pt {
                    if let Err(e) = pt.wait(16_000_000) {
                        tracing::warn!(
                            "commit {} — syncobj acquire wait failed/timed out: {e}",
                            surface.id()
                        );
                    }
                }

                // Reuse last frame's backing Vec for this surface instead of
                // allocating fresh every commit — same-size redraws (by far
                // the common case: video, animations, cursor blink) then cost
                // zero allocator churn instead of a alloc+free of the full
                // buffer every single commit. Falls back to an empty Vec
                // (first commit, or previous buffer type was GPU-detiled and
                // we don't reuse across that boundary — see note below).
                let mut dest = self.surface_buffers.remove(&surface.id()).map(|b| b.rgba).unwrap_or_default();

                // Try shm first, then dmabuf.
                let imported = with_buffer_contents(&buffer, |ptr, len, data| {
                    tracing::info!("commit {} shm {}x{} fmt={:?}", surface.id(), data.width, data.height, data.format);
                    let raw = unsafe { std::slice::from_raw_parts(ptr, len) };
                    shm_to_rgba_into(raw, &data, &mut dest)
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
                        import_dmabuf_into(dmabuf, &mut dest)
                    } else if let Some(gpu) = self.gpu_lazy() {
                        // NOTE: GPU detile path still allocates fresh each
                        // call (untouched — didn't want to change detile.rs
                        // without checking its internals first). Reusable
                        // `dest` is dropped here; not a regression, just not
                        // the win the other two paths get.
                        gpu.import(dmabuf).map(|(rgba, w, h)| { dest = rgba; (w, h) })
                    } else {
                        None
                    }
                });

                if let Some((w, h)) = imported {
                    tracing::info!("commit {} → surface_buffers {}x{}", surface.id(), w, h);
                    self.surface_buffers.insert(surface.id(), SurfaceBuf { rgba: dest, w, h });
                    match toplevel_rect_for(self, surface) {
                        Some(r) => mark_dirty_rect(self, r),
                        None    => mark_dirty_full(self), // subsurface/popup/cursor — no precise rect
                    }
                } else {
                    tracing::warn!("commit {} — unsupported buffer type, skipping", surface.id());
                }

                // Tell the client's GPU it can reuse/free the buffer now that
                // we're done reading it — mirrors buffer.release() below, just
                // for explicit-sync clients specifically.
                if let Some(pt) = &release_pt {
                    if let Err(e) = pt.signal() {
                        tracing::warn!(
                            "commit {} — syncobj release signal failed: {e}",
                            surface.id()
                        );
                    }
                }
                buffer.release();
            }
            Assign::Removed => {
                self.surface_buffers.remove(&surface.id());
                match toplevel_rect_for(self, surface) {
                    Some(r) => mark_dirty_rect(self, r),
                    None    => mark_dirty_full(self),
                }
            }
            Assign::None => {
                // Pure state commit (geometry, role config). Full-frame: a
                // subsurface offset change could affect area outside its own
                // parent toplevel's last-known rect, and this is infrequent
                // enough that precise tracking here isn't worth the risk.
                mark_dirty_full(self);
            }
        }

        // Let PopupManager update its internal book-keeping.
        self.popups.commit(surface);
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        if self.surface_buffers.remove(&surface.id()).is_some() {
            match toplevel_rect_for(self, surface) {
                Some(r) => mark_dirty_rect(self, r),
                None    => mark_dirty_full(self),
            }
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

        self.toplevels.push(Window::new(WindowSurface::Xdg(surface), self.active_workspace));
        // New window takes focus; retile so every window gets its rect + size.
        let ws = self.active_workspace;
        let n = self.toplevels.iter().filter(|t| t.alive() && t.workspace == ws).count();
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

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<wl_output::WlOutput>) {
        let wl = surface.wl_surface().clone();
        let ws = self.active_workspace;
        if let Some(i) = self.toplevels.iter().filter(|t| t.alive() && t.workspace == ws)
            .position(|t| t.wl_surface().as_ref() == Some(&wl))
        {
            self.layout.focused = i;
        }
        self.fullscreen = Some(wl);
        relayout(self);
        refocus_keyboard(self);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if self.fullscreen.as_ref() == Some(surface.wl_surface()) {
            self.fullscreen = None;
            relayout(self);
        }
    }
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
        mark_dirty_full(self);
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
                | Fourcc::Rgbx8888 | Fourcc::Bgrx8888
            );

        if cpu_ok {
            let _ = notifier.successful::<State>();
            return;
        }

        // GPU path: any tiled / non-linear buffer, imported as an EGLImage and
        // read back linear (see detile::GpuImporter). Actually trial-import it
        // now so we only accept what we can genuinely detile.
        // First-touch point for a tiled buffer: this is where the lazy GPU
        // context actually gets stood up (see State::gpu_lazy), not commit().
        if let Some(gpu) = self.gpu_lazy() {
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

impl DrmSyncobjHandler for State {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
    // surface_associated default (no-op) is fine — X11Surface::wl_surface()
    // reflects the pairing automatically once smithay's own bookkeeping
    // completes; we don't need a notification hook for it.
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
// drm_syncobj's manager/surface/timeline dispatch — smithay 0.7.0 uses the
// classic Dispatch/GlobalDispatch pattern here (not Dispatch2, which is
// master-branch-only and not in this pinned version), hence its own macro
// rather than the generic delegate_dispatch2! bridge.
delegate_drm_syncobj!(State);
delegate_xwayland_shell!(State);

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
        Format { code: Fourcc::Rgbx8888, modifier: Modifier::Linear },
        Format { code: Fourcc::Bgrx8888, modifier: Modifier::Linear },
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
/// Writes into `out` in place instead of allocating — see shm_to_rgba_into.
fn import_dmabuf_into(dmabuf: &Dmabuf, out: &mut Vec<u8>) -> Option<(u32, u32)> {
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

    out.clear();
    out.reserve(w * h * 4);
    for y in 0..h {
        let row = &pixel_data[y * stride .. y * stride + w * 4];
        for px in row.chunks_exact(4) {
            // DRM stores as little-endian u32: ARGB8888 = B,G,R,A in memory
            let (r, g, b) = match fmt.code {
                Fourcc::Argb8888 | Fourcc::Xrgb8888 => (px[2], px[1], px[0]),
                Fourcc::Abgr8888 | Fourcc::Xbgr8888 => (px[0], px[1], px[2]),
                Fourcc::Rgbx8888                    => (px[3], px[2], px[1]),
                Fourcc::Bgrx8888                    => (px[1], px[2], px[3]),
                _ => return None,
            };
            out.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Some((w as u32, h as u32))
}

/// Writes into `out` in place instead of allocating — `out.clear()` keeps
/// its existing heap capacity, so a same-size redraw (the common case)
/// reuses the same allocation instead of alloc+free every commit.
fn shm_to_rgba_into(raw: &[u8], data: &smithay::wayland::shm::BufferData, out: &mut Vec<u8>) -> Option<(u32, u32)> {
    let w = data.width  as usize;
    let h = data.height as usize;
    let stride = data.stride as usize;
    if stride < w * 4 || raw.len() < stride * h { return None; }

    out.clear();
    out.reserve(w * h * 4);
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
    Some((w as u32, h as u32))
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

fn adjust_volume(up: bool) -> (String, Option<u8>) {
    let arg = if up { "5%+" } else { "5%-" };
    if let Ok(out) = Command::new("wpctl")
        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", arg])
        .output()
    {
        if out.status.success() {
            if let Ok(get_out) = Command::new("wpctl")
                .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
                .output()
            {
                let s = String::from_utf8_lossy(&get_out.stdout);
                if s.contains("[MUTED]") {
                    return ("MUTED".to_string(), Some(0));
                }
                if let Some(val_str) = s.split_whitespace().nth(1) {
                    if let Ok(val) = val_str.parse::<f32>() {
                        let pct = (val * 100.0).round() as u8;
                        return (format!("{pct}%"), Some(pct));
                    }
                }
            }
        }
    }

    let amixer_arg = if up { "5%+" } else { "5%-" };
    if let Ok(out) = Command::new("amixer")
        .args(["sset", "Master", amixer_arg])
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(start) = s.find('[') {
            if let Some(end) = s[start..].find('%') {
                if let Ok(pct) = s[start + 1..start + end].parse::<u8>() {
                    return (format!("{pct}%"), Some(pct));
                }
            }
        }
    }

    let pactl_arg = if up { "+5%" } else { "-5%" };
    if let Ok(_) = Command::new("pactl")
        .args(["set-sink-volume", "@DEFAULT_SINK@", pactl_arg])
        .output()
    {
        return ("ADJUSTED".to_string(), None);
    }

    (if up { "+5%" } else { "-5%" }.to_string(), None)
}

fn toggle_volume_mute() -> (String, Option<u8>) {
    if let Ok(_) = Command::new("wpctl")
        .args(["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"])
        .output()
    {
        if let Ok(get_out) = Command::new("wpctl")
            .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
            .output()
        {
            let s = String::from_utf8_lossy(&get_out.stdout);
            if s.contains("[MUTED]") {
                return ("MUTED".to_string(), Some(0));
            } else if let Some(val_str) = s.split_whitespace().nth(1) {
                if let Ok(val) = val_str.parse::<f32>() {
                    let pct = (val * 100.0).round() as u8;
                    return (format!("UNMUTED ({pct}%)"), Some(pct));
                }
            }
        }
    }

    if let Ok(out) = Command::new("amixer")
        .args(["sset", "Master", "toggle"])
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        if s.contains("[off]") {
            return ("MUTED".to_string(), Some(0));
        } else {
            return ("UNMUTED".to_string(), None);
        }
    }

    ("TOGGLE MUTE".to_string(), None)
}

fn adjust_brightness(up: bool) -> (String, Option<u8>) {
    let arg = if up { "+5%" } else { "5%-" };
    if let Ok(out) = Command::new("brightnessctl")
        .args(["set", arg])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(start) = s.find('(') {
                if let Some(end) = s[start..].find('%') {
                    if let Ok(pct) = s[start + 1..start + end].parse::<u8>() {
                        return (format!("{pct}%"), Some(pct));
                    }
                }
            }
        }
    }

    let light_flag = if up { "-A" } else { "-U" };
    if let Ok(_) = Command::new("light")
        .args([light_flag, "5"])
        .output()
    {
        if let Ok(get_out) = Command::new("light").output() {
            let s = String::from_utf8_lossy(&get_out.stdout);
            if let Ok(val) = s.trim().parse::<f32>() {
                let pct = val.round() as u8;
                return (format!("{pct}%"), Some(pct));
            }
        }
    }

    (if up { "+5%" } else { "-5%" }.to_string(), None)
}

fn draw_osd_overlay(osd: &OsdNotification, back: &mut [u8], w: u32, h: u32) {
    use crate::font5x7::{draw_text, fill_rect, GLYPH_H, GLYPH_W};

    let scale = 2u32;
    let advance = (GLYPH_W + 1) * scale;
    let line_h = (GLYPH_H + 3) * scale;
    let pad = 12i32;

    let title_line = format!("{}", osd.title.to_ascii_uppercase());
    let body_line = format!("{}", osd.body.to_ascii_uppercase());

    let has_bar = osd.progress.is_some();
    let bar_h = if has_bar { 14i32 } else { 0i32 };

    let mut lines = vec![title_line];
    if !body_line.is_empty() {
        lines.push(body_line);
    }

    let text_cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(18) as u32;
    let box_w = (text_cols * advance + pad as u32 * 2).max(220);
    let box_h = lines.len() as u32 * line_h + pad as u32 * 2 + if has_bar { bar_h as u32 } else { 0 };

    let x0 = ((w as i32 - box_w as i32) / 2).max(0);
    let y0 = (h as i32 / 12).max(10);

    let is_error = osd.title.contains("ERROR");
    let border_color = if is_error { [255, 85, 85, 255] } else { [128, 222, 234, 255] };

    let border = 2i32;
    fill_rect(back, w, h, x0 - border, y0 - border, box_w + (border as u32 * 2), box_h + (border as u32 * 2), border_color);
    fill_rect(back, w, h, x0, y0, box_w, box_h, [10, 10, 20, 240]);

    for (i, line) in lines.iter().enumerate() {
        let ty = y0 + pad + i as i32 * line_h as i32;
        let color = if i == 0 {
            if is_error { [255, 100, 100, 255] } else { [255, 215, 0, 255] }
        } else {
            [220, 220, 240, 255]
        };
        draw_text(back, w, h, x0 + pad, ty, scale, line, color);
    }

    if let Some(pct) = osd.progress {
        let bar_x = x0 + pad;
        let bar_y = y0 + pad + lines.len() as i32 * line_h as i32 + 2;
        let inner_w = box_w - (pad as u32 * 2);
        let filled_w = (inner_w * pct.min(100) as u32) / 100;

        fill_rect(back, w, h, bar_x, bar_y, inner_w, 10, [40, 40, 60, 255]);
        fill_rect(back, w, h, bar_x, bar_y, filled_w, 10, border_color);
    }
}

/// Reload configuration from disk (`config_path()`), updating keybinds,
/// background color, composite rate, etc.
pub fn reload_config(state: &mut State) {
    let path = veil_config::config_path();
    let new_cfg = match path.as_ref() {
        Some(p) => match veil_config::try_load(p) {
            Ok(cfg) => cfg,
            Err(e) => {
                let short_err = e.lines().next().unwrap_or("Syntax error").to_string();
                eprintln!("[config] failed to parse {:?}: {}", p, e);
                state.show_osd("CONFIG ERROR", short_err, None, Duration::from_secs(4));
                return;
            }
        },
        None => veil_config::VeilConfig::default(),
    };

    state.config_mtime = path.as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());
    state.config_path = path.clone();

    state.keybinds = new_cfg.keybinds;
    state.background = [new_cfg.background[0], new_cfg.background[1], new_cfg.background[2], 255];
    state.theme = new_cfg.theme;
    state.bar = new_cfg.bar;
    state.composite_interval = Duration::from_millis(1000 / new_cfg.fps.max(1) as u64);

    if let Some(p) = &path {
        let name = p.file_name().and_then(|f| f.to_str()).unwrap_or("config.lua");
        state.show_osd("CONFIG RELOADED", format!("Applied {name}"), None, Duration::from_secs(2));
        eprintln!("[veil-host] reloaded config from {}", p.display());
    } else {
        state.show_osd("CONFIG RELOADED", "Using default config", None, Duration::from_secs(2));
        eprintln!("[veil-host] reloaded default config (no config.lua found)");
    }
    mark_dirty_full(state);
}

/// Run a Combo-4 keybind action against the live layout, then re-tile and
/// re-focus so the client sees the result immediately.
fn dispatch_action(state: &mut State, action: veil_config::Action) {
    use veil_config::Action::*;
    let ws = state.active_workspace; // pulled out once; every filter below needs it
    match action {
        FocusLeft  => state.layout.focus(&state.layout_rects, crate::layout::Dir::Left),
        FocusRight => state.layout.focus(&state.layout_rects, crate::layout::Dir::Right),
        FocusUp    => state.layout.focus(&state.layout_rects, crate::layout::Dir::Up),
        FocusDown  => state.layout.focus(&state.layout_rects, crate::layout::Dir::Down),
        Swap => {
            // swap_next's indices are positions among LIVE toplevels ON THE
            // ACTIVE WORKSPACE; map them back to real Vec indices in case a
            // dead-but-unpruned or other-workspace entry sits between live
            // ones.
            let live_idx: Vec<usize> = state.toplevels.iter().enumerate()
                .filter(|(_, t)| t.alive() && t.workspace == ws)
                .map(|(i, _)| i)
                .collect();
            if let Some((a, b)) = state.layout.swap_next(live_idx.len()) {
                state.toplevels.swap(live_idx[a], live_idx[b]);
            }
        }
        Rotate => state.layout.rotate_split(),
        Close => {
            if let Some(tl) = state.toplevels.iter().filter(|t| t.alive() && t.workspace == ws).nth(state.layout.focused) {
                tl.close_window();
            }
        }
        ResizeGrow   => state.layout.resize_grow(),
        ResizeShrink => state.layout.resize_shrink(),
        ToggleLayout => state.layout.toggle_mode(),
        ToggleFullscreen => {
            let focused_wl = state.toplevels.iter().filter(|t| t.alive() && t.workspace == ws)
                .nth(state.layout.focused)
                .and_then(|t| t.wl_surface());
            match (&state.fullscreen, &focused_wl) {
                // Already fullscreen on the focused window — toggle off.
                (Some(fs), Some(f)) if fs == f => state.fullscreen = None,
                // Nothing focused-fullscreen yet (including: something ELSE
                // is fullscreen and you've since focused a different
                // window) — fullscreen whichever window has focus now.
                (_, Some(f)) => state.fullscreen = Some(f.clone()),
                (_, None) => {}
            }
        }
        ReloadConfig => reload_config(state),
        SwitchWorkspace(n) => {
            let target = n.saturating_sub(1).min(veil_config::WORKSPACE_COUNT - 1);
            if target != state.active_workspace {
                // Save the outgoing workspace's tiling state and restore the
                // target's, so hopping back later finds it exactly as left —
                // not reset to the dwindle default.
                state.workspace_layouts[state.active_workspace as usize] = state.layout;
                state.layout = state.workspace_layouts[target as usize];
                state.active_workspace = target;
                state.show_osd(format!("WORKSPACE {}", target + 1), "", None, Duration::from_millis(900));
                mark_dirty_full(state);
            }
        }
        MoveToWorkspace(n) => {
            let target = n.saturating_sub(1).min(veil_config::WORKSPACE_COUNT - 1);
            // Real Vec index of whatever's focused right now, found the same
            // way Close/ToggleFullscreen do — `state.layout.focused` is a
            // position among LIVE ACTIVE-WORKSPACE windows, not a raw index.
            let real_idx = state.toplevels.iter().enumerate()
                .filter(|(_, t)| t.alive() && t.workspace == ws)
                .nth(state.layout.focused)
                .map(|(i, _)| i);
            if let (Some(idx), true) = (real_idx, target != ws) {
                state.toplevels[idx].workspace = target;
                // Follow the window: same swap-in/out as SwitchWorkspace, so
                // the workspace we land on keeps its own layout state
                // instead of inheriting whatever the old one had.
                state.workspace_layouts[state.active_workspace as usize] = state.layout;
                state.layout = state.workspace_layouts[target as usize];
                state.active_workspace = target;
                state.show_osd(format!("MOVED TO WORKSPACE {}", target + 1), "", None, Duration::from_millis(900));
                mark_dirty_full(state);
            }
            // No window focused (empty workspace) — nothing to move, and
            // deliberately don't switch either; a bare workspace-switch
            // keybind already exists for that (SwitchWorkspace above).
        }
        VolumeUp => {
            let (body, pct) = adjust_volume(true);
            state.show_osd("VOLUME", body, pct, Duration::from_secs(2));
        }
        VolumeDown => {
            let (body, pct) = adjust_volume(false);
            state.show_osd("VOLUME", body, pct, Duration::from_secs(2));
        }
        VolumeMute => {
            let (body, pct) = toggle_volume_mute();
            state.show_osd("VOLUME", body, pct, Duration::from_secs(2));
        }
        BrightnessUp => {
            let (body, pct) = adjust_brightness(true);
            state.show_osd("BRIGHTNESS", body, pct, Duration::from_secs(2));
        }
        BrightnessDown => {
            let (body, pct) = adjust_brightness(false);
            state.show_osd("BRIGHTNESS", body, pct, Duration::from_secs(2));
        }
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
        mark_dirty_full(state);
        return;
    }

    if sym == keysyms::KEY_Escape {
        state.launcher = None;
        mark_dirty_full(state);
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
            mark_dirty_full(state);
        }
        return;
    }
    if sym == keysyms::KEY_Up {
        if let Some(l) = state.launcher.as_mut() {
            l.selected = l.selected.saturating_sub(1);
            mark_dirty_full(state);
        }
        return;
    }
    if sym == keysyms::KEY_Down {
        if let Some(l) = state.launcher.as_mut() {
            let count = l.matches().len();
            if count > 0 { l.selected = (l.selected + 1).min(count - 1); }
            mark_dirty_full(state);
        }
        return;
    }
    if sym == keysyms::KEY_Page_Down {
        if let Some(l) = state.launcher.as_mut() {
            let count = l.matches().len();
            if count > 0 { l.selected = (l.selected + 10).min(count - 1); }
            mark_dirty_full(state);
        }
        return;
    }
    if sym == keysyms::KEY_Page_Up {
        if let Some(l) = state.launcher.as_mut() {
            l.selected = l.selected.saturating_sub(10);
            mark_dirty_full(state);
        }
        return;
    }
    if sym == keysyms::KEY_Home {
        if let Some(l) = state.launcher.as_mut() {
            l.selected = 0;
            mark_dirty_full(state);
        }
        return;
    }
    if sym == keysyms::KEY_End {
        if let Some(l) = state.launcher.as_mut() {
            let count = l.matches().len();
            if count > 0 { l.selected = count - 1; }
            mark_dirty_full(state);
        }
        return;
    }

    let cp = smithay::input::keyboard::xkb::keysym_to_utf32(keysym.modified_sym());
    if let Some(c) = char::from_u32(cp) {
        if !c.is_control() {
            if let Some(l) = state.launcher.as_mut() {
                l.query.push(c);
                l.selected = 0;
                mark_dirty_full(state);
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
    mark_dirty_full(state);

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

/// Fixed bar height in pixels: one line of the built-in 5x7 font at scale 1
/// (7px glyph) plus 5px padding — see draw_bar(). Not configurable; the
/// three-column layout it implies (16/8/rest chars) is sized against this
/// exact value.
const BAR_HEIGHT: u32 = 12;

/// Recompute the dwindle tiling and push each toplevel its new size. Call
/// whenever the live window set or the output size changes. Also marks the
/// focused window Activated (others deactivated) so clients render focus state.
fn relayout(state: &mut State) {
    let ws = state.active_workspace;
    let n = state.toplevels.iter().filter(|t| t.alive() && t.workspace == ws).count();
    if state.layout.focused >= n {
        state.layout.focused = n.saturating_sub(1);
    }
    let focused = state.layout.focused;

    // Same alive-and-active-workspace sequence configure/composite/pick_focus
    // all already use — index into THIS, not the raw toplevels vec. Windows
    // on other workspaces are simply absent here, so they never get a
    // configure_size call and never enter layout_rects/composite below —
    // that's what actually hides them, no separate visibility flag needed.
    let alive: Vec<&Window> = state.toplevels.iter().filter(|t| t.alive() && t.workspace == ws).collect();

    // Which (alive-sequence) index, if any, is the fullscreen window right
    // now. Looked up by identity every call rather than cached, since
    // `state.fullscreen` only stores a WlSurface, not a position.
    let fs_idx = state.fullscreen.as_ref().and_then(|fs| {
        alive.iter().position(|t| t.wl_surface().as_ref() == Some(fs))
    });

    // Bar reserves a strip of the output — tiled windows only ever see
    // what's left. A fullscreen window still overrides to the FULL output
    // just below, ignoring this: fullscreen conventionally covers the bar
    // too, and composite() skips drawing the bar while something's
    // fullscreen, so nothing's left peeking out from underneath it.
    let (tile_h, tile_y) = if state.bar.enabled {
        let h = state.output_h.saturating_sub(BAR_HEIGHT);
        let y = if state.bar.position == veil_config::BarPosition::Top { BAR_HEIGHT as i32 } else { 0 };
        (h, y)
    } else {
        (state.output_h, 0)
    };

    // Everyone tiles exactly as if nothing were fullscreen — this is what
    // keeps the *other* windows' positions stable across a fullscreen
    // toggle instead of re-tiling around the gap (previously this called
    // rects(n-1, ...) for "everyone else", which re-tiled them every time
    // and, in Scroll mode, used the wrong focused index since that mode's
    // rects depend on it for viewport position — both are what caused
    // windows to visibly jump/shrink/shift left on fullscreen toggle).
    // The fullscreen window's rect is simply overridden afterward.
    let mut rects = state.layout.rects(n, state.output_w, tile_h);
    for r in rects.iter_mut() {
        r.y += tile_y;
    }
    if let Some(fs_i) = fs_idx {
        rects[fs_i] = Rect { x: 0, y: 0, w: state.output_w, h: state.output_h };
    }

    for (i, tl) in alive.iter().enumerate() {
        let is_fs = fs_idx == Some(i);
        // While something's fullscreen, it's the only thing activated —
        // matches it visually being the only thing on screen. Otherwise,
        // normal focus-follows-tiling behavior.
        let activated = fs_idx.map_or(i == focused, |_| is_fs);
        tl.configure_size(rects[i], activated, is_fs);
    }

    state.layout_rects = rects;
    mark_dirty_full(state);
}

/// Point the keyboard at whichever live toplevel is currently focused (or
/// nothing, if there are no windows left).
fn refocus_keyboard(state: &mut State) {
    let ws = state.active_workspace;
    let target = state.toplevels.iter()
        .filter(|t| t.alive() && t.workspace == ws)
        .nth(state.layout.focused)
        .and_then(|t| t.wl_surface());
    let serial = state.next_serial();
    let kb = state.keyboard.clone();
    kb.set_focus(state, target, serial);
}

/// Maps a committing/destroyed `WlSurface` to the on-screen rect of the
/// toplevel it's the ROOT surface of, if it is one. Returns `None` for
/// anything else (subsurfaces, popups, cursor surfaces) — callers fall back
/// to `mark_dirty_full` in that case. Deliberately not subsurface-tree-aware:
/// a toplevel's root surface committing is the common case (most apps render
/// straight into it), and under-detecting here only costs a full-frame
/// redraw instead of a partial one — it can never cause a stale-pixel bug,
/// since full-frame damage always covers whatever a precise rect would have.
fn toplevel_rect_for(state: &State, surface: &WlSurface) -> Option<Rect> {
    let i = state.toplevels.iter().position(|t| t.wl_surface().as_ref() == Some(surface))?;
    state.layout_rects.get(i).copied()
}

/// Marks the whole output as needing repaint — the safe default for any
/// dirty event without a precise on-screen rect (resize, cursor motion,
/// overlay toggles, relayout, anything not going through
/// `toplevel_rect_for`). Over-damaging can only cost extra redraw work, never
/// leave stale pixels, so this is always a legal fallback.
fn mark_dirty_full(state: &mut State) {
    state.dirty = true;
    let full = Rect { x: 0, y: 0, w: state.output_w, h: state.output_h };
    state.damage = Some(match state.damage.take() {
        Some(d) => d.union(&full),
        None => full,
    });
}

/// Marks just `rect` as needing repaint, unioned with whatever's already
/// pending this tick (multiple surfaces can go dirty between composites).
fn mark_dirty_rect(state: &mut State, rect: Rect) {
    state.dirty = true;
    state.damage = Some(match state.damage.take() {
        Some(d) => d.union(&rect),
        None => rect,
    });
}

/// Composite all live toplevels + their popups + the cursor into a single
/// RGBA frame and ship it. Called from the periodic tick when `dirty`.
fn composite_and_send(state: &mut State) {
    if !state.dirty { return; }
    let now = Instant::now();
    if let Some(t) = state.last_composite {
        if now.duration_since(t) < state.composite_interval { return; }
    }
    state.last_composite = Some(now);
    state.dirty = false;
    // Snapshot + reset this tick's damage now, before any of the drawing
    // below — so damage that arrives *during* composite (shouldn't happen on
    // this single-threaded loop, but keeps the invariant obviously true
    // rather than relying on ordering elsewhere) accumulates for next tick
    // instead of being silently dropped.
    let frame_damage = state.damage.take()
        .unwrap_or(Rect { x: 0, y: 0, w: state.output_w, h: state.output_h })
        .clamp_to(state.output_w, state.output_h);
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
    let ws = state.active_workspace;

    let back = &mut state.composite_buf;

    // Toplevels (root buffer + subsurfaces) then their popups, each at its
    // tiled rect origin. Popups are positioned relative to their toplevel.
    // Collected as Option so index i still lines up with layout_rects[i] —
    // an X11 window not yet paired with a wl_surface has nothing to blit,
    // but it still occupies a tiled slot and must not shift later indices.
    // Filtered to the active workspace — this MUST produce the same
    // alive-and-active-workspace sequence relayout() used to build
    // layout_rects, or index i here won't line up with rects[i] anymore.
    let toplevels: Vec<Option<WlSurface>> = state.toplevels.iter()
        .filter(|t| t.alive() && t.workspace == ws)
        .map(|t| t.wl_surface())
        .collect();
    for (i, surf_opt) in toplevels.iter().enumerate() {
        let Some(surf) = surf_opt else { continue };
        let r = state.layout_rects.get(i).copied()
            .unwrap_or(Rect { x: 0, y: 0, w, h });
        blit_subtree(back, w, h, &state.surface_buffers, surf, (r.x, r.y));
        for (popup, off) in PopupManager::popups_for_surface(surf) {
            let ps = popup.wl_surface().clone();
            blit_subtree(back, w, h, &state.surface_buffers, &ps, (r.x + off.x, r.y + off.y));
        }
    }

    // Fullscreen window repaints last among tiled content, regardless of its
    // position in the loop above — other windows keep their normal (real,
    // non-full) tiled rects now (see relayout()), so without this, one that
    // happens to iterate after the fullscreen window in `toplevels` order
    // could paint its own small rect right over part of it.
    //
    // Workspace-filtered too: `state.fullscreen` is only ever set from a
    // window that was active-workspace at the time (dispatch_action's
    // ToggleFullscreen and the xdg fullscreen_request handler both look up
    // the focused window the same filtered way), so a fullscreen window on
    // a workspace you've since switched away from correctly stops matching
    // here and this block becomes a no-op until you switch back.
    if let Some(fs) = &state.fullscreen {
        if let Some(surf) = state.toplevels.iter().filter(|t| t.alive() && t.workspace == ws)
            .find(|t| t.wl_surface().as_ref() == Some(fs))
            .and_then(|t| t.wl_surface())
        {
            blit_subtree(back, w, h, &state.surface_buffers, &surf, (0, 0));
            for (popup, off) in PopupManager::popups_for_surface(&surf) {
                let ps = popup.wl_surface().clone();
                blit_subtree(back, w, h, &state.surface_buffers, &ps, (off.x, off.y));
            }
        }
    }

    // Override-redirect windows (menus, tooltips, dropdowns) — absolute
    // position from the X server, not a tiled rect. Always on top of tiled
    // content; smithay updates X11Surface's internal geometry before
    // configure_notify fires, so .geometry() here is always current, no
    // caching needed on our side.
    for f in state.floating.iter().filter(|f| f.alive()) {
        let Some(surf) = f.wl_surface() else { continue };
        let g = f.geometry();
        blit_subtree(back, w, h, &state.surface_buffers, &surf, (g.loc.x, g.loc.y));
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
    // `theme` is `Copy` (plain color bytes) so pulling it out ahead of the
    // `back` borrow costs nothing.
    let theme = state.theme;
    if state.bar.enabled && state.fullscreen.is_none() {
        let mut occupancy = [0u8; veil_config::WORKSPACE_COUNT as usize];
        for t in state.toplevels.iter().filter(|t| t.alive()) {
            let idx = t.workspace as usize;
            if idx < occupancy.len() {
                occupancy[idx] += 1;
            }
        }
        let active_ws = state.active_workspace;
        let hitboxes = draw_bar(&theme, &state.bar, active_ws, &occupancy, back, w, h);
        state.bar_hitboxes = hitboxes;
    } else {
        // Fullscreen hides the bar entirely (conventional — see relayout()'s
        // note on why the fullscreen rect ignores the bar's reserved strip)
        // and a disabled bar obviously has nothing to draw. Either way, old
        // hitboxes must go or a click could "launch" through a bar that
        // isn't there anymore.
        state.bar_hitboxes.clear();
    }
    if show_help {
        let keybinds = state.keybinds.clone();
        draw_help_overlay(&keybinds, &theme, back, w, h);
    }
    if launcher_present {
        if let Some(ref l) = state.launcher {
            let mod_key = state.keybinds.mod_key;
            // Borrow checker: we need to call draw_launcher_overlay with `back`
            // already mutably borrowed. Clone the launcher (it's tiny) rather
            // than fighting the borrow checker with unsafe aliasing.
            let l = l.clone();
            draw_launcher_overlay(&l, mod_key, &theme, back, w, h);
        }
    }
    if let Some(ref osd) = state.osd {
        if Instant::now() > osd.expires_at {
            state.osd = None;
            state.dirty = true;
        } else {
            let osd_clone = osd.clone();
            draw_osd_overlay(&osd_clone, back, w, h);
            state.dirty = true;
        }
    }

    state.frame_serial = state.frame_serial.wrapping_add(1);

    // Zero-copy dispatch: swap composite_buf out, wrap in Arc, hand to the
    // render thread. For the *next* frame's buffer: try to reclaim the Arc
    // we sent last time via try_unwrap — if the render thread already
    // dropped its clone (the common case; it's the bottleneck, not us),
    // that's a real allocation avoided instead of "one alloc per frame no
    // matter what."
    let mut outgoing = Vec::new();
    std::mem::swap(&mut state.composite_buf, &mut outgoing);
    let outgoing = Arc::new(outgoing);
    state.composite_buf = match state.prev_frame.take().map(Arc::try_unwrap) {
        Some(Ok(mut reclaimed)) => {
            reclaimed.clear();
            reclaimed.reserve(needed);
            reclaimed
        }
        // Render thread is still holding its reference (lagging) or this is
        // the first frame — fall back to a fresh allocation, same as before.
        _ => Vec::with_capacity(needed),
    };
    state.prev_frame = Some(outgoing.clone());
    let _ = state.frame_tx.send(crate::sink::Frame {
        rgba: outgoing, width: w, height: h, serial: state.frame_serial,
        damage: frame_damage,
    });

    // Fire frame callbacks now that we've consumed and displayed this frame.
    // Chromium uses these as vsync: it won't submit the next buffer until
    // it receives one. Firing here (after composite) caps Chromium's render
    // rate to our composite_interval instead of the 8ms tick rate.
    let time = state.start_time.elapsed().as_millis() as u32;
    let surfaces: Vec<WlSurface> = state.toplevels.iter()
        .filter(|t| t.alive())
        .filter_map(|t| t.wl_surface())
        .chain(state.floating.iter().filter(|f| f.alive()).filter_map(|f| f.wl_surface()))
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
/// Renders the status bar: workspace widget (fixed 16 chars), clock (fixed
/// 8 chars), then app shortcuts filling whatever's left. Returns the
/// on-screen click target for each app tile — PointerButton consults this
/// directly rather than redoing this layout math per click.
///
/// Font note: the built-in 5x7 bitmap font (font5x7.rs) is caps-only and
/// has no bullet glyph, so app labels render UPPERCASE regardless of case
/// here, and the workspace widget's per-window marks use `*` in place of
/// the `•` bullets from the original bar spec — closest available glyph
/// with real visual weight (`.` renders as a single near-invisible pixel
/// at this scale).
fn draw_bar(
    theme: &veil_config::Theme,
    bar: &veil_config::BarConfig,
    active_ws: u8,
    occupancy: &[u8; veil_config::WORKSPACE_COUNT as usize],
    back: &mut [u8],
    w: u32,
    h: u32,
) -> Vec<(Rect, String)> {
    use crate::font5x7::{draw_text, fill_rect};
    use veil_config::BarPosition;

    const SCALE: u32 = 1; // full help/launcher overlays use 2 — bar stays
                           // compact: 7px glyph + 5px padding = BAR_HEIGHT.
    const ADVANCE: u32 = 6; // (GLYPH_W + 1) * SCALE, matches font5x7's own spacing formula
    const PAD_Y: i32 = 3;   // vertically centers a 7px glyph in a 12px bar

    let y0 = if bar.position == BarPosition::Top { 0 } else { h.saturating_sub(BAR_HEIGHT) as i32 };
    fill_rect(back, w, h, 0, y0, w, BAR_HEIGHT, theme.panel_bg);

    // --- Column 1: workspace widget, fixed 16 chars / 96px ---
    // Only draws up to whichever's higher: the active workspace, or the
    // highest-numbered occupied one — otherwise all 9 slots would eat the
    // whole budget before the clock column even started. Each token is
    // "N:" (empty) or "N:" + up to 3 `*` (one per window, capped). Active
    // workspace's token gets the accent color so it stands out at a glance.
    let col1_w = 16 * ADVANCE;
    let highest = occupancy.iter().rposition(|&c| c > 0)
        .map(|i| i as u8 + 1)
        .unwrap_or(0)
        .max(active_ws + 1)
        .min(veil_config::WORKSPACE_COUNT);
    let mut cx = 2i32;
    for n in 1..=highest {
        let count = occupancy[(n - 1) as usize];
        let token = format!("{n}:{}", "*".repeat(count.min(3) as usize));
        let token_w = token.chars().count() as u32 * ADVANCE;
        if cx as u32 + token_w > col1_w { break; }
        let color = if n == active_ws + 1 { theme.accent } else { theme.text_dim };
        draw_text(back, w, h, cx, y0 + PAD_Y, SCALE, &token, color);
        cx += token_w as i32 + ADVANCE as i32; // one blank char of gap
    }

    // --- Column 2: clock, fixed 8 chars / 48px, starts right after col 1 ---
    // 24-hour per spec. No internal timer needed — this just reads the
    // current time on whatever cadence composite() already runs at; the
    // displayed minute obviously only visibly changes once a minute.
    let col2_x = col1_w as i32;
    let clock = chrono::Local::now().format("%H:%M").to_string();
    draw_text(back, w, h, col2_x + 2, y0 + PAD_Y, SCALE, &clock, theme.text);

    // --- Column 3: app shortcuts, whatever width is left ---
    let col3_x = col1_w as i32 + 8 * ADVANCE as i32;
    let mut hitboxes = Vec::new();
    let mut tx = col3_x + 2;
    for app in &bar.apps {
        let label = format!("[{}]", app.name);
        let label_w = label.chars().count() as u32 * ADVANCE;
        if tx as u32 + label_w > w { break; } // out of bar width — rest just don't fit
        draw_text(back, w, h, tx, y0 + PAD_Y, SCALE, &label, theme.text);
        hitboxes.push((Rect { x: tx, y: y0, w: label_w, h: BAR_HEIGHT }, app.exec.clone()));
        tx += label_w as i32 + ADVANCE as i32;
    }

    hitboxes
}

fn draw_help_overlay(keybinds: &veil_config::Keybinds, theme: &veil_config::Theme, back: &mut [u8], w: u32, h: u32) {
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
    fill_rect(back, w, h, x0 - border, y0 - border, box_w + (border as u32 * 2), box_h + (border as u32 * 2), theme.border);
    fill_rect(back, w, h, x0, y0, box_w, box_h, theme.panel_bg);
    for (i, line) in lines.iter().enumerate() {
        let ty = y0 + pad + i as i32 * line_h as i32;
        let color = if i == 0 { theme.header } else { theme.text };
        draw_text(back, w, h, x0 + pad, ty, scale, line, color);
    }
}

/// `<mod_key>+D` launcher modal: query box + top matches, same font/box
/// style as the help overlay. Selected row gets a highlight bar.
fn draw_launcher_overlay(launcher: &Launcher, mod_key: veil_config::ModKey, theme: &veil_config::Theme, back: &mut [u8], w: u32, h: u32) {
    use crate::font5x7::{draw_text, fill_rect, GLYPH_H, GLYPH_W};

    const PAGE_SIZE: usize = 10;
    let scale = 2u32;
    let advance = (GLYPH_W + 1) * scale;
    let line_h = (GLYPH_H + 3) * scale;
    let pad = 12i32;

    let mod_label = mod_key.label().to_ascii_uppercase();
    let matches = launcher.matches();
    let selected = launcher.selected.min(matches.len().saturating_sub(1));

    let scroll_offset = if selected < PAGE_SIZE {
        0
    } else {
        selected + 1 - PAGE_SIZE
    };

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
        if scroll_offset > 0 {
            lines.push(format!("^ {} MORE ABOVE", scroll_offset));
        }
        let end_idx = (scroll_offset + PAGE_SIZE).min(matches.len());
        for m in &matches[scroll_offset..end_idx] {
            lines.push(m.name.clone());
        }
        if end_idx < matches.len() {
            lines.push(format!("v {} MORE BELOW", matches.len() - end_idx));
        }
    }

    let text_cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(40) as u32;
    let box_w = text_cols * advance + pad as u32 * 2;
    let box_h = lines.len() as u32 * line_h + pad as u32 * 2;
    let x0 = ((w as i32 - box_w as i32) / 2).max(0);
    let y0 = ((h as i32 - box_h as i32) / 2).max(0);

    let border = 2i32;
    fill_rect(back, w, h, x0 - border, y0 - border, box_w + (border as u32 * 2), box_h + (border as u32 * 2), theme.border);
    fill_rect(back, w, h, x0, y0, box_w, box_h, theme.panel_bg);

    // Highlight bar behind the selected match row
    if !matches.is_empty() {
        let relative_selected = selected - scroll_offset;
        let above_indicator = if scroll_offset > 0 { 1 } else { 0 };
        let row = header_rows + above_indicator + relative_selected;
        let ry = y0 + pad + row as i32 * line_h as i32 - 2;
        fill_rect(back, w, h, x0 + 2, ry, box_w - 4, line_h, theme.highlight);
    }

    for (i, line) in lines.iter().enumerate() {
        let ty = y0 + pad + i as i32 * line_h as i32;
        let color = if i == 0 {
            theme.header // header row
        } else if i == 1 {
            theme.accent // query text
        } else if line.starts_with('^') || line.starts_with('v') {
            theme.text_dim // scroll indicator
        } else {
            theme.text
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

    // Floating (override-redirect) windows first — they're painted on top of
    // everything in composite(), so they must win hit-testing too, or a
    // dropdown/menu would be unclickable over the tiled window beneath it.
    // Last-mapped wins on overlap, same "last wins" convention as popups below.
    for f in state.floating.iter().rev().filter(|f| f.alive()) {
        let Some(surf) = f.wl_surface() else { continue };
        let g = f.geometry();
        if xi >= g.loc.x && yi >= g.loc.y
            && xi < g.loc.x + g.size.w && yi < g.loc.y + g.size.h
        {
            return Some((surf, (g.loc.x as f64, g.loc.y as f64).into()));
        }
    }

    // While something's fullscreen, it's the only tiled thing clickable —
    // everything else keeps its normal rect (see relayout()) but is
    // visually covered (see composite()), so hit-testing it would let
    // clicks fall through to a window you can't actually see.
    let ws = state.active_workspace;
    if let Some(fs) = &state.fullscreen {
        return state.toplevels.iter().filter(|t| t.alive() && t.workspace == ws)
            .find(|t| t.wl_surface().as_ref() == Some(fs))
            .and_then(|t| t.wl_surface())
            .map(|surf| (surf, (0.0, 0.0).into()));
    }

    // Live toplevels paired with their tiled rect, topmost (last) first.
    // Option preserves index alignment with layout_rects (see composite()).
    // Workspace-filtered for the same reason composite()'s copy is — must
    // match the exact sequence relayout() used to build layout_rects.
    let live: Vec<Option<WlSurface>> = state.toplevels.iter()
        .filter(|t| t.alive() && t.workspace == ws)
        .map(|t| t.wl_surface())
        .collect();
    for (i, root_opt) in live.iter().enumerate().rev() {
        let Some(root) = root_opt else { continue };
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

                    // Multimedia keys (volume, brightness)
                    let sym = keysym.modified_sym().raw();
                    match sym {
                        keysyms::KEY_XF86AudioRaiseVolume => {
                            let (body, pct) = adjust_volume(true);
                            st.show_osd("VOLUME", body, pct, Duration::from_secs(2));
                            return FilterResult::Intercept(());
                        }
                        keysyms::KEY_XF86AudioLowerVolume => {
                            let (body, pct) = adjust_volume(false);
                            st.show_osd("VOLUME", body, pct, Duration::from_secs(2));
                            return FilterResult::Intercept(());
                        }
                        keysyms::KEY_XF86AudioMute => {
                            let (body, pct) = toggle_volume_mute();
                            st.show_osd("VOLUME", body, pct, Duration::from_secs(2));
                            return FilterResult::Intercept(());
                        }
                        keysyms::KEY_XF86MonBrightnessUp => {
                            let (body, pct) = adjust_brightness(true);
                            st.show_osd("BRIGHTNESS", body, pct, Duration::from_secs(2));
                            return FilterResult::Intercept(());
                        }
                        keysyms::KEY_XF86MonBrightnessDown => {
                            let (body, pct) = adjust_brightness(false);
                            st.show_osd("BRIGHTNESS", body, pct, Duration::from_secs(2));
                            return FilterResult::Intercept(());
                        }
                        _ => {}
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
                            mark_dirty_full(st);
                            return FilterResult::Intercept(());
                        }
                        if ch == 'd' {
                            st.launcher = Some(Launcher::new());
                            mark_dirty_full(st);
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
            mark_dirty_full(state);

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
            const BTN_LEFT: u32 = 0x110;

            // Bar app tile — launch and stop here. A bar click has no
            // client surface behind it to focus or forward the event to,
            // so it takes a completely separate path from the click-to-
            // focus/forwarding below rather than falling through into it.
            let bar_hit = if pressed && button == BTN_LEFT {
                let (px, py) = (state.pointer_pos.0 as i32, state.pointer_pos.1 as i32);
                state.bar_hitboxes.iter()
                    .find(|(r, _)| px >= r.x && py >= r.y && px < r.x + r.w as i32 && py < r.y + r.h as i32)
                    .map(|(_, exec)| exec.clone())
            } else {
                None
            };

            if let Some(exec) = bar_hit {
                spawn_command(&state.socket_name, &exec);
                return;
            }

            // Left-click on a tile makes it the focused one — just follows
            // click focus, no position rearranging (that's what Alt+S is
            // for). Keeps `layout.focused` in sync with clicks so keyboard
            // nav/close (Alt+H/J/K/L/Q) act on whatever you last clicked,
            // not whatever keyboard nav last visited. A click inside the
            // already-focused tile hits `idx == layout.focused` and no-ops,
            // so ordinary clicks/typing inside an app are unaffected.
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
            if let Some(l) = state.launcher.as_mut() {
                let matches_len = l.matches().len();
                if matches_len > 0 {
                    if v120 < 0 {
                        l.selected = (l.selected + 1).min(matches_len - 1);
                    } else if v120 > 0 {
                        l.selected = l.selected.saturating_sub(1);
                    }
                    mark_dirty_full(state);
                    return;
                }
            }

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

        InputCmd::Osd { title, body, progress } => {
            state.show_osd(title, body, progress, Duration::from_secs(2));
        }
    }
}

// ─── Run loop ─────────────────────────────────────────────────────────────────

/// Bundle of state passed through calloop callbacks.
pub struct LoopData {
    pub state:   State,
    pub display: Display<State>,
}

/// Shared by unmapped_window and destroyed_window — X11 can in some edge
/// cases destroy a window without a prior unmap, so both call this and it's
/// idempotent (retain on an already-removed entry is a no-op). Checks both
/// lists since we don't always know which one a given window landed in by
/// the time an unmap/destroy notification arrives.
fn remove_x11_window(state: &mut State, window: &X11Surface) {
    if let Some(wl) = window.wl_surface() {
        if state.fullscreen.as_ref() == Some(&wl) {
            state.fullscreen = None;
        }
    }

    let before = state.toplevels.len();
    state.toplevels.retain(|w| !matches!(&w.surface, WindowSurface::X11(x) if x == window));
    if state.toplevels.len() != before {
        relayout(state);
        refocus_keyboard(state);
    }

    let before = state.floating.len();
    state.floating.retain(|f| f != window);
    if state.floating.len() != before {
        mark_dirty_full(state);
    }
}

// X11Wm::start_wm's D type parameter is tied to whatever LoopHandle it's
// given — ours is LoopHandle<'static, LoopData>, so these two handler traits
// have to live on LoopData, not State (unlike every other handler in this
// file, which only ever needs &mut State via Display<State>'s dispatch).
// Both just forward into the same State fields/logic everything else uses.
// delegate_xwayland_shell!(State)'s generated Dispatch impls (see smithay's
// wayland::xwayland_shell) require D: XwmHandler where D=State — that's the
// piece the first build caught (State: XwmHandler wasn't satisfied). So the
// real logic has to live here, on State, not just on LoopData. LoopData
// still needs its own impl too — X11Wm::start_wm<D>'s D is tied to whatever
// LoopHandle it's given, and ours is LoopHandle<'static, LoopData> — so
// LoopData's impl below is a thin forward into these.
impl XwmHandler for State {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("XwmHandler called before X11Wm started")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    // Nothing to do before it's actually mapped — X11 lets a client create a
    // window well before showing it, same as new_window.
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_mapped(true);
        let wl = window.wl_surface();
        self.toplevels.push(Window::new(WindowSurface::X11(window), self.active_workspace));
        let ws = self.active_workspace;
        let n = self.toplevels.iter().filter(|t| t.alive() && t.workspace == ws).count();
        self.layout.focused = n.saturating_sub(1);
        // Only if already paired with a wl_surface (usually is, by this
        // point) — if not yet, it'll pick up focus on the next natural
        // refocus_keyboard call (prune tick, next window open/close, etc).
        if let Some(wl) = wl {
            let serial = self.next_serial();
            let kb = self.keyboard.clone();
            kb.set_focus(self, Some(wl), serial);
        }
        relayout(self);
    }

    // Override-redirect windows (menus, tooltips, Steam's own popups) bypass
    // the WM entirely and position themselves absolutely — unlike
    // map_window_request, there's no "please map me" round-trip to answer;
    // the X server has already mapped it by the time this notification
    // arrives, so this is purely "start painting it". It goes in `floating`,
    // not `toplevels`: never tiled, never gets keyboard focus via the
    // tab/swap cycle, painted topmost every composite (see composite()),
    // hit-tested first (see pick_focus()).
    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.floating.push(window);
        mark_dirty_full(self);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
        remove_x11_window(self, &window);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        remove_x11_window(self, &window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let mut geo = window.geometry();
        let or = window.is_override_redirect();
        if or {
            // Floating windows own their geometry entirely — this is how a
            // dropdown/menu ends up positioned where the client wants it.
            if let Some(x) = x { geo.loc.x = x; }
            if let Some(y) = y { geo.loc.y = y; }
        }
        // Tiling WM: a *managed* window's position is always ours, never
        // the client's to set — x/y ignored for those (fall through, no
        // loc change above). Size honored either way, immediately, rather
        // than making the client wait for the next relayout tick (matches
        // smithay's own anvil reference).
        if let Some(w) = w { geo.size.w = w as i32; }
        if let Some(h) = h { geo.size.h = h as i32; }

        // Heuristic for games/apps that go fullscreen by directly requesting
        // output-sized geometry instead of the proper EWMH
        // _NET_WM_STATE_FULLSCREEN path (fullscreen_request below) — same
        // trick i3 uses for legacy clients. Without this, a managed window
        // gets the size it asked for but keeps its old tiled x/y (position
        // is never honored for managed windows, see above), so it renders
        // output-sized but offset — visibly "half on screen".
        if !or && w == Some(self.output_w) && h == Some(self.output_h) {
            if let Some(wl) = window.wl_surface() {
                let ws = self.active_workspace;
                if let Some(i) = self.toplevels.iter().filter(|t| t.alive() && t.workspace == ws)
                    .position(|t| t.wl_surface().as_ref() == Some(&wl))
                {
                    self.layout.focused = i;
                }
                self.fullscreen = Some(wl);
                relayout(self);
                refocus_keyboard(self);
                return; // relayout already configured this window correctly
            }
        }

        let _ = window.configure(geo);
        if or {
            mark_dirty_full(self);
        }
    }

    // Only override-redirect windows can really trigger self-moves under a
    // real WM (a managed window's geometry is ours, set via configure_size).
    // smithay updates X11Surface's internal geometry before this fires, so
    // composite()'s window.geometry() call already sees the new position —
    // this just needs to trigger the repaint.
    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<X11Window>,
    ) {
        mark_dirty_full(self);
    }

    // Tiling WM: geometry is always ours, never interactive. Declining these
    // is the philosophically correct answer here, same as i3/sway would.
    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _edges: ResizeEdge) {}
    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(wl) = window.wl_surface() else { return };
        let ws = self.active_workspace;
        if let Some(i) = self.toplevels.iter().filter(|t| t.alive() && t.workspace == ws)
            .position(|t| t.wl_surface().as_ref() == Some(&wl))
        {
            self.layout.focused = i;
        }
        self.fullscreen = Some(wl);
        relayout(self);
        refocus_keyboard(self);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(wl) = window.wl_surface() {
            if self.fullscreen.as_ref() == Some(&wl) {
                self.fullscreen = None;
                relayout(self);
            }
        }
    }

    // Selection (clipboard) passthrough for X11 apps — deferred; defaults
    // (allow_selection_access -> false) are safe, just means no X11<->Wayland
    // clipboard bridging yet.
}

impl XWaylandShellHandler for LoopData {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        self.state.xwayland_shell_state()
    }
}

impl XwmHandler for LoopData {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm { self.state.xwm_state(xwm) }
    fn new_window(&mut self, xwm: XwmId, window: X11Surface) { self.state.new_window(xwm, window) }
    fn new_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_override_redirect_window(xwm, window)
    }
    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.map_window_request(xwm, window)
    }
    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.mapped_override_redirect_window(xwm, window)
    }
    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) { self.state.unmapped_window(xwm, window) }
    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) { self.state.destroyed_window(xwm, window) }
    fn configure_request(
        &mut self, xwm: XwmId, window: X11Surface,
        x: Option<i32>, y: Option<i32>, w: Option<u32>, h: Option<u32>, reorder: Option<Reorder>,
    ) {
        self.state.configure_request(xwm, window, x, y, w, h, reorder)
    }
    fn configure_notify(
        &mut self, xwm: XwmId, window: X11Surface, geometry: Rectangle<i32, Logical>, above: Option<X11Window>,
    ) {
        self.state.configure_notify(xwm, window, geometry, above)
    }
    fn resize_request(&mut self, xwm: XwmId, window: X11Surface, button: u32, edges: ResizeEdge) {
        XwmHandler::resize_request(&mut self.state, xwm, window, button, edges)
    }
    fn move_request(&mut self, xwm: XwmId, window: X11Surface, button: u32) {
        XwmHandler::move_request(&mut self.state, xwm, window, button)
    }
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
    theme: veil_config::Theme,
    bar: veil_config::BarConfig,
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
    // Probe once, here, purely to learn what the render node can import —
    // that result gets baked into the dmabuf feedback below so feedback-aware
    // clients (niri, Hyprland, GL/Vulkan apps) still know to send us tiled
    // buffers. The actual EGL/GLES context (the expensive part — Mesa
    // driver-side caches, shader compiler state) is dropped right after and
    // only stood back up lazily, in State::gpu_lazy(), on the first commit
    // that actually hands us a non-linear buffer. Sessions that never do
    // (plain shm clients, XWayland apps, linear-only GL apps) never pay for
    // a live context at all.
    let gpu_probe = GpuImporter::new();
    let gpu_available = gpu_probe.is_some();

    // Explicit sync (linux-drm-syncobj-v1): a second, independent fd on the
    // same render node used only to import client syncobj timelines and
    // wait/signal fences — deliberately separate from `gpu`'s EGL/GBM device
    // so this never touches the working detile path. Decoupled from `gpu`
    // being `Some` too: syncobj import doesn't need EGL, just kernel
    // CONFIG_DRM_SYNCOBJ support on the node, so this still comes up on
    // (rare) hardware where EGL fails but the render node itself is fine.
    let syncobj_state = crate::detile::open_render_node().and_then(|fd| {
        let dev = DrmDeviceFd::new(DeviceFd::from(fd));
        if supports_syncobj_eventfd(&dev) {
            Some(DrmSyncobjState::new::<State>(&dh, dev))
        } else {
            eprintln!("[veil-host] syncobj: device doesn't support syncobj_eventfd, explicit sync disabled");
            None
        }
    });

    // v4 dmabuf with default feedback: advertise the render device + the formats
    // we can import (linear always; tiled too when the GPU importer is up).
    // Feedback-aware clients allocate accordingly; we detile tiled buffers.
    let mut dmabuf_state  = DmabufState::new();
    let dmabuf_feedback   = build_dmabuf_feedback(&gpu_probe);
    drop(gpu_probe); // real context recreated lazily by gpu_lazy() when actually needed
    let _dmabuf_global    = dmabuf_state.create_global_with_default_feedback::<State>(&dh, &dmabuf_feedback);
    // XWayland pairing: lets an X11 window's wl_surface get associated with
    // its X11Surface (see XWaylandShellHandler::surface_associated). Only
    // XWayland clients can bind this global.
    let xwayland_shell_state = XWaylandShellState::new::<State>(&dh);
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

    let config_path = veil_config::config_path();
    let config_mtime = config_path.as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());

    let state = State {
        compositor_state, xdg_shell_state, shm_state, seat_state,
        xdg_activation, output_manager,
        dmabuf_state, _dmabuf_global,
        gpu:                  None, // stood up lazily by gpu_lazy() on first non-linear commit
        gpu_available,
        syncobj_state,
        _data_device,
        _xdg_decoration, _viewporter, _fractional, _presentation,
        _text_input, _primary_sel, _cursor_shape,
        _pointer_constraints, _relative_pointer, _idle_inhibit, _kb_inhibit, _tablet,
        seat, keyboard, pointer,
        output,
        output_w: width, output_h: height,
        pointer_pos: (0.0, 0.0),
        toplevels: Vec::new(),
        fullscreen: None,
        floating: Vec::new(),
        xwm: None,
        xwayland_shell_state,
        layout: Layout::default(),
        workspace_layouts: [Layout::default(); veil_config::WORKSPACE_COUNT as usize],
        active_workspace: 0,
        layout_rects: Vec::new(),
        keybinds,
        show_help: false,
        background: [background[0], background[1], background[2], 255],
        theme,
        bar,
        bar_hitboxes: Vec::new(),
        // No anchor client to spawn into (`veil-host start`) → open straight
        // to the launcher instead of an empty screen with no hint of what to press.
        launcher: if spawn.is_none() { Some(Launcher::new()) } else { None },
        socket_name: socket_name.to_string(),
        popups:           PopupManager::default(),
        surface_buffers:  HashMap::new(),
        cursor_status:    CursorImageStatus::default_named(),
        dirty:            false,
        damage:           None,
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
        prev_frame:           None,
        config_path,
        config_mtime,
        last_config_check: Instant::now(),
        composite_interval,
        osd: None,
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
        Ok((xwayland, x_client)) => {
            let xd = xwayland_display.clone();
            let wm_handle = handle.clone();
            // Client isn't Clone-relied-on here — Option::take() means this
            // works regardless, and degrades safely if Ready somehow fired
            // more than once (it shouldn't).
            let mut x_client = Some(x_client);
            handle.insert_source(xwayland, move |event, _, data| {
                match event {
                    XWaylandEvent::Ready { x11_socket, display_number } => {
                        tracing::info!("XWayland ready on DISPLAY=:{display_number}");
                        *xd.lock().unwrap() = Some(display_number);
                        std::env::set_var("DISPLAY", format!(":{display_number}"));
                        match x_client.take() {
                            Some(client) => match X11Wm::start_wm(wm_handle.clone(), x11_socket, client) {
                                Ok(wm) => {
                                    tracing::info!("X11 window manager started");
                                    data.state.xwm = Some(wm);
                                }
                                Err(e) => tracing::error!("X11Wm::start_wm failed: {e} — X11 apps will not work"),
                            },
                            None => tracing::warn!("XWaylandEvent::Ready fired more than once — ignoring"),
                        }
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
        // If the fullscreen window was among those just pruned, don't leave
        // `fullscreen` pointing at a dead surface — relayout() already
        // treats a not-found fullscreen surface as "nothing fullscreen", so
        // this is tidiness (no stale handle retained), not a correctness fix.
        if let Some(wl) = &data.state.fullscreen {
            if !data.state.toplevels.iter().any(|t| t.wl_surface().as_ref() == Some(wl)) {
                data.state.fullscreen = None;
            }
        }

        // Same backstop for floating (override-redirect) windows — normally
        // removed explicitly via unmapped_window/destroyed_window, this just
        // catches anything that slipped through without one firing.
        let before = data.state.floating.len();
        data.state.floating.retain(|f| f.alive());
        if data.state.floating.len() != before {
            mark_dirty_full(&mut data.state);
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

        // Auto-reload config if config.lua was created or modified on disk (checked every 1s).
        let now = Instant::now();
        if now.duration_since(data.state.last_config_check) >= Duration::from_secs(1) {
            data.state.last_config_check = now;
            let current_path = veil_config::config_path();
            let current_mtime = current_path.as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok());

            if current_path != data.state.config_path || current_mtime != data.state.config_mtime {
                reload_config(&mut data.state);
            }
        }

        // Composite all dirty surfaces into one RGBA frame and ship it.
        // Frame callbacks are fired inside composite_and_send after the frame is sent.
        composite_and_send(&mut data.state);

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
