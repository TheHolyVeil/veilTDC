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
    EventLoop, Interest, LoopHandle, Mode as CMode, PostAction,
};

use smithay::{
    backend::drm::DrmDeviceFd,
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_dmabuf,
    delegate_drm_syncobj, delegate_fractional_scale, delegate_idle_inhibit,
    delegate_keyboard_shortcuts_inhibit, delegate_output, delegate_pointer_constraints,
    delegate_presentation, delegate_primary_selection, delegate_relative_pointer, delegate_seat,
    delegate_shm, delegate_tablet_manager, delegate_text_input_manager, delegate_viewporter,
    delegate_xdg_activation, delegate_xdg_decoration, delegate_xdg_shell, delegate_xwayland_shell,
    desktop::{PopupKind, PopupManager},
    input::{
        keyboard::{
            keysyms, FilterResult, KeyboardHandle, KeysymHandle, ModifiersState, XkbConfig,
        },
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
            add_blocker, add_pre_commit_hook, with_states, with_surface_tree_downward,
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SubsurfaceCachedState, SurfaceAttributes, TraversalAction,
        },
        cursor_shape::CursorShapeManagerState,
        dmabuf::{
            get_dmabuf, DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler,
            DmabufState, ImportNotifier,
        },
        drm_syncobj::{
            supports_syncobj_eventfd, DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState,
        },
        fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState},
        idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState},
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState,
            KeyboardShortcutsInhibitor, KeyboardShortcutsInhibitorSeat,
        },
        output::{OutputHandler, OutputManagerState},
        pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState},
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        selection::data_device::{
            request_data_device_client_selection, set_data_device_focus, set_data_device_selection,
            ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
        },
        selection::primary_selection::{PrimarySelectionHandler, PrimarySelectionState},
        selection::{SelectionHandler, SelectionSource, SelectionTarget},
        shell::xdg::{
            decoration::{XdgDecorationHandler, XdgDecorationState},
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{with_buffer_contents, ShmHandler, ShmState},
        socket::ListeningSocketSource,
        tablet_manager::{TabletManagerState, TabletSeatHandler},
        text_input::{TextInputManagerState, TextInputSeat},
        viewporter::ViewporterState,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge, X11Window, XwmId},
        X11Surface, X11Wm, XWayland, XWaylandClientData, XWaylandEvent, XwmHandler,
    },
};
use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use wayland_protocols::xdg::shell::server::xdg_toplevel;

use smithay::backend::allocator::{
    dmabuf::{Dmabuf, DmabufMappingMode},
    Buffer as AllocBuffer, Fourcc, Modifier,
};

use crate::detile::GpuImporter;
use crate::launcher::Launcher;
use crate::layout::{Layout, Rect};
use crate::powermenu::{PowerAction, PowerMenu};
use crate::wallpaper_chooser::WallpaperChooser;
use crate::{input::InputCmd, sink::Frame};

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
    /// Which monitor this window was spawned on (index into `State.monitors`).
    /// "Dumb" multi-monitor: a window never migrates monitors on its own —
    /// only an explicit future move-to-monitor action would change this,
    /// same as `workspace` below only moves via move-to-workspace.
    pub monitor: usize,
    /// Which workspace this window is tiled on (0-indexed, 0..WORKSPACE_COUNT)
    /// — scoped to `monitor` above, not global; monitor 0's workspace 3 and
    /// monitor 1's workspace 3 are unrelated. Set at creation to whatever
    /// workspace was active on that monitor at the time; moved only by
    /// future move-to-workspace keybinds (none bound yet).
    pub workspace: u8,
}

impl Window {
    pub fn new(surface: WindowSurface, monitor: usize, workspace: u8) -> Self {
        Self {
            surface,
            monitor,
            workspace,
        }
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
            WindowSurface::X11(x) => {
                let _ = x.close();
            }
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

/// One monitor's worth of independent compositor state — its own
/// workspaces, tiling, composite buffer, damage tracking, bar hitboxes.
/// "Dumb" multi-monitor (see MULTI_MONITOR_SCOPE.md): nothing here is ever
/// shared or synced across monitors except by explicit user action (a
/// future move-to-monitor keybind). Populated from real display detection
/// via `InputCmd::SetMonitors`; which monitor is "active" is resolved from
/// the shared `State.pointer_global` against each monitor's `rect` — see
/// `active_monitor_idx`.
pub struct Monitor {
    /// Position + size in the shared *virtual* arrangement space, side by
    /// side left-to-right — used only for pointer routing (which monitor
    /// the cursor is over). Set by `InputCmd::SetMonitors`.
    pub rect: Rect,
    pub output_w: u32,
    pub output_h: u32,
    /// LOCAL to this monitor (0,0 origin) — everything that reads this
    /// (click math, cursor blit, pick_focus) already only ever deals in
    /// monitor-local coordinates, so keeping it local here means none of
    /// that code needs to change across the whole multi-monitor project.
    pub pointer_pos: (f64, f64),
    pub fullscreen: Option<WlSurface>,
    pub layout: Layout,
    pub workspace_layouts: [Layout; veil_config::WORKSPACE_COUNT as usize],
    pub active_workspace: u8,
    pub layout_rects: Vec<Rect>,
    pub bar_hitboxes: Vec<(Rect, String)>,
    pub composite_buf: Vec<u8>,
    /// Pre-scaled wallpaper layer (RGBA, exactly `output_w * output_h * 4`),
    /// copied over the buffer instead of the solid background fill.
    pub wallpaper: Option<Vec<u8>>,
    pub damage: Option<Rect>,
    pub last_composite: Option<Instant>,
    /// The Arc we handed to frame_tx last time for THIS monitor. See the
    /// field doc on the old flat `State.prev_frame` — same reclaim trick,
    /// now one slot per monitor since each sends its own Frame.
    pub prev_frame: Option<Arc<Vec<u8>>>,
}

impl Monitor {
    /// A monitor's `pointer_pos`, `layout`, and `layout_rects` all start
    /// empty/default; workspace 0 (workspace "1") starts active, same as
    /// the old flat `State` defaults did.
    fn new(width: u32, height: u32, origin_x: i32) -> Self {
        Self {
            rect: Rect {
                x: origin_x,
                y: 0,
                w: width,
                h: height,
            },
            output_w: width,
            output_h: height,
            pointer_pos: (0.0, 0.0),
            fullscreen: None,
            layout: Layout::default(),
            workspace_layouts: Default::default(),
            active_workspace: 0,
            layout_rects: Vec::new(),
            bar_hitboxes: Vec::new(),
            composite_buf: Vec::with_capacity((width as usize) * (height as usize) * 4),
            wallpaper: None,
            damage: None,
            last_composite: None,
            prev_frame: None,
        }
    }
}

pub struct State {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub xdg_activation: XdgActivationState,
    pub output_manager: OutputManagerState,
    pub dmabuf_state: DmabufState,
    pub _dmabuf_global: Option<DmabufGlobal>,
    /// GPU dmabuf importer for tiled/non-linear client buffers. `None` when no
    /// render node / EGL is available — veil then stays CPU-only + linear-only.
    pub gpu: Option<GpuImporter>,
    /// Whether a DRM render node was found and imports work, per the startup
    /// probe — governs whether gpu_lazy() will attempt (re)creation at all.
    pub gpu_available: bool,
    /// Whether a DRM render node was found and imports work, per the startup
    /// probe — governs whether gpu_lazy() will attempt (re)creation at all.
    /// `linux-drm-syncobj-v1` explicit sync. `None` when the render node's
    /// kernel/driver doesn't support `syncobj_eventfd` — commit() then skips
    /// the acquire-fence wait entirely and relies on implicit (kernel dma-buf)
    /// sync only, same behavior as before this existed.
    pub syncobj_state: Option<DrmSyncobjState>,
    pub _data_device: DataDeviceState,
    pub _xdg_decoration: XdgDecorationState,
    pub _viewporter: ViewporterState,
    pub _fractional: FractionalScaleManagerState,
    pub _presentation: PresentationState,
    pub _text_input: TextInputManagerState,
    pub _primary_sel: PrimarySelectionState,
    pub _cursor_shape: CursorShapeManagerState,
    pub _pointer_constraints: PointerConstraintsState,
    pub _relative_pointer: RelativePointerManagerState,
    pub _idle_inhibit: IdleInhibitManagerState,
    pub _kb_inhibit: KeyboardShortcutsInhibitState,
    pub _tablet: TabletManagerState,
    pub seat: Seat<Self>,
    pub keyboard: KeyboardHandle<Self>,
    pub pointer: PointerHandle<Self>,
    pub output: Output,
    /// One entry per physical display, side by side left-to-right in
    /// detection order (see `InputCmd::SetMonitors`). `monitors[0]` is what
    /// the old flat `output_w`/`output_h`/`pointer_pos`/`fullscreen`/
    /// `layout`/`workspace_layouts`/`active_workspace`/`layout_rects`/
    /// `bar_hitboxes`/`composite_buf`/`damage`/`last_composite`/
    /// `prev_frame` fields used to be. See `Monitor` and
    /// `active_monitor_idx`. Starts as exactly one entry (a startup guess,
    /// terminal-mode-shaped) and gets rebuilt to match reality once
    /// `SetMonitors` arrives with real sizes from output detection.
    pub monitors: Vec<Monitor>,
    /// Pointer position in the shared virtual arrangement space that spans
    /// every monitor's `rect` — NOT any single monitor's local coordinates
    /// (see `Monitor::pointer_pos` for that). The only thing every monitor
    /// shares: "where is the cursor right now, across the whole
    /// arrangement." `active_monitor_idx` resolves which monitor that
    /// falls inside; `InputCmd::PointerMotionAbs` is what updates it.
    pub pointer_global: (f64, f64),
    pub toplevels: Vec<Window>,
    /// Override-redirect X11 windows (menus, tooltips, Steam's own popups) —
    /// unmanaged, positioned absolutely by the client itself, never tiled.
    /// Painted last (topmost) every composite. See mapped_override_redirect_window.
    pub floating: Vec<X11Surface>,
    /// X11 window manager for the XWayland connection — `None` until
    /// XWaylandEvent::Ready fires and start_wm succeeds (or if Xwayland
    /// isn't installed).
    pub xwm: Option<X11Wm>,
    pub xwayland_shell_state: XWaylandShellState,
    /// Parsed `keybinds` config (Combo 4). Super+/ (hardcoded, not itself
    /// configurable) toggles `show_help`.
    pub keybinds: veil_config::Keybinds,
    pub show_help: bool,
    /// Bare background color (RGBA, alpha always 255) — fills the composite
    /// buffer before any window blit, so uncovered space is a solid color
    /// instead of black.
    pub background: [u8; 4],
    /// Resolved color set for launcher/help/sidebar chrome — see
    /// `veil_config::Theme`. Swapped wholesale on `reload_config`.
    pub theme: veil_config::Theme,
    pub bar: veil_config::BarConfig,
    /// `<mod_key>+D` app launcher — `Some` while the modal is open. See
    /// `crate::launcher`. Not itself a `keybinds` config entry, same as help.
    pub launcher: Option<Launcher>,
    /// `<mod_key>+P` power menu — `Some` while the modal is open. Same
    /// intercept-all-input shape as `launcher`, see `crate::powermenu`.
    pub power_menu: Option<PowerMenu>,
    /// `<mod_key>+Y` (configurable via `keybinds.wallpaper_chooser`) wallpaper
    /// chooser — `Some` while the modal is open. Shows a thumbnail grid of
    /// every file the wallpaper engine knows about. Enter jumps to the selected
    /// image without resetting the shuffle rotation timer.
    pub wallpaper_chooser: Option<WallpaperChooser>,
    /// Volume/brightness helpers run on their own thread (see `media.rs`).
    pub media: crate::media::MediaWorker,
    /// N6: evdev keycodes whose *press* we intercepted. The matching release
    /// is intercepted too, so a client never sees a release with no press.
    pub suppressed_keys: std::collections::HashSet<u32>,
    /// N7: set when we consumed a left-button press (bar tile, modal); the
    /// next left release is dropped for the same reason.
    pub swallow_left_release: bool,
    /// Own Wayland socket name, so launcher-spawned clients can connect back
    /// into us (`WAYLAND_DISPLAY=<this>`).
    pub socket_name: String,
    pub popups: PopupManager,
    /// xdg_popups that asked for a grab (context menus, dropdowns), oldest
    /// first. A press that lands outside the popup's client dismisses them —
    /// see `dismiss_popup_grabs_on_press`.
    pub popup_grabs: Vec<PopupSurface>,
    pub surface_buffers: HashMap<ObjectId, SurfaceBuf>,
    /// Handle to the main calloop loop, set once the loop exists. The
    /// pre-commit hook in `new_surface` needs it to register an explicit-sync
    /// acquire-point source (S4); `None` before startup finishes, in which
    /// case commits simply aren't held for sync.
    pub loop_handle: Option<LoopHandle<'static, LoopData>>,
    pub cursor_status: CursorImageStatus,
    /// Cache for the last `pick_focus` result. Stores the surface, its origin
    /// in compositor space, and the tight bounding rect that was hit-tested.
    /// Invalidated when windows open/close/resize (relayout clears it) and
    /// on every tick where focus changes. If the cursor is still inside
    /// `focus_cache_rect` the tree walk is skipped entirely.
    pub focus_cache: Option<(
        WlSurface,
        smithay::utils::Point<f64, smithay::utils::Logical>,
        Rect,
    )>,
    pub dirty: bool,
    pub frame_tx: mpsc::Sender<Frame>,
    pub serial_counter: u32,
    pub frame_serial: u64,
    pub running: bool,
    /// Same `Arc<AtomicBool>` `run()` was handed for external shutdown
    /// (Ctrl-C/SIGTERM/`veil-host stop`) — Shift+Alt+E sets it from the
    /// inside too, same graceful-quit path either direction.
    pub stop: Arc<AtomicBool>,
    pub start_time: Instant,
    pub display_handle: DisplayHandle,
    pub host_clipboard: Option<String>,
    pub clipboard_rx: mpsc::Receiver<String>,
    pub pending_copy_out: bool,
    pub client_has_selection: bool,
    pub config_path: Option<std::path::PathBuf>,
    pub config_mtime: Option<std::time::SystemTime>,
    pub last_config_check: Instant,
    pub composite_interval: Duration,
    /// `fps` as configured (0 = follow the display); kept so the interval can
    /// be recomputed when the real refresh becomes known or config reloads.
    pub fps_cfg: u32,
    /// `clipboard_sync` from config.lua (see N9).
    pub clipboard_sync: bool,
    pub wallpaper: crate::wallpaper::WallpaperEngine,
    /// On-Screen Display (OSD) popup notification overlay.
    pub osd: Option<OsdNotification>,
}

/// Per-surface RGBA cache entry. We re-blit these every dirty tick.
pub struct SurfaceBuf {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

impl State {
    pub fn show_osd(
        &mut self,
        title: impl Into<String>,
        body: impl Into<String>,
        progress: Option<u8>,
        duration: Duration,
    ) {
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
    fn initialized(&self, _id: ClientId) {
        tracing::debug!("client connected");
    }
    fn disconnected(&self, _id: ClientId, _r: DisconnectReason) {
        tracing::debug!("client gone");
    }
}

// ─── Handler impls ────────────────────────────────────────────────────────────

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

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

    fn new_surface(&mut self, surface: &WlSurface) {
        // S4: explicit-sync acquire points become Smithay blockers. The hook
        // runs before the commit is applied: if the client attached an
        // acquire point, the commit is held (not blocking the event loop)
        // until the client's GPU signals it, then `blocker_cleared` releases
        // it. Replaces a 2 ms synchronous wait that gave up after one timeout
        // and then read unfinished buffers forever.
        add_pre_commit_hook::<Self, _>(surface, |state, _dh, surface| {
            let acquire = with_states(surface, |states| {
                let mut cached = states.cached_state.get::<DrmSyncobjCachedState>();
                cached.pending().acquire_point.clone()
            });
            let Some(acquire) = acquire else {
                return;
            };
            let (Some(handle), Some(client)) = (state.loop_handle.clone(), surface.client()) else {
                return;
            };
            let (blocker, source) = match acquire.generate_blocker() {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!("syncobj acquire blocker unavailable ({e}); not waiting");
                    return;
                }
            };
            let inserted = handle.insert_source(source, move |_, _, data| {
                let dh = data.display.handle();
                let cs = data.state.client_compositor_state(&client);
                cs.blocker_cleared(&mut data.state, &dh);
                Ok(())
            });
            if inserted.is_ok() {
                add_blocker(surface, blocker);
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Pull the newly-attached buffer (if any) and cache it as RGBA
        // keyed by surface id. We re-composite from all caches on tick.
        enum Assign {
            New(wl_buffer::WlBuffer),
            Removed,
            None,
        }
        let assign = with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            match guard.current().buffer.take() {
                Some(BufferAssignment::NewBuffer(b)) => Assign::New(b),
                Some(BufferAssignment::Removed) => Assign::Removed,
                None => Assign::None,
            }
        });

        match assign {
            Assign::New(buffer) => {
                // Explicit sync (linux-drm-syncobj-v1): the ACQUIRE wait no
                // longer happens here. S4: the old code blocked the whole
                // compositor for 2 ms per commit and, on the first timeout,
                // marked the surface "broken" forever and read its buffer
                // unfinished — but a heavy GPU client legitimately signals up
                // to a frame later. The acquire point is now a Smithay
                // blocker installed by the pre-commit hook in `new_surface`,
                // so this commit only runs once the client's GPU is done and
                // the event loop never stalls. The release point is still
                // signalled below once we've finished reading the buffer.
                let release_pt = with_states(surface, |states| {
                    let mut cached = states.cached_state.get::<DrmSyncobjCachedState>();
                    cached.current().release_point.clone()
                });

                // Reuse last frame's backing Vec for this surface instead of
                // allocating fresh every commit — same-size redraws (by far
                // the common case: video, animations, cursor blink) then cost
                // zero allocator churn instead of a alloc+free of the full
                // buffer every single commit. Falls back to an empty Vec
                // (first commit, or previous buffer type was GPU-detiled and
                // we don't reuse across that boundary — see note below).
                // S5: every conversion path leaves `dest` untouched when it
                // returns None (checks run before `out.clear()`, and the GPU
                // path only assigns on success), so on failure we can put the
                // previous frame straight back instead of making the window
                // vanish until the client sends a supported buffer.
                let old = self.surface_buffers.remove(&surface.id());
                let old_size = old.as_ref().map(|b| (b.w, b.h));
                let mut dest = old.map(|b| b.rgba).unwrap_or_default();

                // Try shm first, then dmabuf.
                let imported = with_buffer_contents(&buffer, |ptr, len, data| {
                    tracing::debug!(
                        "commit {} shm {}x{} fmt={:?}",
                        surface.id(),
                        data.width,
                        data.height,
                        data.format
                    );
                    let raw = unsafe { std::slice::from_raw_parts(ptr, len) };
                    shm_to_rgba_into(raw, &data, &mut dest)
                })
                .ok()
                .flatten()
                .or_else(|| {
                    let dmabuf = get_dmabuf(&buffer).ok()?;
                    tracing::debug!(
                        "commit {} dma {}x{} fmt={:?} mod={:?}",
                        surface.id(),
                        dmabuf.width(),
                        dmabuf.height(),
                        dmabuf.format().code,
                        dmabuf.format().modifier
                    );
                    // Linear → CPU mmap (fast, no GPU roundtrip). Anything else
                    // (tiled / implicit modifier) → GPU detile via EGLImage, if
                    // an importer is up; otherwise unsupported → blank.
                    if dmabuf.format().modifier == Modifier::Linear {
                        import_dmabuf_into(dmabuf, &mut dest)
                    } else if !surface_size_ok(dmabuf.width() as usize, dmabuf.height() as usize) {
                        None // S8: don't even start a GPU import for a monster buffer
                    } else if let Some(gpu) = self.gpu_lazy() {
                        // NOTE: GPU detile path still allocates fresh each
                        // call (untouched — didn't want to change detile.rs
                        // without checking its internals first). Reusable
                        // `dest` is dropped here; not a regression, just not
                        // the win the other two paths get.
                        gpu.import(dmabuf)
                            .filter(|(_, w, h)| surface_size_ok(*w as usize, *h as usize))
                            .map(|(rgba, w, h)| {
                                dest = rgba;
                                (w, h)
                            })
                    } else {
                        None
                    }
                });

                if let Some((w, h)) = imported {
                    tracing::debug!("commit {} → surface_buffers {}x{}", surface.id(), w, h);
                    self.surface_buffers
                        .insert(surface.id(), SurfaceBuf { rgba: dest, w, h });
                    match toplevel_rect_for(self, surface) {
                        Some((m, r)) => mark_dirty_rect(self, m, r),
                        None => mark_dirty_full(self), // subsurface/popup/cursor — no precise rect
                    }
                } else {
                    tracing::warn!(
                        "commit {} — unsupported buffer type, keeping previous frame",
                        surface.id()
                    );
                    if let Some((w, h)) = old_size {
                        self.surface_buffers
                            .insert(surface.id(), SurfaceBuf { rgba: dest, w, h });
                    }
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
                    Some((m, r)) => mark_dirty_rect(self, m, r),
                    None => mark_dirty_full(self),
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

        // S2: an xdg_popup must receive its initial configure before the
        // client will attach a buffer (Qt and GTK both wait for it), and the
        // client's first commit is the trigger. Nothing sent one, so context
        // menus / dropdowns never mapped. Mirrors Smithay's reference
        // compositor: send it once, from commit, after PopupManager has seen
        // the commit.
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface) {
            if !popup.is_initial_configure_sent() {
                if let Err(e) = popup.send_configure() {
                    tracing::warn!("popup initial configure failed: {e:?}");
                }
            }
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        if self.surface_buffers.remove(&surface.id()).is_some() {
            match toplevel_rect_for(self, surface) {
                Some((m, r)) => mark_dirty_rect(self, m, r),
                None => mark_dirty_full(self),
            }
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // We DON'T force Fullscreen — weston-terminal, thunar and friends gate
        // input/decoration on !Fullscreen. The tiled size is sent by relayout.
        let wl = surface.wl_surface().clone();
        // Tell the client which output it's on. Without wl_surface.enter a
        // client has no "current display": Chromium-family browsers then can't
        // resolve the display for fullscreen / scale and back out of it.
        self.output.enter(&wl);
        let serial = self.next_serial();
        let kb = self.keyboard.clone();
        kb.set_focus(self, Some(wl), serial);

        // New windows land on whichever monitor is active right now — the
        // dumb-multi-monitor placement rule (see MULTI_MONITOR_SCOPE.md):
        // "wherever the cursor is" decides both the monitor AND (below) the
        // workspace a new window is tagged with.
        let idx = active_monitor_idx(self);
        let ws = self.monitors[idx].active_workspace;
        self.toplevels
            .push(Window::new(WindowSurface::Xdg(surface), idx, ws));
        // New window takes focus; retile so every window gets its rect + size.
        let n = self
            .toplevels
            .iter()
            .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
            .count();
        self.monitors[idx].layout.focused = n.saturating_sub(1);
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
    fn grab(&mut self, surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // H9: record the grab so a press outside the popup's client can send
        // `popup_done` (see `dismiss_popup_grabs_on_press`). Smithay's own
        // PopupPointerGrab/PopupKeyboardGrab are deliberately not used: Veil
        // routes input through its own hit-testing and a plain `ptr.button`,
        // and those grabs would fight it. Keyboard focus stays on the parent
        // toplevel, which both Qt and GTK handle for menu navigation. The
        // seat/serial aren't validated — Veil has a single seat.
        self.popup_grabs.retain(|p| p.alive());
        self.popup_grabs.push(surface);
    }
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        // H9 (part): was an empty stub. A client that repositions a popup (GTK
        // and Qt do when a menu would overflow the screen) waits for the
        // `repositioned` event; answering it keeps the popup from hanging.
        surface.with_pending_state(|s| {
            s.geometry = positioner.get_geometry();
            s.positioner = positioner;
        });
        surface.send_repositioned(token);
        surface.send_pending_configure().ok();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        let wl = surface.wl_surface().clone();
        // The window's own monitor, not the one under the pointer (S10).
        let idx = window_monitor(self, &wl).unwrap_or_else(|| active_monitor_idx(self));
        tracing::info!(monitor = idx, active = active_monitor_idx(self), "xdg fullscreen_request");
        let ws = self.monitors[idx].active_workspace;
        if let Some(i) = self
            .toplevels
            .iter()
            .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
            .position(|t| t.wl_surface().as_ref() == Some(&wl))
        {
            self.monitors[idx].layout.focused = i;
        }
        self.monitors[idx].fullscreen = Some(wl);
        relayout_monitor(self, idx);
        refocus_keyboard(self);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        // Client-initiated, not keyboard-triggered — look up which monitor
        // actually has this window fullscreen and retile THAT monitor.
        let wl = surface.wl_surface().clone();
        tracing::info!("xdg unfullscreen_request (client asked)");
        if let Some(idx) = self
            .monitors
            .iter()
            .position(|m| m.fullscreen.as_ref() == Some(&wl))
        {
            self.monitors[idx].fullscreen = None;
            relayout_monitor(self, idx);
        }
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
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

/// S9: clipboard pipe I/O with a bound on how long a stalled peer can pin a
/// thread. Both directions used to block forever — a client that never reads
/// its paste fd (or never closes/writes the copy-out pipe) leaked a thread
/// per attempt. Each wait gives up after `CLIP_IO_TIMEOUT_MS` with no
/// progress, and copy-out is capped at `CLIP_MAX_BYTES` so a client can't make
/// us buffer without limit. SIGPIPE is ignored by Rust's runtime, so a closed
/// peer shows up as an EPIPE error here, not a signal.
const CLIP_IO_TIMEOUT_MS: i32 = 5000;
const CLIP_MAX_BYTES: usize = 16 * 1024 * 1024;

fn clip_set_nonblocking(fd: std::os::unix::io::RawFd) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
}

/// True if `fd` became ready (or hung up / errored — the caller's next
/// read/write reports which); false on timeout or a real poll failure.
fn clip_wait(fd: std::os::unix::io::RawFd, events: i16) -> bool {
    let mut p = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        let r = unsafe { libc::poll(&mut p, 1, CLIP_IO_TIMEOUT_MS) };
        if r > 0 {
            return true;
        }
        if r == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Write all of `data` to a client's paste fd, then close it (EOF tells the
/// client the paste is complete). Gives up quietly on a stalled/closed peer.
fn clip_write_bounded(fd: OwnedFd, data: &[u8]) {
    let raw = fd.as_raw_fd();
    clip_set_nonblocking(raw);
    let mut off = 0;
    while off < data.len() {
        let n = unsafe { libc::write(raw, data[off..].as_ptr().cast(), data.len() - off) };
        if n > 0 {
            off += n as usize;
            continue;
        }
        if n == 0 {
            return;
        }
        match std::io::Error::last_os_error().kind() {
            std::io::ErrorKind::WouldBlock => {
                if !clip_wait(raw, libc::POLLOUT) {
                    return;
                }
            }
            std::io::ErrorKind::Interrupted => {}
            _ => return,
        }
    }
}

/// Read a client's copy-out pipe to EOF. `None` on timeout, error, or if the
/// client sends more than `CLIP_MAX_BYTES`.
fn clip_read_bounded(fd: OwnedFd) -> Option<Vec<u8>> {
    let raw = fd.as_raw_fd();
    clip_set_nonblocking(raw);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = unsafe { libc::read(raw, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n > 0 {
            buf.extend_from_slice(&chunk[..n as usize]);
            if buf.len() > CLIP_MAX_BYTES {
                return None;
            }
            continue;
        }
        if n == 0 {
            return Some(buf);
        }
        match std::io::Error::last_os_error().kind() {
            std::io::ErrorKind::WouldBlock => {
                if !clip_wait(raw, libc::POLLIN) {
                    return None;
                }
            }
            std::io::ErrorKind::Interrupted => {}
            _ => return None,
        }
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        self.client_has_selection = source.is_some();
        if source.is_some() {
            // Schedule a deferred read: Smithay updates seat_data AFTER this callback returns,
            // so we request the data on the next tick when seat_data is current.
            self.pending_copy_out = true;
        }
    }

    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        // S9: only serve the mimes we actually advertised (see the
        // `set_data_device_selection` call in the paste-in path). Anything
        // else gets its fd dropped, i.e. an empty paste, instead of text
        // being handed over as an unrelated type.
        if mime_type != "text/plain;charset=utf-8" && mime_type != "text/plain" {
            return;
        }
        let Some(text) = self.host_clipboard.clone() else {
            return;
        };
        std::thread::spawn(move || clip_write_bounded(fd, text.as_bytes()));
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self._data_device
    }
}
impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

impl OutputHandler for State {}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self._primary_sel
    }
}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {}
    fn cursor_position_hint(
        &mut self,
        _: &WlSurface,
        _: &PointerHandle<Self>,
        _: Point<f64, Logical>,
    ) {
    }
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
    // N3: always granted, and honoured in the xkb filter: while the focused
    // surface holds an active inhibitor, Veil's own chords (Alt+Tab, the
    // keybinds) pass through to it, so VMs and remote-desktop clients get
    // them. Media keys, VT switching and the kill chord still work, and
    // Alt+Shift+Escape hands the shortcuts back (see `INHIBIT_ESCAPE`).
    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

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
                Fourcc::Argb8888
                    | Fourcc::Xrgb8888
                    | Fourcc::Abgr8888
                    | Fourcc::Xbgr8888
                    | Fourcc::Rgbx8888
                    | Fourcc::Bgrx8888
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
    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        _mode: zxdg_toplevel_decoration_v1::Mode,
    ) {
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
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // N1: this used to grant every request, which (a) let any background
        // client steal keyboard focus and (b) moved wl_keyboard focus without
        // touching `layout.focused`, so Close/Swap/Fullscreen then acted on a
        // different window than the one receiving keystrokes.
        //
        // Policy: honor only tokens created in response to real input (they
        // carry a seat serial) and fresh, and only for a live toplevel on the
        // active workspace of the active monitor — the same index space every
        // per-window action uses. Focus then goes through the normal
        // `layout.focused` → relayout → refocus_keyboard path, so the
        // keyboard focus and the keybind target can never diverge.
        // Denied requests are dropped silently, as the protocol allows.
        if data.serial.is_none() || data.timestamp.elapsed() > Duration::from_secs(10) {
            tracing::debug!("xdg_activation: denied (no input serial or stale token)");
            return;
        }
        let Some(mon) = self
            .toplevels
            .iter()
            .find(|t| t.alive() && t.wl_surface().as_ref() == Some(&surface))
            .map(|t| t.monitor)
        else {
            return;
        };
        if mon != active_monitor_idx(self) {
            return;
        }
        let ws = self.monitors[mon].active_workspace;
        let Some(i) = self
            .toplevels
            .iter()
            .filter(|t| t.alive() && t.monitor == mon && t.workspace == ws)
            .position(|t| t.wl_surface().as_ref() == Some(&surface))
        else {
            return; // window is on another workspace — don't yank focus there
        };
        self.monitors[mon].layout.focused = i;
        relayout(self);
        refocus_keyboard(self);
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
fn build_dmabuf_feedback(gpu: &Option<GpuImporter>) -> Option<DmabufFeedback> {
    use smithay::backend::allocator::Format;
    use std::os::unix::fs::MetadataExt;

    // Walk render nodes to find the first accessible one. dev_t tells clients
    // which GPU device to allocate on (must match the node it opens).
    let dev_t: libc::dev_t = (128..=135u32)
        .map(|n| format!("/dev/dri/renderD{n}"))
        .find_map(|path| std::fs::metadata(&path).ok().map(|m| m.rdev()))
        .unwrap_or(0);

    if dev_t == 0 {
        eprintln!("[veil-host] dmabuf: no DRM render node found, feedback dev_t=0");
    } else {
        eprintln!(
            "[veil-host] dmabuf feedback: dev_t={dev_t:#x} (renderD{})",
            (dev_t & 0xFF)
        );
    }

    // Always-importable linear formats (CPU mmap path).
    let mut formats: Vec<Format> = vec![
        Format {
            code: Fourcc::Argb8888,
            modifier: Modifier::Linear,
        },
        Format {
            code: Fourcc::Xrgb8888,
            modifier: Modifier::Linear,
        },
        Format {
            code: Fourcc::Abgr8888,
            modifier: Modifier::Linear,
        },
        Format {
            code: Fourcc::Xbgr8888,
            modifier: Modifier::Linear,
        },
        Format {
            code: Fourcc::Rgbx8888,
            modifier: Modifier::Linear,
        },
        Format {
            code: Fourcc::Bgrx8888,
            modifier: Modifier::Linear,
        },
    ];

    // Everything the render node can import (tiled modifiers included).
    if let Some(g) = gpu {
        for f in g.formats() {
            if !formats.contains(&f) {
                formats.push(f);
            }
        }
        eprintln!(
            "[veil-host] dmabuf feedback: {} formats (GPU detile enabled)",
            formats.len()
        );
    } else {
        eprintln!("[veil-host] dmabuf feedback: linear-only (no GPU importer)");
    }

    DmabufFeedbackBuilder::new(dev_t, formats)
        .build()
        .map_err(|e| eprintln!("[veil-host] dmabuf feedback build failed: {e} — dmabuf disabled, clients fall back to shm"))
        .ok()
}

/// S8: cap on client buffer size. A client picks width/height freely, and the
/// importers allocate `w*h*4` bytes and convert them on the compositor
/// thread, so an absurd buffer is a multi-hundred-MB allocation plus a
/// stalled main loop (and `blit`'s u32 index math overflows past ~2^30
/// pixels). 16384 per side covers any real output; the pixel cap (64 MP,
/// 256 MiB of RGBA — 8K x 8K) bounds the allocation and keeps `blit`'s u32
/// math comfortably in range. Over-limit buffers are rejected like any other
/// unsupported buffer, so S5 keeps the previous frame on screen.
const MAX_SURFACE_DIM: usize = 16384;
const MAX_SURFACE_PIXELS: usize = 64 * 1024 * 1024;

fn surface_size_ok(w: usize, h: usize) -> bool {
    w > 0
        && h > 0
        && w <= MAX_SURFACE_DIM
        && h <= MAX_SURFACE_DIM
        && w.saturating_mul(h) <= MAX_SURFACE_PIXELS
}

/// Import a linear dmabuf by mmapping plane 0 and converting pixels to RGBA.
/// Only single-plane ARGB/XRGB/ABGR/XBGR 8888 with linear layout are supported.
/// Writes into `out` in place instead of allocating — see shm_to_rgba_into.
fn import_dmabuf_into(dmabuf: &Dmabuf, out: &mut Vec<u8>) -> Option<(u32, u32)> {
    if dmabuf.num_planes() != 1 {
        return None;
    }
    let fmt = dmabuf.format();
    // Backstop: this is the CPU fast path — only ever mmap a LINEAR buffer.
    // Tiled / non-linear buffers go through the GPU detile path (commit routes
    // them to GpuImporter); a stray one reaching map_plane here would freeze the
    // compositor thread on an uncached/detiled CPU read, so bail.
    if fmt.modifier != Modifier::Linear {
        return None;
    }
    let w = dmabuf.width() as usize;
    let h = dmabuf.height() as usize;
    if !surface_size_ok(w, h) {
        return None; // S8: checked before map_plane so we never mmap a monster
    }
    let stride = dmabuf.strides().next()? as usize;
    let offset = dmabuf.offsets().next()? as usize;

    if stride < w * 4 {
        return None;
    }

    let mapping = dmabuf.map_plane(0, DmabufMappingMode::READ).ok()?;
    let raw = unsafe { std::slice::from_raw_parts(mapping.ptr() as *const u8, mapping.length()) };

    let pixel_data = raw.get(offset..)?;
    if pixel_data.len() < stride * h {
        return None;
    }

    // S5: resolve the pixel layout BEFORE `out.clear()` so every failure path
    // in this function leaves `out` untouched — the commit handler relies on
    // that to keep showing the previous frame when a new buffer is rejected.
    // Byte indices of (r, g, b) within each 4-byte pixel, plus whether the
    // format has a real alpha channel (S6: keep it, premultiplied, instead of
    // forcing 255 — X/x formats have padding in that byte, so stay opaque).
    let ([ri, gi, bi], has_alpha) = match fmt.code {
        // DRM stores as little-endian u32: ARGB8888 = B,G,R,A in memory
        Fourcc::Argb8888 => ([2, 1, 0], true),
        Fourcc::Xrgb8888 => ([2, 1, 0], false),
        Fourcc::Abgr8888 => ([0, 1, 2], true),
        Fourcc::Xbgr8888 => ([0, 1, 2], false),
        Fourcc::Rgbx8888 => ([3, 2, 1], false),
        Fourcc::Bgrx8888 => ([1, 2, 3], false),
        _ => return None,
    };

    out.clear();
    out.reserve(w * h * 4);
    for y in 0..h {
        let row = &pixel_data[y * stride..y * stride + w * 4];
        for px in row.as_chunks::<4>().0 {
            let a = if has_alpha { px[3] } else { 255 };
            out.extend_from_slice(&[px[ri], px[gi], px[bi], a]);
        }
    }
    Some((w as u32, h as u32))
}

/// Writes into `out` in place instead of allocating — `out.clear()` keeps
/// its existing heap capacity, so a same-size redraw (the common case)
/// reuses the same allocation instead of alloc+free every commit.
fn shm_to_rgba_into(
    raw: &[u8],
    data: &smithay::wayland::shm::BufferData,
    out: &mut Vec<u8>,
) -> Option<(u32, u32)> {
    // S7: every field here is a client-controlled i32. Reject negatives via
    // try_from (a bare `as usize` turns -1 into usize::MAX and sails past
    // the length check), and bounds-check with checked math that includes
    // `offset`, so a hostile or buggy client can't make the row slice below
    // panic and abort the compositor.
    let w = usize::try_from(data.width).ok()?;
    let h = usize::try_from(data.height).ok()?;
    let stride = usize::try_from(data.stride).ok()?;
    let offset = usize::try_from(data.offset).ok()?;
    let row_bytes = w.checked_mul(4)?;
    if w == 0 || h == 0 || stride < row_bytes || !surface_size_ok(w, h) {
        return None; // S8: size cap, checked before any allocation
    }
    let needed = (h - 1)
        .checked_mul(stride)?
        .checked_add(offset)?
        .checked_add(row_bytes)?;
    if raw.len() < needed {
        return None;
    }
    // Resolved once, not per pixel. Both layouts are 4 bytes/px; only the
    // R/B order differs. Unknown formats bail before `out` is touched.
    // S6: ARGB/ABGR carry real (premultiplied) alpha that we must keep —
    // forcing 255 turned every transparent pixel (GTK shadows, rounded
    // corners, popups) into an opaque black box. X formats have a padding
    // byte, not alpha, so those stay opaque.
    let (bgr, has_alpha) = match data.format {
        wl_shm::Format::Xbgr8888 => (false, false),
        wl_shm::Format::Abgr8888 => (false, true),
        wl_shm::Format::Xrgb8888 => (true, false),
        wl_shm::Format::Argb8888 => (true, true),
        _ => return None,
    };

    out.clear();
    out.reserve(row_bytes * h);
    for y in 0..h {
        let row_start = offset + y * stride;
        let row = &raw[row_start..row_start + row_bytes];
        for px in row.as_chunks::<4>().0 {
            let (r, g, b) = if bgr {
                (px[2], px[1], px[0])
            } else {
                (px[0], px[1], px[2])
            };
            let a = if has_alpha { px[3] } else { 255 };
            out.extend_from_slice(&[r, g, b, a]);
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
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for dy in y0..y1 {
        let sy = (dy - y) as u32;
        let drow = (dy as u32 * back_w * 4) as usize;
        let srow = (sy * src.w * 4) as usize;
        for dx in x0..x1 {
            let sx = (dx - x) as u32;
            let di = drow + (dx as u32 * 4) as usize;
            let si = srow + (sx * 4) as usize;
            let a = src.rgba[si + 3] as u32;
            if a == 255 {
                back[di..di + 4].copy_from_slice(&src.rgba[si..si + 4]);
            } else if a > 0 {
                // S6: Wayland ARGB/ABGR is PREMULTIPLIED, so the source color
                // already carries its alpha: out = src + dst * (1 - a).
                // The old straight-alpha form (src*a + dst*(1-a)) multiplied
                // by alpha twice and darkened every soft edge. `.min(255)`
                // guards clients that send color > alpha (invalid, but seen).
                let inv = 255 - a;
                back[di] = (src.rgba[si] as u32 + back[di] as u32 * inv / 255).min(255) as u8;
                back[di + 1] =
                    (src.rgba[si + 1] as u32 + back[di + 1] as u32 * inv / 255).min(255) as u8;
                back[di + 2] =
                    (src.rgba[si + 2] as u32 + back[di + 2] as u32 * inv / 255).min(255) as u8;
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
        if py < 0 || py as u32 >= back_h {
            continue;
        }
        for (dx, &ch) in row.iter().enumerate() {
            let px = x + dx as i32;
            if px < 0 || px as u32 >= back_w {
                continue;
            }
            let rgb = match ch {
                b'#' => Some([255u8, 255, 255]),
                b'.' => Some([0u8, 0, 0]),
                _ => None,
            };
            if let Some(c) = rgb {
                let i = ((py as u32 * back_w + px as u32) * 4) as usize;
                back[i] = c[0];
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
    back: &mut [u8],
    back_w: u32,
    back_h: u32,
    cache: &HashMap<ObjectId, SurfaceBuf>,
    root: &WlSurface,
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

/// N3: the active shortcuts inhibitor on the keyboard-focused surface, if any.
fn focused_active_inhibitor(st: &State) -> Option<KeyboardShortcutsInhibitor> {
    let focus = st.keyboard.current_focus()?;
    st.seat
        .keyboard_shortcuts_inhibitor_for_surface(&focus)
        .filter(|i| i.is_active())
}

/// While shortcuts are inhibited, is this the escape chord (Alt+Shift+Escape)
/// that takes them back? Pure, so it is tested.
fn is_inhibit_escape(sym: u32, alt: bool, shift: bool) -> bool {
    sym == smithay::input::keyboard::keysyms::KEY_Escape && alt && shift
}

/// Lowercased ASCII char for a keysym, ignoring Shift — so a configured
/// `"h"` bind matches whether or not the modifier chord also holds Shift.
fn keysym_char(keysym: KeysymHandle<'_>) -> Option<char> {
    let cp = smithay::input::keyboard::xkb::keysym_to_utf32(keysym.modified_sym());
    char::from_u32(cp).map(|c| c.to_ascii_lowercase())
}

/// The display number of the XWayland we started, or `XWAYLAND_UNSET`.
static XWAYLAND_DISPLAY: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(XWAYLAND_UNSET);
const XWAYLAND_UNSET: u32 = u32::MAX;

/// Unconditionally seed native Wayland flags so browsers (Helium, Chrome),
/// Electron apps, Firefox, Qt, GTK, and SDL apps run natively under Wayland
/// without requiring special command-line flags.
pub fn apply_wayland_env(cmd: &mut Command, socket_name: &str) {
    cmd.env("WAYLAND_DISPLAY", socket_name);
    // N12: X11 clients get our XWayland through the child's own environment.
    // Mutating the process environment (`set_var`) from the event loop raced
    // with other threads that read it.
    let xd = XWAYLAND_DISPLAY.load(std::sync::atomic::Ordering::Relaxed);
    if xd != XWAYLAND_UNSET {
        cmd.env("DISPLAY", format!(":{xd}"));
    }
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
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::error!("spawn {exec:?} failed: {e}"),
    }
}

fn draw_osd_overlay(osd: &OsdNotification, back: &mut [u8], w: u32, h: u32) {
    use crate::font5x7::{draw_text, fill_rect, GLYPH_H, GLYPH_W};

    let scale = 2u32;
    let advance = (GLYPH_W + 1) * scale;
    let line_h = (GLYPH_H + 3) * scale;
    let pad = 12i32;

    let title_line = osd.title.to_ascii_uppercase().to_string();
    let body_line = osd.body.to_ascii_uppercase().to_string();

    let has_bar = osd.progress.is_some();
    let bar_h = if has_bar { 14i32 } else { 0i32 };

    let mut lines = vec![title_line];
    if !body_line.is_empty() {
        lines.push(body_line);
    }

    let text_cols = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(18) as u32;
    let box_w = (text_cols * advance + pad as u32 * 2).max(220);
    let box_h =
        lines.len() as u32 * line_h + pad as u32 * 2 + if has_bar { bar_h as u32 } else { 0 };

    let x0 = ((w as i32 - box_w as i32) / 2).max(0);
    let y0 = (h as i32 / 12).max(10);

    let is_error = osd.title.contains("ERROR");
    let border_color = if is_error {
        [255, 85, 85, 255]
    } else {
        [128, 222, 234, 255]
    };

    let border = 2i32;
    fill_rect(
        back,
        w,
        h,
        x0 - border,
        y0 - border,
        box_w + (border as u32 * 2),
        box_h + (border as u32 * 2),
        border_color,
    );
    fill_rect(back, w, h, x0, y0, box_w, box_h, [10, 10, 20, 240]);

    for (i, line) in lines.iter().enumerate() {
        let ty = y0 + pad + i as i32 * line_h as i32;
        let color = if i == 0 {
            if is_error {
                [255, 100, 100, 255]
            } else {
                [255, 215, 0, 255]
            }
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

    state.config_mtime = path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());
    state.config_path = path.clone();

    state.keybinds = new_cfg.keybinds;
    state.background = [
        new_cfg.background[0],
        new_cfg.background[1],
        new_cfg.background[2],
        255,
    ];
    state.theme = new_cfg.theme;
    state.bar = new_cfg.bar;
    state.fps_cfg = new_cfg.fps;
    set_wallpaper_config(state, new_cfg.wallpaper.clone());
    let refresh_mhz = state.output.current_mode().map_or(60_000, |m| m.refresh as u32);
    state.composite_interval = composite_interval_for(new_cfg.fps, refresh_mhz);

    if let Some(p) = &path {
        let name = p
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("config.lua");
        state.show_osd(
            "CONFIG RELOADED",
            format!("Applied {name}"),
            None,
            Duration::from_secs(2),
        );
        eprintln!("[veil-host] reloaded config from {}", p.display());
    } else {
        state.show_osd(
            "CONFIG RELOADED",
            "Using default config",
            None,
            Duration::from_secs(2),
        );
        eprintln!("[veil-host] reloaded default config (no config.lua found)");
    }
    mark_dirty_full(state);
}

/// Which monitor is "active" right now — every keyboard-triggered action,
/// relayout, and dirty-mark resolves this fresh rather than trusting a
/// stored index. Matches the dumb model exactly: whichever monitor the
/// cursor is currently over IS the active one, full stop, no separate
/// concept of "focused monitor" that could drift out of sync with the
/// cursor.
///
/// Resolved fresh every call from `pointer_global` against each monitor's
/// `rect` (side-by-side virtual arrangement — see `InputCmd::SetMonitors`).
/// Falls back to monitor 0 if the point somehow lands outside every rect
/// (shouldn't happen — `pointer_global` is clamped to the combined
/// bounding box wherever it's written — but a safe default beats a panic
/// if some future caller ever writes an out-of-range value).
fn active_monitor_idx(state: &State) -> usize {
    let (gx, gy) = state.pointer_global;
    state
        .monitors
        .iter()
        .position(|m| m.rect.contains(gx as i32, gy as i32))
        .unwrap_or(0)
}

/// The monitor a toplevel lives on, by its root surface (`None` if unknown).
fn window_monitor(state: &State, surface: &WlSurface) -> Option<usize> {
    state
        .toplevels
        .iter()
        .find(|t| t.wl_surface().as_ref() == Some(surface))
        .map(|t| t.monitor)
        .filter(|&m| m < state.monitors.len())
}

/// Run a Combo-4 keybind action against the live layout, then re-tile and
/// re-focus so the client sees the result immediately.
/// Alt+Tab / Alt+Shift+Tab: advance `layout.focused` through live windows
/// on the active workspace, wrapping. Same filter + index space every other
/// per-window action (Close, ToggleFullscreen, Swap) already uses — no MRU
/// stack, just a straight cycle through `toplevels` order. `forward` false
/// means Alt+Shift+Tab (reverse).
fn cycle_focus(state: &mut State, forward: bool) {
    let idx = active_monitor_idx(state);
    let ws = state.monitors[idx].active_workspace;
    let n = state
        .toplevels
        .iter()
        .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
        .count();
    if n == 0 {
        return;
    }
    let focused = state.monitors[idx].layout.focused;
    state.monitors[idx].layout.focused = if forward {
        (focused + 1) % n
    } else {
        (focused + n - 1) % n
    };
    relayout(state);
    refocus_keyboard(state);
}

fn dispatch_action(state: &mut State, action: veil_config::Action) {
    use veil_config::Action::*;
    let idx = active_monitor_idx(state);
    let ws = state.monitors[idx].active_workspace; // pulled out once; every filter below needs it
    match action {
        FocusLeft => {
            let r = state.monitors[idx].layout_rects.clone();
            state.monitors[idx]
                .layout
                .focus(&r, crate::layout::Dir::Left);
        }
        FocusRight => {
            let r = state.monitors[idx].layout_rects.clone();
            state.monitors[idx]
                .layout
                .focus(&r, crate::layout::Dir::Right);
        }
        FocusUp => {
            let r = state.monitors[idx].layout_rects.clone();
            state.monitors[idx].layout.focus(&r, crate::layout::Dir::Up);
        }
        FocusDown => {
            let r = state.monitors[idx].layout_rects.clone();
            state.monitors[idx]
                .layout
                .focus(&r, crate::layout::Dir::Down);
        }
        Swap => {
            // swap_next's indices are positions among LIVE toplevels ON THIS
            // MONITOR'S ACTIVE WORKSPACE; map them back to real Vec indices
            // in case a dead-but-unpruned or other-workspace/monitor entry
            // sits between live ones.
            let live_idx: Vec<usize> = state
                .toplevels
                .iter()
                .enumerate()
                .filter(|(_, t)| t.alive() && t.monitor == idx && t.workspace == ws)
                .map(|(i, _)| i)
                .collect();
            if let Some((a, b)) = state.monitors[idx].layout.swap_next(live_idx.len()) {
                state.toplevels.swap(live_idx[a], live_idx[b]);
            }
        }
        Rotate => state.monitors[idx].layout.rotate_split(),
        Close => {
            let focused = state.monitors[idx].layout.focused;
            if let Some(tl) = state
                .toplevels
                .iter()
                .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
                .nth(focused)
            {
                tl.close_window();
            }
        }
        ResizeGrow => state.monitors[idx].layout.resize_grow(),
        ResizeShrink => state.monitors[idx].layout.resize_shrink(),
        ToggleLayout => state.monitors[idx].layout.toggle_mode(),
        ToggleFullscreen => {
            let focused = state.monitors[idx].layout.focused;
            let focused_wl = state
                .toplevels
                .iter()
                .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
                .nth(focused)
                .and_then(|t| t.wl_surface());
            match (&state.monitors[idx].fullscreen, &focused_wl) {
                // Already fullscreen on the focused window — toggle off.
                (Some(fs), Some(f)) if fs == f => state.monitors[idx].fullscreen = None,
                // Nothing focused-fullscreen yet (including: something ELSE
                // is fullscreen and you've since focused a different
                // window) — fullscreen whichever window has focus now.
                (_, Some(f)) => state.monitors[idx].fullscreen = Some(f.clone()),
                (_, None) => {}
            }
        }
        ReloadConfig => reload_config(state),
        WallpaperChooser => {
            if state.wallpaper_chooser.is_some() {
                // Toggle off.
                state.wallpaper_chooser = None;
            } else {
                // With nothing configured (or nothing parsed) this still opens:
                // the chooser shows "NO WALLPAPERS FOUND".
                let files = state.wallpaper.files().to_vec();
                let current = state.wallpaper.current_idx();
                state.wallpaper_chooser =
                    Some(crate::wallpaper_chooser::WallpaperChooser::new(files, current));
            }
            mark_dirty_full(state);
            // Skip the relayout/refocus at the end — this doesn't touch windows.
            return;
        }
        SwitchWorkspace(n) => {
            let target = n.saturating_sub(1).min(veil_config::WORKSPACE_COUNT - 1);
            if target != state.monitors[idx].active_workspace {
                // Save the outgoing workspace's tiling state and restore the
                // target's, so hopping back later finds it exactly as left —
                // not reset to the dwindle default. Scoped to THIS monitor —
                // monitor 1's workspace 3 and monitor 0's workspace 3 are
                // unrelated, each monitor has its own 9-slot array.
                let cur_ws = state.monitors[idx].active_workspace;
                let cur_layout = state.monitors[idx].layout;
                state.monitors[idx].workspace_layouts[cur_ws as usize] = cur_layout;
                state.monitors[idx].layout = state.monitors[idx].workspace_layouts[target as usize];
                state.monitors[idx].active_workspace = target;
                state.show_osd(
                    format!("WORKSPACE {}", target + 1),
                    "",
                    None,
                    Duration::from_millis(900),
                );
                mark_dirty_full(state);
            }
        }
        MoveToWorkspace(n) => {
            let target = n.saturating_sub(1).min(veil_config::WORKSPACE_COUNT - 1);
            // Real Vec index of whatever's focused right now, found the same
            // way Close/ToggleFullscreen do — `layout.focused` is a
            // position among LIVE ACTIVE-WORKSPACE windows on THIS monitor,
            // not a raw index.
            let focused = state.monitors[idx].layout.focused;
            let real_idx = state
                .toplevels
                .iter()
                .enumerate()
                .filter(|(_, t)| t.alive() && t.monitor == idx && t.workspace == ws)
                .nth(focused)
                .map(|(i, _)| i);
            if let (Some(w_idx), true) = (real_idx, target != ws) {
                state.toplevels[w_idx].workspace = target;
                // Follow the window: same swap-in/out as SwitchWorkspace, so
                // the workspace we land on keeps its own layout state
                // instead of inheriting whatever the old one had. Still
                // entirely within this one monitor — "move to workspace"
                // never crosses monitors, that's a separate future action.
                let cur_ws = state.monitors[idx].active_workspace;
                let cur_layout = state.monitors[idx].layout;
                state.monitors[idx].workspace_layouts[cur_ws as usize] = cur_layout;
                state.monitors[idx].layout = state.monitors[idx].workspace_layouts[target as usize];
                state.monitors[idx].active_workspace = target;
                state.show_osd(
                    format!("MOVED TO WORKSPACE {}", target + 1),
                    "",
                    None,
                    Duration::from_millis(900),
                );
                mark_dirty_full(state);
            }
            // No window focused (empty workspace) — nothing to move, and
            // deliberately don't switch either; a bare workspace-switch
            // keybind already exists for that (SwitchWorkspace above).
        }
        VolumeUp => {
            // Off-thread; the result arrives via `media_tick`. Nothing here
            // changes layout or focus, so skip the relayout below.
            state.media.send(crate::media::MediaCmd::VolumeUp);
            return;
        }
        VolumeDown => {
            // Off-thread; the result arrives via `media_tick`. Nothing here
            // changes layout or focus, so skip the relayout below.
            state.media.send(crate::media::MediaCmd::VolumeDown);
            return;
        }
        VolumeMute => {
            // Off-thread; the result arrives via `media_tick`. Nothing here
            // changes layout or focus, so skip the relayout below.
            state.media.send(crate::media::MediaCmd::VolumeMute);
            return;
        }
        BrightnessUp => {
            // Off-thread; the result arrives via `media_tick`. Nothing here
            // changes layout or focus, so skip the relayout below.
            state.media.send(crate::media::MediaCmd::BrightnessUp);
            return;
        }
        BrightnessDown => {
            // Off-thread; the result arrives via `media_tick`. Nothing here
            // changes layout or focus, so skip the relayout below.
            state.media.send(crate::media::MediaCmd::BrightnessDown);
            return;
        }
        Launch(cmd) => spawn_command(&state.socket_name, &cmd),
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
        veil_config::ModKey::Ctrl => mods.ctrl,
        veil_config::ModKey::Alt => mods.alt,
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
            if count > 0 {
                l.selected = (l.selected + 1).min(count - 1);
            }
            mark_dirty_full(state);
        }
        return;
    }
    if sym == keysyms::KEY_Page_Down {
        if let Some(l) = state.launcher.as_mut() {
            let count = l.matches().len();
            if count > 0 {
                l.selected = (l.selected + 10).min(count - 1);
            }
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
            if count > 0 {
                l.selected = count - 1;
            }
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
    let Some(launcher) = state.launcher.take() else {
        return;
    };
    mark_dirty_full(state);

    let matches = launcher.matches();
    let exec = if !matches.is_empty() {
        matches[launcher.selected.min(matches.len() - 1)]
            .exec
            .clone()
    } else if !launcher.query.trim().is_empty() {
        launcher.query.clone()
    } else {
        return;
    };

    spawn_command(&state.socket_name, &exec);
}

/// Per-keystroke handling while the power menu modal is open. Up/Down move
/// the selection, Enter executes it, Escape cancels — same shape as
/// `handle_launcher_key` minus the query text (nothing to type here).
fn handle_power_menu_key(state: &mut State, mods: &ModifiersState, keysym: KeysymHandle<'_>) {
    let sym = keysym.modified_sym().raw();

    // <mod_key>+P closes the menu too — same chord opens and closes it.
    let mod_held = match state.keybinds.mod_key {
        veil_config::ModKey::Super => mods.logo,
        veil_config::ModKey::Ctrl => mods.ctrl,
        veil_config::ModKey::Alt => mods.alt,
        veil_config::ModKey::Shift => mods.shift,
    };
    if mod_held && matches!(sym, keysyms::KEY_p | keysyms::KEY_P) {
        state.power_menu = None;
        mark_dirty_full(state);
        return;
    }

    if sym == keysyms::KEY_Escape {
        state.power_menu = None;
        mark_dirty_full(state);
        return;
    }
    if sym == keysyms::KEY_Return || sym == keysyms::KEY_KP_Enter {
        execute_power_menu_selection(state);
        return;
    }
    if sym == keysyms::KEY_Up {
        if let Some(m) = state.power_menu.as_mut() {
            m.selected = m.selected.saturating_sub(1);
            mark_dirty_full(state);
        }
        return;
    }
    if sym == keysyms::KEY_Down {
        if let Some(m) = state.power_menu.as_mut() {
            m.selected = (m.selected + 1).min(crate::powermenu::POWER_ACTIONS.len() - 1);
            mark_dirty_full(state);
        }
    }
}

/// Run whatever's selected — fires immediately, no confirmation step (same
/// no-bloat call as the launcher: one Enter press, no "are you sure?").
/// Poweroff/Reboot shell out to systemd the same way `Launch` does; Logout
/// reuses the exact graceful-quit path Shift+Alt+E already uses — there's
/// no session manager here, so "logout" just means "stop veil-host".
fn execute_power_menu_selection(state: &mut State) {
    let Some(menu) = state.power_menu.take() else {
        return;
    };
    mark_dirty_full(state);

    match menu.selected_action() {
        PowerAction::Poweroff => spawn_command(&state.socket_name, "systemctl poweroff"),
        PowerAction::Reboot => spawn_command(&state.socket_name, "systemctl reboot"),
        PowerAction::Logout => state.stop.store(true, Ordering::Relaxed),
    }
}

/// Per-keystroke handling while the wallpaper chooser is open.
/// Arrow keys navigate the grid; Enter jumps to the selection; Escape or the
/// `wallpaper_chooser` keybind closes without changing anything.
fn handle_wallpaper_chooser_key(
    state: &mut State,
    mods: &ModifiersState,
    keysym: KeysymHandle<'_>,
) {
    use smithay::input::keyboard::keysyms;

    // Read the symbol first: `keysym_char` takes the handle by value.
    let sym = keysym.modified_sym().raw();

    // Allow the configured wallpaper_chooser action (defaults to <mod>+Y) to
    // close the overlay — same chord that opened it.
    let mod_held = match state.keybinds.mod_key {
        veil_config::ModKey::Super => mods.logo,
        veil_config::ModKey::Ctrl => mods.ctrl,
        veil_config::ModKey::Alt => mods.alt,
        veil_config::ModKey::Shift => mods.shift,
    };
    if mod_held {
        if let Some(ch) = keysym_char(keysym) {
            // Find whichever char is bound to WallpaperChooser and close on it.
            let bound_char = state
                .keybinds
                .binds
                .iter()
                .find(|(_, a)| *a == veil_config::Action::WallpaperChooser)
                .map(|(c, _)| *c);
            if bound_char == Some(ch) {
                state.wallpaper_chooser = None;
                mark_dirty_full(state);
                return;
            }
        }
    }

    match sym {
        keysyms::KEY_Escape => {
            state.wallpaper_chooser = None;
        }
        keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
            // Only the engine index moves; the rotation timer keeps its
            // schedule, so shuffle carries on from the picked image.
            if let Some(idx) = state
                .wallpaper_chooser
                .take()
                .and_then(|wc| wc.selected_file_idx())
            {
                state.wallpaper.jump_to(idx);
                request_wallpaper(state);
            }
        }
        keysyms::KEY_Left => {
            if let Some(wc) = state.wallpaper_chooser.as_mut() {
                wc.move_left();
            }
        }
        keysyms::KEY_Right => {
            if let Some(wc) = state.wallpaper_chooser.as_mut() {
                wc.move_right();
            }
        }
        keysyms::KEY_Up => {
            if let Some(wc) = state.wallpaper_chooser.as_mut() {
                wc.move_up();
            }
        }
        keysyms::KEY_Down => {
            if let Some(wc) = state.wallpaper_chooser.as_mut() {
                wc.move_down();
            }
        }
        keysyms::KEY_Page_Up => {
            if let Some(wc) = state.wallpaper_chooser.as_mut() {
                wc.page_up();
            }
        }
        keysyms::KEY_Page_Down => {
            if let Some(wc) = state.wallpaper_chooser.as_mut() {
                wc.page_down();
            }
        }
        keysyms::KEY_Home => {
            if let Some(wc) = state.wallpaper_chooser.as_mut() {
                wc.home();
            }
        }
        keysyms::KEY_End => {
            if let Some(wc) = state.wallpaper_chooser.as_mut() {
                wc.end();
            }
        }
        _ => {}
    }
    mark_dirty_full(state);
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
    let idx = active_monitor_idx(state);
    relayout_monitor(state, idx);
}

/// [`relayout`] for an explicit monitor. Client-driven requests (fullscreen,
/// unfullscreen) concern the window's own monitor, which need not be the one
/// under the pointer.
fn relayout_monitor(state: &mut State, idx: usize) {
    state.focus_cache = None;
    let ws = state.monitors[idx].active_workspace;
    let n = state
        .toplevels
        .iter()
        .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
        .count();
    if state.monitors[idx].layout.focused >= n {
        state.monitors[idx].layout.focused = n.saturating_sub(1);
    }
    let focused = state.monitors[idx].layout.focused;

    // Same alive-and-active-workspace-and-monitor sequence
    // configure/composite/pick_focus all already use — index into THIS, not
    // the raw toplevels vec. Windows on other workspaces or other monitors
    // are simply absent here, so they never get a configure_size call and
    // never enter layout_rects/composite below — that's what actually hides
    // them, no separate visibility flag needed.
    let alive: Vec<&Window> = state
        .toplevels
        .iter()
        .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
        .collect();

    // Which (alive-sequence) index, if any, is the fullscreen window right
    // now. Looked up by identity every call rather than cached, since
    // `fullscreen` only stores a WlSurface, not a position.
    let fs_idx = state.monitors[idx].fullscreen.as_ref().and_then(|fs| {
        alive
            .iter()
            .position(|t| t.wl_surface().as_ref() == Some(fs))
    });

    let (output_w, output_h) = (state.monitors[idx].output_w, state.monitors[idx].output_h);

    // Bar reserves a strip of the output — tiled windows only ever see
    // what's left. A fullscreen window still overrides to the FULL output
    // just below, ignoring this: fullscreen conventionally covers the bar
    // too, and composite() skips drawing the bar while something's
    // fullscreen, so nothing's left peeking out from underneath it.
    let (tile_h, tile_y) = if state.bar.enabled {
        let h = output_h.saturating_sub(BAR_HEIGHT);
        let y = if state.bar.position == veil_config::BarPosition::Top {
            BAR_HEIGHT as i32
        } else {
            0
        };
        (h, y)
    } else {
        (output_h, 0)
    };

    // Everyone tiles exactly as if nothing were fullscreen — this is what
    // keeps the *other* windows' positions stable across a fullscreen
    // toggle instead of re-tiling around the gap (previously this called
    // rects(n-1, ...) for "everyone else", which re-tiled them every time
    // and, in Scroll mode, used the wrong focused index since that mode's
    // rects depend on it for viewport position — both are what caused
    // windows to visibly jump/shrink/shift left on fullscreen toggle).
    // The fullscreen window's rect is simply overridden afterward.
    let mut rects = state.monitors[idx].layout.rects(n, output_w, tile_h);
    for r in rects.iter_mut() {
        r.y += tile_y;
    }
    if let Some(fs_i) = fs_idx {
        rects[fs_i] = Rect {
            x: 0,
            y: 0,
            w: output_w,
            h: output_h,
        };
    }

    for (i, tl) in alive.iter().enumerate() {
        let is_fs = fs_idx == Some(i);
        // While something's fullscreen, it's the only thing activated —
        // matches it visually being the only thing on screen. Otherwise,
        // normal focus-follows-tiling behavior.
        let activated = fs_idx.map_or(i == focused, |_| is_fs);
        tl.configure_size(rects[i], activated, is_fs);
    }

    let old_rects = std::mem::replace(&mut state.monitors[idx].layout_rects, rects);
    // If geometry didn't change (focus-only), dirty just the two affected tiles.
    // If it did change (window added/removed/resized), dirty the whole monitor.
    let new_focused = state.monitors[idx].layout.focused;
    let new_rects = &state.monitors[idx].layout_rects;
    if old_rects == *new_rects && !old_rects.is_empty() {
        // Only the `activated` state changed on two windows — dirty their rects.
        let prev_rect = old_rects.get(focused).copied();
        let next_rect = new_rects.get(new_focused).copied();
        let dirty_rect = match (prev_rect, next_rect) {
            (Some(a), Some(b)) if a == b => a,
            (Some(a), Some(b)) => a.union(&b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => Rect {
                x: 0,
                y: 0,
                w: state.monitors[idx].output_w,
                h: state.monitors[idx].output_h,
            },
        };
        mark_dirty_rect(state, idx, dirty_rect);
    } else {
        mark_dirty_full(state);
    }
}

/// Point the keyboard at whichever live toplevel is currently focused (or
/// nothing, if there are no windows left).
fn refocus_keyboard(state: &mut State) {
    let idx = active_monitor_idx(state);
    let ws = state.monitors[idx].active_workspace;
    let target = state
        .toplevels
        .iter()
        .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
        .nth(state.monitors[idx].layout.focused)
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
/// redraw instead of a partial one. That only holds if a hit returns the
/// RIGHT rect: `layout_rects` is indexed by the alive + this-monitor +
/// active-workspace sequence (same filter as `relayout`/`composite_one_monitor`),
/// NOT by position in the raw `toplevels` Vec. A window that isn't in that
/// sequence (dead, other workspace) returns `None` and falls back to full
/// damage, which is always safe.
fn toplevel_rect_for(state: &State, surface: &WlSurface) -> Option<(usize, Rect)> {
    let monitor = state
        .toplevels
        .iter()
        .find(|t| t.wl_surface().as_ref() == Some(surface))?
        .monitor;
    let m = state.monitors.get(monitor)?;
    let i = state
        .toplevels
        .iter()
        .filter(|t| t.alive() && t.monitor == monitor && t.workspace == m.active_workspace)
        .position(|t| t.wl_surface().as_ref() == Some(surface))?;
    let rect = m.layout_rects.get(i).copied()?;
    Some((monitor, rect))
}

/// H9: a pointer press while grabbing popups are open. If it landed on a
/// surface of the SAME client as the popup, leave it alone — Qt and GTK
/// dismiss their own menus on an outside click and expect to see it. If it
/// landed anywhere else (another client, the bar, empty desktop), no event
/// reaches the popup's client, so the compositor must send `popup_done`.
/// Newest popup first, so nested submenus unwind innermost-out.
fn dismiss_popup_grabs_on_press(state: &mut State, clicked: Option<WlSurface>) {
    state.popup_grabs.retain(|p| p.alive());
    let Some(first) = state.popup_grabs.first() else {
        return;
    };
    let popup_client = first.wl_surface().client().map(|c| c.id());
    let click_client = clicked.as_ref().and_then(|s| s.client()).map(|c| c.id());
    if popup_client.is_some() && popup_client == click_client {
        return;
    }
    for p in state.popup_grabs.drain(..).rev() {
        p.send_popup_done();
    }
}

/// Marks every monitor's whole output as needing repaint — the safe default
/// for any dirty event without a precise on-screen rect (resize, cursor
/// motion, overlay toggles, relayout, anything not going through
/// `toplevel_rect_for`). Over-damaging can only cost extra redraw work, never
/// leave stale pixels, so this is always a legal fallback. Deliberately
/// whole-COMPOSITOR, not just the active monitor: a caller with no specific
/// surface/monitor in hand (e.g. a config reload, a global theme change)
/// has no way to know which monitor(s) it actually affects, so marking all
/// of them is the only safe choice — same reasoning as before this had more
/// than one monitor to worry about, just applied to each of them now.
fn mark_dirty_full(state: &mut State) {
    state.dirty = true;
    for m in state.monitors.iter_mut() {
        let full = Rect {
            x: 0,
            y: 0,
            w: m.output_w,
            h: m.output_h,
        };
        m.damage = Some(match m.damage.take() {
            Some(d) => d.union(&full),
            None => full,
        });
    }
}

/// Marks just `rect` on one specific monitor as needing repaint, unioned
/// with whatever's already pending on that monitor this tick (multiple
/// surfaces can go dirty between composites). `monitor` is a property of
/// whatever triggered this (which window's surface committed, e.g.), not
/// resolved from cursor position — a window on a monitor you're not
/// currently looking at can still legitimately go dirty.
fn mark_dirty_rect(state: &mut State, monitor: usize, rect: Rect) {
    state.dirty = true;
    if let Some(m) = state.monitors.get_mut(monitor) {
        m.damage = Some(match m.damage.take() {
            Some(d) => d.union(&rect),
            None => rect,
        });
    }
}

/// Composite one monitor's live toplevels + popups + cursor (if active) +
/// overlays into its own RGBA frame and ship it. Pulled out of
/// `composite_and_send` so that function can stay a simple per-tick loop
/// over every monitor. `active_idx`, `show_help`, `launcher_present`, and
/// `power_menu_present` are resolved once per tick by the caller rather
/// than once per monitor — none of them vary by monitor.
fn composite_one_monitor(
    state: &mut State,
    idx: usize,
    active_idx: usize,
    show_help: bool,
    launcher_present: bool,
    power_menu_present: bool,
    wallpaper_chooser_present: bool,
) {
    let now = Instant::now();
    // One `&mut Monitor` borrow, reused for every monitor-scoped field
    // access below via `m.field` — NOT re-indexing `state.monitors[idx]`
    // repeatedly, which the borrow checker can't prove disjoint across
    // separate field accesses through the same runtime index. `state.X`
    // accesses elsewhere in this function (toplevels, surface_buffers,
    // floating, theme, ...) are all different top-level `State` fields,
    // disjoint from `state.monitors` and so compatible with `m` staying
    // alive throughout — same disjoint-field-borrow pattern the original
    // single-monitor version already relied on (`back = &mut
    // state.composite_buf` alongside reads of other `state.*` fields).
    let m = &mut state.monitors[idx];
    m.last_composite = Some(now);
    let (w, h) = (m.output_w, m.output_h);
    // Snapshot + reset this monitor's damage now, before any drawing below
    // — so damage that arrives *during* composite (shouldn't happen on this
    // single-threaded loop, but keeps the invariant obviously true rather
    // than relying on ordering elsewhere) accumulates for next tick instead
    // of being silently dropped.
    let frame_damage = m
        .damage
        .take()
        .unwrap_or(Rect { x: 0, y: 0, w, h })
        .clamp_to(w, h);

    let needed = (w as usize) * (h as usize) * 4;
    if m.composite_buf.len() != needed {
        m.composite_buf.resize(needed, 0);
    }

    // Background fill: one pass over the buffer as 4-byte RGBA pixels. Safe
    // (no `align_to_mut`; `Vec<u8>` only guarantees 1-byte alignment) and it
    // compiles to the same vectorised store loop in release builds.
    // With a wallpaper layer of the right size this is a straight memcpy of
    // the pre-scaled image; otherwise (none, or mid-resize until the loader
    // thread catches up) fall back to the solid colour.
    match &m.wallpaper {
        Some(wp) if wp.len() == needed => m.composite_buf.copy_from_slice(wp),
        _ => {
            let bg = state.background;
            m.composite_buf.as_chunks_mut::<4>().0.fill(bg);
        }
    }

    let ws = m.active_workspace;
    let back = &mut m.composite_buf;

    // Toplevels (root buffer + subsurfaces) then their popups, each at its
    // tiled rect origin. Popups are positioned relative to their toplevel.
    // Collected as Option so index i still lines up with layout_rects[i] —
    // an X11 window not yet paired with a wl_surface has nothing to blit,
    // but it still occupies a tiled slot and must not shift later indices.
    // Filtered to THIS monitor's active workspace — this MUST produce the
    // same alive-and-monitor-and-active-workspace sequence relayout() used
    // to build layout_rects, or index i here won't line up with rects[i].
    let toplevels: Vec<Option<WlSurface>> = state
        .toplevels
        .iter()
        .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
        .map(|t| t.wl_surface())
        .collect();
    for (i, surf_opt) in toplevels.iter().enumerate() {
        let Some(surf) = surf_opt else { continue };
        let r = m
            .layout_rects
            .get(i)
            .copied()
            .unwrap_or(Rect { x: 0, y: 0, w, h });
        blit_subtree(back, w, h, &state.surface_buffers, surf, (r.x, r.y));
        for (popup, off) in PopupManager::popups_for_surface(surf) {
            let ps = popup.wl_surface().clone();
            blit_subtree(
                back,
                w,
                h,
                &state.surface_buffers,
                &ps,
                (r.x + off.x, r.y + off.y),
            );
        }
    }

    // Fullscreen window repaints last among tiled content, regardless of its
    // position in the loop above — other windows keep their normal (real,
    // non-full) tiled rects now (see relayout()), so without this, one that
    // happens to iterate after the fullscreen window in `toplevels` order
    // could paint its own small rect right over part of it.
    //
    // Monitor- and workspace-filtered too: `m.fullscreen` is only ever set
    // from a window that was this-monitor-active-workspace at the time
    // (dispatch_action's ToggleFullscreen and the xdg fullscreen_request
    // handler both look up the focused window the same filtered way), so a
    // fullscreen window on a workspace you've since switched away from (on
    // this monitor) correctly stops matching here and this block becomes a
    // no-op until you switch back.
    if let Some(fs) = &m.fullscreen {
        if let Some(surf) = state
            .toplevels
            .iter()
            .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
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
    // position from the X server, not a tiled rect. Drawn on every
    // monitor's pass unconditionally for now, same as before this had more
    // than one monitor to worry about: XWayland's coordinate space isn't
    // (yet) monitor-aware here, so there's no correct per-monitor filter to
    // apply — a real fix means tagging floating X11Surfaces with their
    // parent toplevel's monitor and translating into that monitor's local
    // space, which is real follow-up work, not part of this pass. With one
    // monitor (today) this is exactly the old behavior; with more than one
    // it'll over-draw rather than mis-draw, which is the safer failure mode.
    for f in state.floating.iter().filter(|f| f.alive()) {
        let Some(surf) = f.wl_surface() else { continue };
        let g = f.geometry();
        blit_subtree(
            back,
            w,
            h,
            &state.surface_buffers,
            &surf,
            (g.loc.x, g.loc.y),
        );
    }

    // Cursor on top — only on the monitor the pointer is actually over.
    // Every other monitor renders with no cursor overlay this tick.
    if idx == active_idx {
        match &state.cursor_status {
            CursorImageStatus::Surface(cs) => {
                let hotspot = with_states(cs, |s| {
                    s.data_map
                        .get::<std::sync::Mutex<CursorImageAttributes>>()
                        .map(|m| m.lock().unwrap().hotspot)
                        .unwrap_or_default()
                });
                let cx = m.pointer_pos.0 as i32 - hotspot.x;
                let cy = m.pointer_pos.1 as i32 - hotspot.y;
                blit_subtree(back, w, h, &state.surface_buffers, cs, (cx, cy));
            }
            CursorImageStatus::Named(_) => {
                // Client wants a themed cursor (default arrow etc) — we don't
                // load themes. Draw a tiny built-in arrow so the user can see
                // where their pointer is.
                draw_fallback_cursor(back, w, h, m.pointer_pos.0 as i32, m.pointer_pos.1 as i32);
            }
            CursorImageStatus::Hidden => {}
        }
    }

    // Only clone keybinds/launcher when the overlays are actually on screen.
    // `theme` is `Copy` (plain color bytes) so pulling it out ahead of the
    // `back` borrow costs nothing.
    let theme = state.theme;
    // N15: only a fullscreen window that is on this monitor's *active*
    // workspace hides the bar (same test as the fullscreen draw block above).
    let fullscreen_visible = m.fullscreen.as_ref().is_some_and(|fs| {
        state.toplevels.iter().any(|t| {
            t.alive() && t.monitor == idx && t.workspace == ws && t.wl_surface().as_ref() == Some(fs)
        })
    });
    if state.bar.enabled && !fullscreen_visible {
        // Occupancy is THIS monitor's own — a window on another monitor's
        // workspace 3 has no bearing on what this monitor's bar shows for
        // its own workspace 3. Matches the dumb-separation model: each
        // monitor's bar is exactly as if it were the only monitor.
        let mut occupancy = [0u8; veil_config::WORKSPACE_COUNT as usize];
        for t in state
            .toplevels
            .iter()
            .filter(|t| t.alive() && t.monitor == idx)
        {
            let occ_idx = t.workspace as usize;
            if occ_idx < occupancy.len() {
                occupancy[occ_idx] += 1;
            }
        }
        let active_ws = m.active_workspace;
        let hitboxes = draw_bar(
            &theme,
            &state.bar,
            &state.keybinds,
            active_ws,
            &occupancy,
            back,
            w,
            h,
        );
        m.bar_hitboxes = hitboxes;
    } else {
        // Fullscreen hides the bar entirely (conventional — see relayout()'s
        // note on why the fullscreen rect ignores the bar's reserved strip)
        // and a disabled bar obviously has nothing to draw. Either way, old
        // hitboxes must go or a click could "launch" through a bar that
        // isn't there anymore.
        m.bar_hitboxes.clear();
    }
    // S12: modals (help / launcher / power menu / chooser / OSD) belong to the
    // monitor that is active; the others no longer get a copy.
    let on_active = idx == active_idx;
    if show_help && on_active {
        let keybinds = state.keybinds.clone();
        draw_help_overlay(&keybinds, &theme, back, w, h);
    }
    if launcher_present && on_active {
        if let Some(ref l) = state.launcher {
            let mod_key = state.keybinds.mod_key;
            // Borrow checker: we need to call draw_launcher_overlay with `back`
            // already mutably borrowed. Clone the launcher (it's tiny) rather
            // than fighting the borrow checker with unsafe aliasing.
            let l = l.clone();
            draw_launcher_overlay(&l, mod_key, &theme, back, w, h);
        }
    }
    if power_menu_present && on_active {
        if let Some(ref pm) = state.power_menu {
            let mod_key = state.keybinds.mod_key;
            let pm = pm.clone();
            draw_power_menu_overlay(&pm, mod_key, &theme, back, w, h);
        }
    }
    if wallpaper_chooser_present && on_active {
        if let Some(wc) = state.wallpaper_chooser.as_mut() {
            let g = crate::wallpaper_chooser::geometry(w, h);
            wc.set_view(g.cols, g.rows);
            wc.draw(&theme, back, w, h);
        }
    }
    if let Some(ref osd) = state.osd {
        if Instant::now() > osd.expires_at {
            state.osd = None;
            state.dirty = true;
        } else {
            if on_active {
                let osd_clone = osd.clone();
                draw_osd_overlay(&osd_clone, back, w, h);
            }
            state.dirty = true;
        }
    }

    state.frame_serial = state.frame_serial.wrapping_add(1);

    // Zero-copy dispatch: swap this monitor's composite_buf out, wrap in
    // Arc, hand to the render thread. For the *next* frame's buffer: try to
    // reclaim the Arc we sent last time for THIS monitor via
    // Arc::try_unwrap — if the render thread already dropped its clone (the
    // common case; it's the bottleneck, not us), that's a real allocation
    // avoided instead of "one alloc per frame no matter what."
    let m = &mut state.monitors[idx]; // re-borrow: `back`/`theme` above no longer live past their last use
    let mut outgoing = Vec::new();
    std::mem::swap(&mut m.composite_buf, &mut outgoing);
    let outgoing = Arc::new(outgoing);
    m.composite_buf = match m.prev_frame.take().map(Arc::try_unwrap) {
        Some(Ok(mut reclaimed)) => {
            reclaimed.clear();
            reclaimed.reserve(needed);
            reclaimed
        }
        // Render thread is still holding its reference (lagging) or this is
        // the first frame — fall back to a fresh allocation, same as before.
        _ => Vec::with_capacity(needed),
    };
    m.prev_frame = Some(outgoing.clone());
    let _ = state.frame_tx.send(crate::sink::Frame {
        rgba: outgoing,
        width: w,
        height: h,
        output_id: idx,
        serial: state.frame_serial,
        damage: frame_damage,
    });
}

/// Composite every monitor + ship each one's frame. Called from the
/// periodic tick when `dirty`. The dirty/throttle check is whole-compositor
/// (all monitors composite on the same tick cadence — see `Monitor`'s doc),
/// not per-monitor; the actual drawing work is delegated to
/// `composite_one_monitor`, called once per entry in `state.monitors`.
/// Composite attempts skipped by S1 backpressure since the last tick-trace line.
static BP_SKIPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn composite_and_send(state: &mut State) {
    if !state.dirty {
        return;
    }
    let now = Instant::now();
    if let Some(t) = state.monitors[0].last_composite {
        if now.duration_since(t) < state.composite_interval {
            return;
        }
    }
    // S1 backpressure: `prev_frame` is the Arc we last handed to `frame_tx`.
    // strong_count > 1 means the render side (channel, `latest` slot, or an
    // in-flight `render_frame`) still holds it, so another composite would
    // only grow an unbounded queue of ~8 MB frames behind a slow consumer
    // (serial/SSH). Bail BEFORE touching `dirty`, damage, or frame
    // callbacks: damage keeps accumulating on each monitor, `dirty` stays
    // set so the next tick retries, and clients get no frame callback so
    // they slow down too. Steady state is at most one frame in flight per
    // monitor, and the `try_unwrap` reclaim below now always succeeds.
    if state.monitors.iter().any(|m| {
        m.prev_frame
            .as_ref()
            .is_some_and(|a| Arc::strong_count(a) > 1)
    }) {
        BP_SKIPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    state.dirty = false;
    tracing::debug!(
        "compositing frame (buffers={})",
        state.surface_buffers.len()
    );

    let show_help = state.show_help;
    let launcher_present = state.launcher.is_some();
    let power_menu_present = state.power_menu.is_some();
    let wallpaper_chooser_present = state.wallpaper_chooser.is_some();
    let active_idx = active_monitor_idx(state);

    for idx in 0..state.monitors.len() {
        composite_one_monitor(
            state,
            idx,
            active_idx,
            show_help,
            launcher_present,
            power_menu_present,
            wallpaper_chooser_present,
        );
    }

    // Fire frame callbacks now that we've consumed and displayed every
    // monitor's frame this tick. Chromium uses these as vsync: it won't
    // submit the next buffer until it receives one. Firing here (after all
    // compositing) caps Chromium's render rate to our composite_interval
    // instead of the 8ms tick rate. Whole-compositor, not per-monitor — a
    // client's surface belongs to exactly one monitor, but there's no
    // reason to fire its callback more than once per tick regardless.
    let time = state.start_time.elapsed().as_millis() as u32;
    let surfaces: Vec<WlSurface> = state
        .toplevels
        .iter()
        .filter(|t| t.alive())
        .filter_map(|t| t.wl_surface())
        .chain(
            state
                .floating
                .iter()
                .filter(|f| f.alive())
                .filter_map(|f| f.wl_surface()),
        )
        .collect();
    let now = monotonic_now();
    let seq = state.frame_serial;
    for s in &surfaces {
        send_frame_callbacks(s, time);
        send_presentation_feedback(s, &state.output, now, seq);
        for (popup, _) in PopupManager::popups_for_surface(s) {
            send_frame_callbacks(popup.wl_surface(), time);
            send_presentation_feedback(popup.wl_surface(), &state.output, now, seq);
        }
    }
}

fn monotonic_now() -> std::time::Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Point the wallpaper engine at a (possibly new) config and start a load.
fn set_wallpaper_config(state: &mut State, cfg: Option<veil_config::WallpaperConfig>) {
    if state.wallpaper.configure(cfg) {
        request_wallpaper(state);
    }
}

/// (Re)load the wallpaper for the monitors' current sizes. With no wallpaper
/// configured this drops any layers so the solid background shows again.
fn request_wallpaper(state: &mut State) {
    if state.wallpaper.is_active() {
        let sizes = state.monitors.iter().map(|m| (m.output_w, m.output_h)).collect();
        let bg = state.background;
        state.wallpaper.request(sizes, bg);
    } else {
        for m in &mut state.monitors {
            m.wallpaper = None;
        }
        mark_dirty_full(state);
    }
}

/// Install freshly loaded layers on every monitor of matching size.
fn apply_wallpaper_layers(state: &mut State, layers: Vec<crate::wallpaper::Layer>) {
    for m in &mut state.monitors {
        m.wallpaper = layers
            .iter()
            .find(|l| l.size == (m.output_w, m.output_h))
            .map(|l| l.rgba.clone());
    }
    mark_dirty_full(state);
}

/// Per-tick: pick up a finished load and drive rotation.
/// Show any finished volume/brightness result. Cheap: one channel poll.
fn media_tick(state: &mut State) {
    while let Some(u) = state.media.try_recv() {
        state.show_osd(u.title, u.body, u.progress, Duration::from_secs(2));
    }
}

fn wallpaper_tick(state: &mut State) {
    if state
        .wallpaper_chooser
        .as_mut()
        .is_some_and(|wc| wc.poll_thumbs())
    {
        mark_dirty_full(state);
    }
    let monitors = &state.monitors;
    let layers = state.wallpaper.tick(
        Instant::now(),
        || monitors.iter().map(|m| (m.output_w, m.output_h)).collect(),
        state.background,
    );
    if let Some(layers) = layers {
        apply_wallpaper_layers(state, layers);
    }
}

/// How often to composite: the `fps` config value, or the display's own
/// refresh when `fps == 0`. Deliberately 10% under the nominal period: the
/// tick timer fires at half that period and the check is `elapsed < interval`,
/// so an exact period would lose a tick to timer jitter and fall to half rate.
/// Compositing a hair faster than the display is free: the output loop only
/// flips once per VBLANK and coalesces the rest.
fn composite_interval_for(fps: u32, refresh_mhz: u32) -> Duration {
    let mhz = if fps == 0 {
        refresh_mhz.max(1000) as u64
    } else {
        fps as u64 * 1000
    };
    Duration::from_micros(1_000_000_000 / mhz * 9 / 10)
}

/// Answer `wp_presentation` feedback requests with `presented`. They used to
/// be dropped, which smithay turns into `discarded` — "this frame was never
/// shown". Chromium-family browsers use that feedback to judge whether their
/// frames are reaching the screen, and give up on fullscreen after ~1.5 s of
/// discards. Timestamped at composite time (the frame is in the buffer we hand
/// to the output this tick), on CLOCK_MONOTONIC to match the advertised clock.
fn send_presentation_feedback(
    surface: &WlSurface,
    output: &Output,
    now: std::time::Duration,
    seq: u64,
) {
    use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
    use smithay::wayland::presentation::{PresentationFeedbackCachedState, Refresh};
    // Real refresh period from the mode the output advertises (mHz -> us).
    let period_us = output
        .current_mode()
        .map_or(16_667, |m| 1_000_000_000 / m.refresh.max(1000) as u64);
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surf, states, &()| {
            let mut guard = states.cached_state.get::<PresentationFeedbackCachedState>();
            for cb in std::mem::take(&mut guard.current().callbacks) {
                cb.presented(
                    output,
                    now,
                    Refresh::Fixed(std::time::Duration::from_micros(period_us)),
                    seq,
                    Kind::empty(),
                );
            }
        },
        |_, _, &()| true,
    );
}

/// `<mod_key>+/` help overlay: dumps the parsed `keybinds` config as an
/// on-screen box, stamped directly into the composited RGBA frame with the
/// built-in 5x7 font ([`crate::font5x7`]) — veil-host has no other text
/// rendering.
/// Renders the status bar: workspace widget (fixed 16 chars), clock (fixed
/// 8 chars), then app shortcuts filling whatever's left. Shortcuts are
/// derived from `keybinds` — every `Action::Launch` bind becomes a tile,
/// labeled from its command — not a separate config list, so nothing needs
/// declaring twice. Returns the on-screen click target for each app tile —
/// PointerButton consults this directly rather than redoing this layout
/// math per click.
///
/// Font note: the built-in 5x7 bitmap font (font5x7.rs) is caps-only and
/// has no bullet glyph, so app labels render UPPERCASE regardless of case
/// here, and the workspace widget's per-window marks use `*` in place of
/// the `•` bullets from the original bar spec — closest available glyph
/// with real visual weight (`.` renders as a single near-invisible pixel
/// at this scale).
#[allow(clippy::too_many_arguments)]
fn draw_bar(
    theme: &veil_config::Theme,
    bar: &veil_config::BarConfig,
    keybinds: &veil_config::Keybinds,
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
    const PAD_Y: i32 = 3; // vertically centers a 7px glyph in a 12px bar

    let y0 = if bar.position == BarPosition::Top {
        0
    } else {
        h.saturating_sub(BAR_HEIGHT) as i32
    };
    fill_rect(back, w, h, 0, y0, w, BAR_HEIGHT, theme.panel_bg);

    // --- Column 1: workspace widget, fixed 16 chars / 96px ---
    // Only draws up to whichever's higher: the active workspace, or the
    // highest-numbered occupied one — otherwise all 9 slots would eat the
    // whole budget before the clock column even started. Each token is
    // "N:" (empty) or "N:" + up to 3 `*` (one per window, capped). Active
    // workspace's token gets the accent color so it stands out at a glance.
    let col1_w = 16 * ADVANCE;
    let highest = occupancy
        .iter()
        .rposition(|&c| c > 0)
        .map(|i| i as u8 + 1)
        .unwrap_or(0)
        .max(active_ws + 1)
        .min(veil_config::WORKSPACE_COUNT);
    let mut cx = 2i32;
    for n in 1..=highest {
        let count = occupancy[(n - 1) as usize];
        let token = format!("{n}:{}", "*".repeat(count.min(3) as usize));
        let token_w = token.chars().count() as u32 * ADVANCE;
        if cx as u32 + token_w > col1_w {
            break;
        }
        let color = if n == active_ws + 1 {
            theme.accent
        } else {
            theme.text_dim
        };
        draw_text(back, w, h, cx, y0 + PAD_Y, SCALE, &token, color);
        cx += token_w as i32 + ADVANCE as i32; // one blank char of gap
    }

    // --- Column 2: clock, fixed 8 chars / 48px, starts right after col 1 ---
    // 24-hour per spec. No internal timer needed — this just reads the
    // current time on whatever cadence composite() already runs at; the
    // displayed minute obviously only visibly changes once a minute.
    let col2_x = col1_w as i32;
    let clock = chrono::Local::now().format("%H:%M").to_string();
    draw_text(
        back,
        w,
        h,
        col2_x + 2,
        y0 + PAD_Y,
        SCALE,
        &clock,
        theme.text,
    );

    // --- Column 3: app shortcuts, whatever width is left ---
    // Every Action::Launch bind becomes a tile — no separate `bar.apps`
    // list to keep in sync. Label is the command's program name (first
    // whitespace token, path stripped), so `apps.f = "firefox"` shows as
    // FIREFOX and `apps.t = "foot -e htop"` shows as FOOT, not the full
    // command line. Order follows `keybinds.binds` declaration order.
    let col3_x = col1_w as i32 + 8 * ADVANCE as i32;
    let mut hitboxes = Vec::new();
    let mut tx = col3_x + 2;
    for (_, action) in &keybinds.binds {
        let veil_config::Action::Launch(cmd) = action else {
            continue;
        };
        let prog = cmd.split_whitespace().next().unwrap_or(cmd);
        let name = prog.rsplit('/').next().unwrap_or(prog);
        let label = format!("[{name}]");
        let label_w = label.chars().count() as u32 * ADVANCE;
        if tx as u32 + label_w > w {
            break;
        } // out of bar width — rest just don't fit
        draw_text(back, w, h, tx, y0 + PAD_Y, SCALE, &label, theme.text);
        hitboxes.push((
            Rect {
                x: tx,
                y: y0,
                w: label_w,
                h: BAR_HEIGHT,
            },
            cmd.clone(),
        ));
        tx += label_w as i32 + ADVANCE as i32;
    }

    hitboxes
}

fn draw_help_overlay(
    keybinds: &veil_config::Keybinds,
    theme: &veil_config::Theme,
    back: &mut [u8],
    w: u32,
    h: u32,
) {
    use crate::font5x7::{draw_text, fill_rect, GLYPH_H, GLYPH_W};

    let scale = 2u32;
    let advance = (GLYPH_W + 1) * scale;
    let line_h = (GLYPH_H + 3) * scale;
    let pad = 12i32;

    let mod_label = keybinds.mod_key.label().to_ascii_uppercase();
    let mut lines: Vec<String> = vec!["KEYBINDS".to_string(), String::new()];
    for (key, action) in &keybinds.binds {
        // Return is stored as '\r', which the bitmap font can't draw.
        let key_label = if *key == '\r' {
            "ENTER".to_string()
        } else {
            key.to_ascii_uppercase().to_string()
        };
        lines.push(format!(
            "{mod_label}+{key_label}  {}",
            action.label().to_ascii_uppercase()
        ));
    }
    lines.push(String::new());
    lines.push(format!("{mod_label}+/  TOGGLE THIS MENU"));
    lines.push(format!("{mod_label}+D  APP LAUNCHER"));
    lines.push(format!("{mod_label}+P  POWER MENU"));
    lines.push("ALT+TAB  CYCLE WINDOWS".to_string());
    lines.push("SHIFT+ALT+E  QUIT (GRACEFUL)".to_string());

    let text_cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u32;
    let box_w = text_cols * advance + pad as u32 * 2;
    let box_h = lines.len() as u32 * line_h + pad as u32 * 2;
    let x0 = ((w as i32 - box_w as i32) / 2).max(0);
    let y0 = ((h as i32 - box_h as i32) / 2).max(0);

    let border = 2i32;
    fill_rect(
        back,
        w,
        h,
        x0 - border,
        y0 - border,
        box_w + (border as u32 * 2),
        box_h + (border as u32 * 2),
        theme.border,
    );
    fill_rect(back, w, h, x0, y0, box_w, box_h, theme.panel_bg);
    for (i, line) in lines.iter().enumerate() {
        let ty = y0 + pad + i as i32 * line_h as i32;
        let color = if i == 0 { theme.header } else { theme.text };
        draw_text(back, w, h, x0 + pad, ty, scale, line, color);
    }
}

/// `<mod_key>+D` launcher modal: query box + top matches, same font/box
/// style as the help overlay. Selected row gets a highlight bar.
fn draw_launcher_overlay(
    launcher: &Launcher,
    mod_key: veil_config::ModKey,
    theme: &veil_config::Theme,
    back: &mut [u8],
    w: u32,
    h: u32,
) {
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

    let text_cols = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(40) as u32;
    let box_w = text_cols * advance + pad as u32 * 2;
    let box_h = lines.len() as u32 * line_h + pad as u32 * 2;
    let x0 = ((w as i32 - box_w as i32) / 2).max(0);
    let y0 = ((h as i32 - box_h as i32) / 2).max(0);

    let border = 2i32;
    fill_rect(
        back,
        w,
        h,
        x0 - border,
        y0 - border,
        box_w + (border as u32 * 2),
        box_h + (border as u32 * 2),
        theme.border,
    );
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

/// `<mod_key>+P` power menu modal: fixed 3-row list, same box/font style as
/// the launcher overlay. Selected row gets a highlight bar.
fn draw_power_menu_overlay(
    menu: &PowerMenu,
    mod_key: veil_config::ModKey,
    theme: &veil_config::Theme,
    back: &mut [u8],
    w: u32,
    h: u32,
) {
    use crate::font5x7::{draw_text, fill_rect, GLYPH_H, GLYPH_W};

    let scale = 2u32;
    let advance = (GLYPH_W + 1) * scale;
    let line_h = (GLYPH_H + 3) * scale;
    let pad = 12i32;

    let mod_label = mod_key.label().to_ascii_uppercase();
    let selected = menu.selected.min(crate::powermenu::POWER_ACTIONS.len() - 1);

    let mut lines: Vec<String> = vec![
        format!("POWER  ({mod_label}+P CLOSE, ENTER SELECT, ESC CANCEL)"),
        String::new(),
    ];
    let header_rows = lines.len();
    for a in crate::powermenu::POWER_ACTIONS.iter() {
        lines.push(a.label().to_string());
    }

    let text_cols = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(40) as u32;
    let box_w = text_cols * advance + pad as u32 * 2;
    let box_h = lines.len() as u32 * line_h + pad as u32 * 2;
    let x0 = ((w as i32 - box_w as i32) / 2).max(0);
    let y0 = ((h as i32 - box_h as i32) / 2).max(0);

    let border = 2i32;
    fill_rect(
        back,
        w,
        h,
        x0 - border,
        y0 - border,
        box_w + (border as u32 * 2),
        box_h + (border as u32 * 2),
        theme.border,
    );
    fill_rect(back, w, h, x0, y0, box_w, box_h, theme.panel_bg);

    // Highlight bar behind the selected entry row
    let row = header_rows + selected;
    let ry = y0 + pad + row as i32 * line_h as i32 - 2;
    fill_rect(back, w, h, x0 + 2, ry, box_w - 4, line_h, theme.highlight);

    for (i, line) in lines.iter().enumerate() {
        let ty = y0 + pad + i as i32 * line_h as i32;
        let color = if i == 0 { theme.header } else { theme.text };
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
fn toplevel_at(state: &State, monitor: usize, x: f64, y: f64) -> Option<usize> {
    let xi = x as i32;
    let yi = y as i32;
    state.monitors[monitor]
        .layout_rects
        .iter()
        .position(|r| xi >= r.x && yi >= r.y && xi < r.x + r.w as i32 && yi < r.y + r.h as i32)
}

/// Walk all toplevels' popups (newest first) then the toplevel root.
/// Return the first surface whose cached buffer rect contains (x, y),
/// along with the cursor's surface-local coordinates.
fn pick_focus(
    state: &State,
    monitor: usize,
    x: f64,
    y: f64,
) -> Option<(
    WlSurface,
    smithay::utils::Point<f64, smithay::utils::Logical>,
)> {
    let xi = x as i32;
    let yi = y as i32;

    // Floating (override-redirect) windows first — they're painted on top of
    // everything in composite(), so they must win hit-testing too, or a
    // dropdown/menu would be unclickable over the tiled window beneath it.
    // Last-mapped wins on overlap, same "last wins" convention as popups below.
    for f in state.floating.iter().rev().filter(|f| f.alive()) {
        let Some(surf) = f.wl_surface() else { continue };
        let g = f.geometry();
        if xi >= g.loc.x && yi >= g.loc.y && xi < g.loc.x + g.size.w && yi < g.loc.y + g.size.h {
            return Some((surf, (g.loc.x as f64, g.loc.y as f64).into()));
        }
    }

    // While something's fullscreen, it's the only tiled thing clickable —
    // everything else keeps its normal rect (see relayout()) but is
    // visually covered (see composite()), so hit-testing it would let
    // clicks fall through to a window you can't actually see.
    let ws = state.monitors[monitor].active_workspace;
    if let Some(fs) = &state.monitors[monitor].fullscreen {
        return state
            .toplevels
            .iter()
            .filter(|t| t.alive() && t.monitor == monitor && t.workspace == ws)
            .find(|t| t.wl_surface().as_ref() == Some(fs))
            .and_then(|t| t.wl_surface())
            .map(|surf| (surf, (0.0, 0.0).into()));
    }

    // Live toplevels paired with their tiled rect, topmost (last) first.
    // Option preserves index alignment with layout_rects (see composite()).
    // Monitor- and workspace-filtered for the same reason composite()'s
    // copy is — must match the exact sequence relayout() used to build
    // layout_rects.
    let live: Vec<Option<WlSurface>> = state
        .toplevels
        .iter()
        .filter(|t| t.alive() && t.monitor == monitor && t.workspace == ws)
        .map(|t| t.wl_surface())
        .collect();
    for (i, root_opt) in live.iter().enumerate().rev() {
        let Some(root) = root_opt else { continue };
        let (out_w, out_h) = (
            state.monitors[monitor].output_w,
            state.monitors[monitor].output_h,
        );
        let r = state.monitors[monitor]
            .layout_rects
            .get(i)
            .copied()
            .unwrap_or(Rect {
                x: 0,
                y: 0,
                w: out_w,
                h: out_h,
            });

        // Popups (per-toplevel) — last-added wins on overlap. Positioned at
        // the toplevel rect origin + the popup's toplevel-relative offset.
        let popups: Vec<_> = PopupManager::popups_for_surface(root).collect();
        for (popup, off) in popups.iter().rev() {
            let ps = popup.wl_surface();
            if let Some(buf) = state.surface_buffers.get(&ps.id()) {
                let bx = r.x + off.x;
                let by = r.y + off.y;
                if xi >= bx && yi >= by && xi < bx + buf.w as i32 && yi < by + buf.h as i32 {
                    // loc = surface origin in compositor space; Smithay
                    // computes surface-local as event.location - loc.
                    return Some((ps.clone(), (bx as f64, by as f64).into()));
                }
            }
        }

        // Toplevel root sits at its rect origin.
        if let Some(buf) = state.surface_buffers.get(&root.id()) {
            if xi >= r.x && yi >= r.y && xi < r.x + buf.w as i32 && yi < r.y + buf.h as i32 {
                return Some((root.clone(), (r.x as f64, r.y as f64).into()));
            }
        }
    }
    None
}

fn apply_input(state: &mut State, cmd: InputCmd) {
    use smithay::backend::input::{ButtonState as BState, KeyState};
    let serial = state.next_serial();
    let time = state.start_time.elapsed().as_millis() as u32;

    match cmd {
        InputCmd::Key {
            keycode, pressed, ..
        } => {
            let ks = if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            };
            // xkbcommon Keycode is an X11 keycode = evdev + 8.
            let kb = state.keyboard.clone();
            // N6: release of a press we intercepted. It still has to go
            // through xkb (so modifier state stays right), but not to clients.
            let suppress_release = !pressed && state.suppressed_keys.remove(&keycode);
            let swallowed = kb.input::<(), _>(
                state,
                (keycode + 8).into(),
                ks,
                serial,
                time,
                |st, mods: &ModifiersState, keysym: KeysymHandle<'_>| {
                    if suppress_release {
                        return FilterResult::Intercept(());
                    }
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

                    if st.power_menu.is_some() {
                        if pressed {
                            handle_power_menu_key(st, mods, keysym);
                        }
                        return FilterResult::Intercept(());
                    }

                    if st.wallpaper_chooser.is_some() {
                        if pressed {
                            handle_wallpaper_chooser_key(st, mods, keysym);
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
                            st.media.send(crate::media::MediaCmd::VolumeUp);
                            return FilterResult::Intercept(());
                        }
                        keysyms::KEY_XF86AudioLowerVolume => {
                            st.media.send(crate::media::MediaCmd::VolumeDown);
                            return FilterResult::Intercept(());
                        }
                        keysyms::KEY_XF86AudioMute => {
                            st.media.send(crate::media::MediaCmd::VolumeMute);
                            return FilterResult::Intercept(());
                        }
                        keysyms::KEY_XF86MonBrightnessUp => {
                            st.media.send(crate::media::MediaCmd::BrightnessUp);
                            return FilterResult::Intercept(());
                        }
                        keysyms::KEY_XF86MonBrightnessDown => {
                            st.media.send(crate::media::MediaCmd::BrightnessDown);
                            return FilterResult::Intercept(());
                        }
                        _ => {}
                    }

                    // N3: a client (VM, remote desktop) asked to keep the
                    // compositor's shortcuts, and it holds the focus: pass
                    // everything through, except the escape chord.
                    if let Some(inhibitor) = focused_active_inhibitor(st) {
                        if is_inhibit_escape(sym, mods.alt, mods.shift) {
                            inhibitor.inactivate();
                            return FilterResult::Intercept(());
                        }
                        return FilterResult::Forward;
                    }

                    // Alt+Tab / Alt+Shift+Tab: cycle focus among live windows
                    // on the active workspace. Fixed to literal Alt rather
                    // than `keybinds.mod_key` — same reasoning as
                    // Shift+Alt+E below: this chord is muscle memory across
                    // every DE and shouldn't move if mod_key is remapped.
                    // N5: Shift+Tab resolves to ISO_Left_Tab in `modified_sym`,
                    // not Tab, so backward cycling never matched before.
                    let left_tab = sym == keysyms::KEY_ISO_Left_Tab;
                    if (sym == keysyms::KEY_Tab || left_tab) && mods.alt {
                        cycle_focus(st, !(mods.shift || left_tab));
                        return FilterResult::Intercept(());
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
                        veil_config::ModKey::Ctrl => mods.ctrl,
                        veil_config::ModKey::Alt => mods.alt,
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
                        if ch == 'p' {
                            st.power_menu = Some(PowerMenu::new());
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
            if pressed {
                if swallowed.is_some() {
                    state.suppressed_keys.insert(keycode);
                } else {
                    // Forwarded press: forget any stale entry so its release
                    // is forwarded too (no stuck key in the client).
                    state.suppressed_keys.remove(&keycode);
                }
            }
        }

        InputCmd::PointerMotionAbs {
            x,
            y,
            width,
            height,
        } => {
            // Caller (evdev's virtual cursor, or crossterm's terminal-cell
            // mapping) works in its own (width × height) pixel space —
            // rescale into OUR shared virtual arrangement (the combined
            // bounding box of every monitor's rect, side by side). With one
            // monitor (terminal mode, or a single DRM display) this box is
            // exactly that monitor's size, so the rescale is a no-op — same
            // behavior as before this had more than one monitor to place.

            // Captured BEFORE `state.pointer_global` is overwritten below —
            // this is "which monitor/position the cursor was actually drawn
            // at last frame", needed to damage exactly the region it's
            // leaving instead of the whole screen.
            let old_idx = active_monitor_idx(state);
            let old_pos = state.monitors[old_idx].pointer_pos;

            let total_w: u32 = state.monitors.iter().map(|m| m.output_w).sum();
            let total_h: u32 = state.monitors.iter().map(|m| m.output_h).max().unwrap_or(0);
            let gx = if width > 0 {
                x as f64 * total_w as f64 / width as f64
            } else {
                x as f64
            };
            let gy = if height > 0 {
                y as f64 * total_h as f64 / height as f64
            } else {
                y as f64
            };
            let gx = gx.clamp(0.0, total_w.saturating_sub(1) as f64);
            let gy = gy.clamp(0.0, total_h.saturating_sub(1) as f64);
            state.pointer_global = (gx, gy);

            // Which monitor that puts us on, then translate into ITS local
            // space — everything below this point (pick_focus, click math,
            // cursor blit) only ever deals in monitor-local coordinates, so
            // none of it needed to change for multi-monitor.
            let idx = active_monitor_idx(state);
            let m = &state.monitors[idx];
            let nx = (gx - m.rect.x as f64).clamp(0.0, m.output_w.saturating_sub(1) as f64);
            let ny = (gy - m.rect.y as f64).clamp(0.0, m.output_h.saturating_sub(1) as f64);
            state.monitors[idx].pointer_pos = (nx, ny);

            // Recompositing itself always redraws the whole frame regardless
            // of the damage rect (see composite_one_monitor) — `damage` here
            // only controls how much of the ALREADY-correct recomposited
            // buffer gets forwarded to the output backend. So a cursor-sized
            // rect is exactly as correct as the whole screen, just far
            // cheaper to blit. Margin is generous (not measured from the
            // actual cursor surface/hotspot) to safely cover the built-in
            // 12×16 fallback arrow and typical client cursor themes alike;
            // an unusually large custom client cursor could in principle
            // exceed it and leave a stale edge pixel behind — a cosmetic
            // edge case, not a correctness or crash issue.
            fn cursor_bbox(pos: (f64, f64)) -> Rect {
                Rect {
                    x: pos.0 as i32 - 16,
                    y: pos.1 as i32 - 16,
                    w: 64,
                    h: 64,
                }
            }
            if old_idx == idx {
                mark_dirty_rect(
                    state,
                    idx,
                    cursor_bbox(old_pos).union(&cursor_bbox((nx, ny))),
                );
            } else {
                // Cursor crossed onto a different monitor this event (rare —
                // only right at a monitor's edge): erase it from where it
                // was, draw it where it is now.
                mark_dirty_rect(state, old_idx, cursor_bbox(old_pos));
                mark_dirty_rect(state, idx, cursor_bbox((nx, ny)));
            }

            // Resolve focus: prefer the topmost popup under the cursor,
            // else the toplevel. Surface-local coords are (global - origin).
            let focus = pick_focus(state, idx, nx, ny);
            if focus.is_none() {
                tracing::debug!(
                    "motion ({:.0},{:.0}) on monitor {idx} → no focus (toplevels={}, buffers={})",
                    nx,
                    ny,
                    state.toplevels.len(),
                    state.surface_buffers.len()
                );
            }
            let ptr = state.pointer.clone();
            ptr.motion(
                state,
                focus,
                &MotionEvent {
                    location: (nx, ny).into(),
                    serial,
                    time,
                },
            );
            ptr.frame(state);
        }

        InputCmd::PointerButton { button, pressed } => {
            let idx = active_monitor_idx(state);
            let bs = if pressed {
                BState::Pressed
            } else {
                BState::Released
            };
            const BTN_LEFT: u32 = 0x110;

            // N7: the release of a left press we consumed (bar tile or modal)
            // never reaches a client.
            if !pressed && button == BTN_LEFT && state.swallow_left_release {
                state.swallow_left_release = false;
                return;
            }
            // A fresh left press that we do not consume must not inherit a
            // stale flag (its release would be dropped, sticking the button).
            if pressed && button == BTN_LEFT {
                state.swallow_left_release = false;
            }

            // Launcher and power menu are keyboard modals: while one is open,
            // clicks must not reach windows or the bar behind it.
            if state.launcher.is_some() || state.power_menu.is_some() {
                if pressed && button == BTN_LEFT {
                    state.swallow_left_release = true;
                }
                return;
            }

            // Wallpaper chooser is modal: a left click on a tile picks it,
            // a click off the panel closes it, and no button event (press or
            // release) reaches the windows behind it.
            if state.wallpaper_chooser.is_some() {
                if pressed && button == BTN_LEFT {
                    state.swallow_left_release = true;
                    let m = &state.monitors[idx];
                    let (px, py) = (m.pointer_pos.0 as i32, m.pointer_pos.1 as i32);
                    let hit = state
                        .wallpaper_chooser
                        .as_ref()
                        .map(|wc| wc.hit_test(px, py, m.output_w, m.output_h));
                    match hit {
                        Some(crate::wallpaper_chooser::Hit::Wallpaper(file_idx)) => {
                            state.wallpaper_chooser = None;
                            // Same as Enter: only the index moves, the
                            // rotation timer keeps its schedule.
                            state.wallpaper.jump_to(file_idx);
                            request_wallpaper(state);
                        }
                        Some(crate::wallpaper_chooser::Hit::Outside) => {
                            state.wallpaper_chooser = None;
                        }
                        _ => {}
                    }
                    mark_dirty_full(state);
                }
                return;
            }

            // H9: dismiss grabbing popups on an outside press. Before the bar
            // check below, since a bar click returns early.
            if pressed {
                let clicked = state.pointer.current_focus();
                dismiss_popup_grabs_on_press(state, clicked);
            }
            // Bar app tile — launch and stop here. A bar click has no
            // client surface behind it to focus or forward the event to,
            // so it takes a completely separate path from the click-to-
            // focus/forwarding below rather than falling through into it.
            let bar_hit = if pressed && button == BTN_LEFT {
                let (px, py) = (
                    state.monitors[idx].pointer_pos.0 as i32,
                    state.monitors[idx].pointer_pos.1 as i32,
                );
                state.monitors[idx]
                    .bar_hitboxes
                    .iter()
                    .find(|(r, _)| {
                        px >= r.x && py >= r.y && px < r.x + r.w as i32 && py < r.y + r.h as i32
                    })
                    .map(|(_, exec)| exec.clone())
            } else {
                None
            };

            if let Some(exec) = bar_hit {
                spawn_command(&state.socket_name, &exec);
                state.swallow_left_release = true;
                return;
            }

            // Left-click on a tile makes it the focused one — just follows
            // click focus, no position rearranging (that's what Alt+S is
            // for). Keeps `layout.focused` in sync with clicks so keyboard
            // nav/close (Alt+H/J/K/L/Q) act on whatever you last clicked,
            // not whatever keyboard nav last visited. A click inside the
            // already-focused tile hits `t_idx == layout.focused` and no-ops,
            // so ordinary clicks/typing inside an app are unaffected.
            if pressed && button == BTN_LEFT {
                let (px, py) = state.monitors[idx].pointer_pos;
                if let Some(t_idx) = toplevel_at(state, idx, px, py) {
                    if t_idx != state.monitors[idx].layout.focused {
                        state.monitors[idx].layout.focused = t_idx;
                        relayout(state);
                        refocus_keyboard(state);
                    }
                }
            }

            let focus = state.pointer.current_focus();
            tracing::info!(
                "button 0x{:x} pressed={} pos=({:.0},{:.0}) focus={:?}",
                button,
                pressed,
                state.monitors[idx].pointer_pos.0,
                state.monitors[idx].pointer_pos.1,
                focus.as_ref().map(|s| s.id()),
            );
            let ptr = state.pointer.clone();
            ptr.button(
                state,
                &ButtonEvent {
                    button,
                    state: bs,
                    serial,
                    time,
                },
            );
            ptr.frame(state);
        }

        InputCmd::Resize { width, height } => {
            // N8: a terminal shrunk to nothing reports 0 px. Zero extents flow
            // into relayout, the composite buffer and the encoders, so clamp
            // to a small floor here instead of trusting the sender.
            let (width, height) = (width.max(64), height.max(64));
            // Terminal-mode-only event (window resize) — terminal output is
            // always exactly one monitor, so this always targets
            // monitors[0] unconditionally rather than resolving
            // active_monitor_idx.
            let m = &mut state.monitors[0];
            // Rescale existing pointer position into the new pixel space so the
            // cursor doesn't jump on resize.
            if m.output_w > 0 && m.output_h > 0 {
                m.pointer_pos.0 = m.pointer_pos.0 * width as f64 / m.output_w as f64;
                m.pointer_pos.1 = m.pointer_pos.1 * height as f64 / m.output_h as f64;
            }
            m.output_w = width;
            m.output_h = height;
            m.rect.w = width;
            m.rect.h = height;
            let mode = OutputMode {
                size: (width as i32, height as i32).into(),
                refresh: state.output.current_mode().map_or(60_000, |m| m.refresh),
            };
            state
                .output
                .change_current_state(Some(mode), None, None, None);
            request_wallpaper(state);
            // Retile everyone into the new output extent.
            relayout(state);
        }

        InputCmd::SetMonitors { sizes, refresh_mhz } => {
            if sizes.is_empty() {
                tracing::warn!("SetMonitors: empty sizes list, ignoring");
                return;
            }
            let mut origin_x = 0i32;
            for (i, &(w, h)) in sizes.iter().enumerate() {
                match state.monitors.get_mut(i) {
                    // Already exists — almost always monitor 0, built from
                    // main.rs's startup guess before real output detection
                    // finished. Resize in place rather than replace, so
                    // anything already tiled on it (a window spawned in the
                    // brief window before this arrived) survives instead of
                    // being silently discarded.
                    Some(m) => {
                        m.rect = Rect {
                            x: origin_x,
                            y: 0,
                            w,
                            h,
                        };
                        m.output_w = w;
                        m.output_h = h;
                        m.pointer_pos.0 = m.pointer_pos.0.clamp(0.0, w.saturating_sub(1) as f64);
                        m.pointer_pos.1 = m.pointer_pos.1.clamp(0.0, h.saturating_sub(1) as f64);
                    }
                    // Genuinely new monitor — nothing was ever tiled on it
                    // (it didn't exist until this call), so a fresh default
                    // Monitor is exactly correct, not a discard of anything.
                    None => state.monitors.push(Monitor::new(w, h, origin_x)),
                }
                origin_x += w as i32;
            }
            // Only ever grows in practice (one SetMonitors call, once, at
            // startup) — truncate defensively rather than leave stale
            // entries with windows nothing would route to again, in case
            // this is ever called a second time with a smaller list.
            state.monitors.truncate(sizes.len());

            // Single shared wl_output global still reports monitor 0's mode
            // — see the note on `State.output`, this is a known
            // simplification (real per-monitor wl_output globals are
            // follow-up work, not part of this pass).
            let m0 = &state.monitors[0];
            let mode = OutputMode {
                size: (m0.output_w as i32, m0.output_h as i32).into(),
                refresh: refresh_mhz as i32,
            };
            state
                .output
                .change_current_state(Some(mode), None, None, None);
            state.composite_interval = composite_interval_for(state.fps_cfg, refresh_mhz);
            // First time through this also loads the configured wallpaper;
            // later (hotplug / mode change) it just re-scales for new sizes.
            let wp_cfg = veil_config::config_path()
                .and_then(|p| veil_config::try_load(&p).ok())
                .and_then(|c| c.wallpaper);
            state.wallpaper.configure(wp_cfg);
            request_wallpaper(state);

            // Newly added monitors have nothing tiled yet, so there's
            // nothing for relayout() to do on them — but they DO need their
            // first composite pass (background fill) so DRM actually shows
            // something instead of leaving their dumb buffer at its
            // startup-cleared state. mark_dirty_full covers every monitor.
            relayout(state);
            mark_dirty_full(state);
        }

        InputCmd::Scroll { v120 } => {
            if let Some(wc) = state.wallpaper_chooser.as_mut() {
                // Same direction convention as the launcher below.
                if v120 < 0 {
                    wc.move_down();
                } else if v120 > 0 {
                    wc.move_up();
                }
                mark_dirty_full(state);
                return;
            }
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
            if let Some(m) = state.power_menu.as_mut() {
                let count = crate::powermenu::POWER_ACTIONS.len();
                if v120 < 0 {
                    m.selected = (m.selected + 1).min(count - 1);
                } else if v120 > 0 {
                    m.selected = m.selected.saturating_sub(1);
                }
                mark_dirty_full(state);
                return;
            }

            // v120 = 120 per notch (Windows convention). Convert to a 15px-per-notch
            // continuous value as well; clients pick whichever they understand.
            let notches = v120 as f64 / 120.0;
            let mut f = AxisFrame::new(time).source(smithay::backend::input::AxisSource::Wheel);
            f.axis = (0.0, notches * 15.0);
            f.v120 = Some((0, v120));
            let ptr = state.pointer.clone();
            ptr.axis(state, f);
            ptr.frame(state);
        }

        InputCmd::Osd {
            title,
            body,
            progress,
        } => {
            state.show_osd(title, body, progress, Duration::from_secs(2));
        }
    }
}

// ─── Run loop ─────────────────────────────────────────────────────────────────

/// Bundle of state passed through calloop callbacks.
pub struct LoopData {
    pub state: State,
    pub display: Display<State>,
}

/// Shared by unmapped_window and destroyed_window — X11 can in some edge
/// cases destroy a window without a prior unmap, so both call this and it's
/// idempotent (retain on an already-removed entry is a no-op). Checks both
/// lists since we don't always know which one a given window landed in by
/// the time an unmap/destroy notification arrives.
fn remove_x11_window(state: &mut State, window: &X11Surface) {
    if let Some(wl) = window.wl_surface() {
        if let Some(idx) = state
            .monitors
            .iter()
            .position(|m| m.fullscreen.as_ref() == Some(&wl))
        {
            state.monitors[idx].fullscreen = None;
        }
    }

    let mut lost: Vec<usize> = state
        .toplevels
        .iter()
        .filter(|w| matches!(&w.surface, WindowSurface::X11(x) if x == window))
        .map(|w| w.monitor)
        .collect();
    if !lost.is_empty() {
        state
            .toplevels
            .retain(|w| !matches!(&w.surface, WindowSurface::X11(x) if x == window));
        lost.sort_unstable();
        lost.dedup();
        for idx in lost {
            if idx < state.monitors.len() {
                relayout_monitor(state, idx);
            }
        }
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
        self.xwm
            .as_mut()
            .expect("XwmHandler called before X11Wm started")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    // Nothing to do before it's actually mapped — X11 lets a client create a
    // window well before showing it, same as new_window.
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_mapped(true);
        let wl = window.wl_surface();
        let idx = active_monitor_idx(self);
        let ws = self.monitors[idx].active_workspace;
        self.toplevels
            .push(Window::new(WindowSurface::X11(window), idx, ws));
        let n = self
            .toplevels
            .iter()
            .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
            .count();
        self.monitors[idx].layout.focused = n.saturating_sub(1);
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
            if let Some(x) = x {
                geo.loc.x = x;
            }
            if let Some(y) = y {
                geo.loc.y = y;
            }
        }
        // Tiling WM: a *managed* window's position is always ours, never
        // the client's to set — x/y ignored for those (fall through, no
        // loc change above). Size honored either way, immediately, rather
        // than making the client wait for the next relayout tick (matches
        // smithay's own anvil reference).
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }

        // Heuristic for games/apps that go fullscreen by directly requesting
        // output-sized geometry instead of the proper EWMH
        // _NET_WM_STATE_FULLSCREEN path (fullscreen_request below) — same
        // trick i3 uses for legacy clients. Without this, a managed window
        // gets the size it asked for but keeps its old tiled x/y (position
        // is never honored for managed windows, see above), so it renders
        // output-sized but offset — visibly "half on screen".
        let idx = active_monitor_idx(self);
        let (out_w, out_h) = (self.monitors[idx].output_w, self.monitors[idx].output_h);
        if !or && w == Some(out_w) && h == Some(out_h) {
            if let Some(wl) = window.wl_surface() {
                let ws = self.monitors[idx].active_workspace;
                if let Some(i) = self
                    .toplevels
                    .iter()
                    .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
                    .position(|t| t.wl_surface().as_ref() == Some(&wl))
                {
                    self.monitors[idx].layout.focused = i;
                }
                self.monitors[idx].fullscreen = Some(wl);
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
    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edges: ResizeEdge,
    ) {
    }
    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(wl) = window.wl_surface() else {
            return;
        };
        let idx = window_monitor(self, &wl).unwrap_or_else(|| active_monitor_idx(self));
        let ws = self.monitors[idx].active_workspace;
        if let Some(i) = self
            .toplevels
            .iter()
            .filter(|t| t.alive() && t.monitor == idx && t.workspace == ws)
            .position(|t| t.wl_surface().as_ref() == Some(&wl))
        {
            self.monitors[idx].layout.focused = i;
        }
        self.monitors[idx].fullscreen = Some(wl);
        relayout_monitor(self, idx);
        refocus_keyboard(self);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        // Client-initiated — search rather than assume active monitor, same
        // reasoning as the xdg-shell unfullscreen_request above.
        if let Some(wl) = window.wl_surface() {
            if let Some(idx) = self
                .monitors
                .iter()
                .position(|m| m.fullscreen.as_ref() == Some(&wl))
            {
                self.monitors[idx].fullscreen = None;
                relayout_monitor(self, idx);
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
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        self.state.xwm_state(xwm)
    }
    fn new_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_window(xwm, window)
    }
    fn new_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_override_redirect_window(xwm, window)
    }
    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.map_window_request(xwm, window)
    }
    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.mapped_override_redirect_window(xwm, window)
    }
    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.unmapped_window(xwm, window)
    }
    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.destroyed_window(xwm, window)
    }
    fn configure_request(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        self.state
            .configure_request(xwm, window, x, y, w, h, reorder)
    }
    fn configure_notify(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<X11Window>,
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
    width: u32,
    height: u32,
    fps: u32,
    clipboard_sync: bool,
    spawn: Option<Vec<String>>,
    wayland_debug: bool,
    frame_tx: mpsc::Sender<Frame>,
    input_rx: mpsc::Receiver<InputCmd>,
    stop: Arc<AtomicBool>,
    keybinds: veil_config::Keybinds,
    background: [u8; 3],
    theme: veil_config::Theme,
    bar: veil_config::BarConfig,
) -> io::Result<()> {
    let composite_interval = composite_interval_for(fps, 60_000);
    let display: Display<State> =
        Display::new().map_err(|e| io::Error::other(format!("display: {e}")))?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<State>(&dh);
    let shm_state = ShmState::new::<State>(&dh, vec![]);
    let xdg_shell_state = XdgShellState::new::<State>(&dh);
    let xdg_activation = XdgActivationState::new::<State>(&dh);
    let output_manager = OutputManagerState::new_with_xdg_output::<State>(&dh);
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
    let mut dmabuf_state = DmabufState::new();
    let dmabuf_feedback = build_dmabuf_feedback(&gpu_probe);
    drop(gpu_probe); // real context recreated lazily by gpu_lazy() when actually needed

    // H8: feedback build failure now disables dmabuf (clients use shm) instead
    // of panicking the compositor at startup.
    let _dmabuf_global = dmabuf_feedback
        .map(|fb| dmabuf_state.create_global_with_default_feedback::<State>(&dh, &fb));
    // XWayland pairing: lets an X11 window's wl_surface get associated with
    // its X11Surface (see XWaylandShellHandler::surface_associated). Only
    // XWayland clients can bind this global.
    let xwayland_shell_state = XWaylandShellState::new::<State>(&dh);
    let _data_device = DataDeviceState::new::<State>(&dh);
    let _xdg_decoration = XdgDecorationState::new::<State>(&dh);
    let _viewporter = ViewporterState::new::<State>(&dh);
    let _fractional = FractionalScaleManagerState::new::<State>(&dh);
    // clk_id 1 = CLOCK_MONOTONIC.
    let _presentation = PresentationState::new::<State>(&dh, 1);
    let _text_input = TextInputManagerState::new::<State>(&dh);
    let _primary_sel = PrimarySelectionState::new::<State>(&dh);
    let _cursor_shape = CursorShapeManagerState::new::<State>(&dh);
    let _pointer_constraints = PointerConstraintsState::new::<State>(&dh);
    let _relative_pointer = RelativePointerManagerState::new::<State>(&dh);
    let _idle_inhibit = IdleInhibitManagerState::new::<State>(&dh);
    let _kb_inhibit = KeyboardShortcutsInhibitState::new::<State>(&dh);
    let _tablet = TabletManagerState::new::<State>(&dh);
    let mut seat_state = SeatState::<State>::new();
    let mut seat = seat_state.new_wl_seat(&dh, "veil-seat");
    let keyboard = seat
        .add_keyboard(XkbConfig::default(), 200, 16)
        .map_err(|e| io::Error::other(format!("keyboard: {e}")))?;
    let pointer = seat.add_pointer();

    let output = Output::new(
        "veil-host-0".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "veil".into(),
            model: "host".into(),
        },
    );
    let mode = OutputMode {
        size: (width as i32, height as i32).into(),
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    let _output_global = output.create_global::<State>(&dh);

    // Clipboard: poll the host compositor for clipboard changes every second.
    // Only offers host content when the hosted client has no active selection.
    let (clipboard_tx, clipboard_rx) = mpsc::channel::<String>();
    // N9: `clipboard_sync = false` never reads the host clipboard at all, so
    // nothing the user copies outside Veil is offered to clients inside it.
    if clipboard_sync {
        std::thread::spawn(move || {
        use std::io::Read;
        use wl_clipboard_rs::paste::{get_contents, ClipboardType, MimeType, Seat as PasteSeat};
        let mut last = String::new();
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            if let Ok((mut reader, _)) = get_contents(
                ClipboardType::Regular,
                PasteSeat::Unspecified,
                MimeType::Text,
            ) {
                // N9: bounded, so a huge or endless host selection cannot
                // make this thread buffer without limit.
                let mut buf = Vec::new();
                if reader
                    .by_ref()
                    .take(CLIP_MAX_BYTES as u64)
                    .read_to_end(&mut buf)
                    .is_ok()
                {
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if !text.is_empty() && text != last {
                        last = text.clone();
                        if clipboard_tx.send(text).is_err() {
                            break;
                        }
                    }
                }
            }
        }
        });
    }

    let config_path = veil_config::config_path();
    let config_mtime = config_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());

    let state = State {
        compositor_state,
        xdg_shell_state,
        shm_state,
        seat_state,
        xdg_activation,
        output_manager,
        dmabuf_state,
        _dmabuf_global,
        gpu: None, // stood up lazily by gpu_lazy() on first non-linear commit
        gpu_available,
        syncobj_state,
        _data_device,
        _xdg_decoration,
        _viewporter,
        _fractional,
        _presentation,
        _text_input,
        _primary_sel,
        _cursor_shape,
        _pointer_constraints,
        _relative_pointer,
        _idle_inhibit,
        _kb_inhibit,
        _tablet,
        seat,
        keyboard,
        pointer,
        output,
        // Startup guess — one monitor, sized to the terminal cell estimate
        // or CLI -w/-h. Rebuilt to match reality shortly after by
        // `InputCmd::SetMonitors` once main.rs's output detection
        // completes (that happens after Host::spawn, so this can't just
        // be sized correctly from the start).
        monitors: vec![Monitor::new(width, height, 0)],
        pointer_global: (width as f64 / 2.0, height as f64 / 2.0),
        toplevels: Vec::new(),
        floating: Vec::new(),
        xwm: None,
        xwayland_shell_state,
        keybinds,
        show_help: false,
        background: [background[0], background[1], background[2], 255],
        theme,
        bar,
        // No anchor client to spawn into (`veil-host start`) → open straight
        // to the launcher instead of an empty screen with no hint of what to press.
        launcher: if spawn.is_none() {
            Some(Launcher::new())
        } else {
            None
        },
        power_menu: None,
        wallpaper_chooser: None,
        media: crate::media::MediaWorker::spawn(),
        suppressed_keys: std::collections::HashSet::new(),
        swallow_left_release: false,
        socket_name: socket_name.to_string(),
        popups: PopupManager::default(),
        popup_grabs: Vec::new(),
        surface_buffers: HashMap::new(),
        loop_handle: None, // set right after the event loop is created
        cursor_status: CursorImageStatus::default_named(),
        focus_cache: None,
        dirty: false,
        frame_tx,
        serial_counter: 0,
        frame_serial: 0,
        running: true,
        stop: stop.clone(),
        start_time: Instant::now(),
        display_handle: dh.clone(),
        host_clipboard: None,
        clipboard_rx,
        pending_copy_out: false,
        client_has_selection: false,
        config_path,
        config_mtime,
        last_config_check: Instant::now(),
        composite_interval,
        fps_cfg: fps,
        clipboard_sync,
        wallpaper: crate::wallpaper::WallpaperEngine::new(),
        osd: None,
    };

    let mut data = LoopData { state, display };

    // ── Calloop event loop ────────────────────────────────────────────────────
    let mut event_loop: EventLoop<'static, LoopData> =
        EventLoop::try_new().map_err(|e| io::Error::other(format!("event_loop: {e}")))?;
    let handle = event_loop.handle();
    // S4: the surface pre-commit hook registers explicit-sync sources on this.
    data.state.loop_handle = Some(handle.clone());
    // Grabbed so the stop checks below can actually end `event_loop.run()` —
    // `calloop::EventLoop::run` only returns once this is told to stop; the
    // `stop`/`data.state.running` checks alone don't do that on their own,
    // they just decide whether to call it.
    let loop_signal = event_loop.get_signal();

    // 1. Wayland listening socket → accept clients.
    let listener_source = ListeningSocketSource::with_name(socket_name)
        .map_err(|e| io::Error::other(format!("bind {socket_name}: {e}")))?;
    let bound_name = listener_source.socket_name().to_os_string();
    handle
        .insert_source(listener_source, |stream, _, data| {
            let _ = data
                .display
                .handle()
                .insert_client(stream, Arc::new(ClientState::default()));
        })
        .map_err(|e| io::Error::other(format!("insert listener: {e}")))?;
    tracing::info!("listening on WAYLAND_DISPLAY={:?}", bound_name);

    // 2. Wayland display fd → dispatch protocol messages.
    let wl_fd = data.display.backend().poll_fd().as_raw_fd();
    let wl_src = Generic::new(
        unsafe { FdWrapper::new(wl_fd) },
        Interest::READ,
        CMode::Level,
    );
    handle
        .insert_source(wl_src, |_, _, data| {
            data.display
                .dispatch_clients(&mut data.state)
                .map_err(|e| {
                    tracing::error!("dispatch: {e}");
                    e
                })?;
            Ok(PostAction::Continue)
        })
        .map_err(|e| io::Error::other(format!("insert display: {e}")))?;

    // 3. XWayland — spawn it and listen for the Ready event so we can set
    //    DISPLAY for any X11 children we later spawn. Failure is non-fatal:
    //    if Xwayland isn't installed, we just lose X11 compat.
    let xwayland_display: Arc<std::sync::Mutex<Option<u32>>> =
        Arc::new(std::sync::Mutex::new(None));
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
            handle
                .insert_source(xwayland, move |event, _, data| match event {
                    XWaylandEvent::Ready {
                        x11_socket,
                        display_number,
                    } => {
                        tracing::info!("XWayland ready on DISPLAY=:{display_number}");
                        *xd.lock().unwrap() = Some(display_number);
                        XWAYLAND_DISPLAY.store(display_number, std::sync::atomic::Ordering::Relaxed);
                        match x_client.take() {
                            Some(client) => {
                                match X11Wm::start_wm(wm_handle.clone(), x11_socket, client) {
                                    Ok(wm) => {
                                        tracing::info!("X11 window manager started");
                                        data.state.xwm = Some(wm);
                                    }
                                    Err(e) => tracing::error!(
                                        "X11Wm::start_wm failed: {e} — X11 apps will not work"
                                    ),
                                }
                            }
                            None => tracing::warn!(
                                "XWaylandEvent::Ready fired more than once — ignoring"
                            ),
                        }
                    }
                    XWaylandEvent::Error => {
                        tracing::error!("XWayland startup failed");
                    }
                })
                .map_err(|e| io::Error::other(format!("insert xwayland: {e}")))?;
        }
        Err(e) => tracing::warn!("XWayland unavailable: {e} — X11 apps will not work"),
    }

    // 4. Periodic timer: drain input channel, send frame callbacks,
    //    flush clients, check stop flag. 8 ms tick = ~120 Hz ceiling.
    let socket_name_owned = socket_name.to_string();
    let stop_t = stop.clone();
    let loop_signal_t = loop_signal.clone();
    let tick = Timer::immediate();
    let tick_trace = std::env::var("VEIL_TICK_TRACE").is_ok();
    if tick_trace {
        eprintln!("[veil-tick] VEIL_TICK_TRACE on — phase timing every 120th tick (~1s)");
    }
    let mut tick_n: u32 = 0;
    let (mut acc_total, mut acc_total_max, mut acc_comp, mut acc_comp_max) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut acc_comp_n, mut acc_serial) = (0u32, 0u64);
    handle
        .insert_source(tick, move |_, _, data| {
            let t_start = tick_trace.then(std::time::Instant::now);
            // Prune toplevels that the client has destroyed. If any closed, retile
            // the survivors and move keyboard focus onto one of them.
            let mut lost: Vec<usize> = data
                .state
                .toplevels
                .iter()
                .filter(|t| !t.alive())
                .map(|t| t.monitor)
                .collect();
            if !lost.is_empty() {
                data.state.toplevels.retain(|t| t.alive());
                // S11: retile each monitor that lost a window, not only the
                // active one, so a close elsewhere leaves no gap.
                lost.sort_unstable();
                lost.dedup();
                for idx in lost {
                    if idx < data.state.monitors.len() {
                        relayout_monitor(&mut data.state, idx);
                    }
                }
                refocus_keyboard(&mut data.state);
            }
            // If the fullscreen window was among those just pruned, don't leave
            // `fullscreen` pointing at a dead surface — relayout() already
            // treats a not-found fullscreen surface as "nothing fullscreen", so
            // this is tidiness (no stale handle retained), not a correctness
            // fix. Every monitor has its own slot to check now.
            for m in data.state.monitors.iter_mut() {
                if let Some(wl) = &m.fullscreen {
                    if !data
                        .state
                        .toplevels
                        .iter()
                        .any(|t| t.wl_surface().as_ref() == Some(wl))
                    {
                        m.fullscreen = None;
                    }
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
                // N9: with clipboard sync off nothing leaves Veil either.
                let mimes: &[&str] = if data.state.clipboard_sync {
                    &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"]
                } else {
                    &[]
                };
                for &mime in mimes {
                    let mut fds = [-1i32; 2];
                    let ok = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) == 0 };
                    if !ok {
                        break;
                    }
                    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
                    if request_data_device_client_selection::<State>(
                        &seat,
                        mime.to_string(),
                        write_fd,
                    )
                    .is_ok()
                    {
                        std::thread::spawn(move || {
                            // S9: bounded read — a client that never closes
                            // the pipe no longer pins this thread forever.
                            let Some(buf) = clip_read_bounded(read_fd) else {
                                return;
                            };
                            if buf.is_empty() {
                                return;
                            }
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
                while let Ok(text) = data.state.clipboard_rx.try_recv() {
                    latest = Some(text);
                }
                if let Some(text) = latest {
                    if data.state.host_clipboard.as_deref() != Some(&text) {
                        data.state.host_clipboard = Some(text);
                        let dh = data.display.handle();
                        let seat = data.state.seat.clone();
                        set_data_device_selection::<State>(
                            &dh,
                            &seat,
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
                let current_mtime = current_path
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .and_then(|m| m.modified().ok());

                if current_path != data.state.config_path
                    || current_mtime != data.state.config_mtime
                {
                    reload_config(&mut data.state);
                }
            }

            // Composite all dirty surfaces into one RGBA frame and ship it.
            // Frame callbacks are fired inside composite_and_send after the frame is sent.
            wallpaper_tick(&mut data.state);
            media_tick(&mut data.state);
            let t_pre_composite = tick_trace.then(std::time::Instant::now);
            composite_and_send(&mut data.state);
            let t_post_composite = tick_trace.then(std::time::Instant::now);

            // Flush outgoing wayland messages.
            let _ = data.display.flush_clients();
            let t_end = tick_trace.then(std::time::Instant::now);

            if tick_trace {
                tick_n = tick_n.wrapping_add(1);
                if let (Some(s), Some(pc), Some(poc), Some(e)) =
                    (t_start, t_pre_composite, t_post_composite, t_end)
                {
                    // Aggregate over the whole ~1 s window. The old output
                    // printed ONE tick in 120, which at idle is nearly always
                    // a 0.00ms no-op and says nothing about load.
                    let total = (e - s).as_secs_f64() * 1000.0;
                    let comp = (poc - pc).as_secs_f64() * 1000.0;
                    acc_total += total;
                    acc_total_max = acc_total_max.max(total);
                    if comp > 0.0 {
                        acc_comp += comp;
                        acc_comp_n += 1;
                        acc_comp_max = acc_comp_max.max(comp);
                    }
                }
                if tick_n.is_multiple_of(120) {
                    let serial = data.state.frame_serial;
                    let bp = BP_SKIPS.swap(0, Ordering::Relaxed);
                    eprintln!(
                        "[veil-tick] 120 ticks: frames_sent={} backpressure_skips={} \
                         composite avg={:.2}ms max={:.2}ms (n={}) tick avg={:.2}ms max={:.2}ms",
                        serial.wrapping_sub(acc_serial),
                        bp,
                        if acc_comp_n > 0 { acc_comp / acc_comp_n as f64 } else { 0.0 },
                        acc_comp_max,
                        acc_comp_n,
                        acc_total / 120.0,
                        acc_total_max,
                    );
                    acc_serial = serial;
                    acc_total = 0.0;
                    acc_total_max = 0.0;
                    acc_comp = 0.0;
                    acc_comp_max = 0.0;
                    acc_comp_n = 0;
                }
            }

            if stop_t.load(Ordering::Relaxed) || !data.state.running {
                loop_signal_t.stop();
                TimeoutAction::Drop
            } else {
                // Half the composite period, so one tick of jitter can't drop a
                // frame: 8.3 ms at 60 Hz (was a fixed 8 ms, i.e. a ~125 fps
                // ceiling), ~2.8 ms at 180 Hz. Re-read each tick so a config
                // reload or the real refresh arriving takes effect.
                TimeoutAction::ToDuration(
                    (data.state.composite_interval / 2)
                        .clamp(Duration::from_millis(2), Duration::from_millis(8)),
                )
            }
        })
        .map_err(|e| io::Error::other(format!("insert tick: {e}")))?;

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
                    std::thread::spawn(move || match child.wait() {
                        Ok(s) => tracing::info!("anchor client exited: {s} (veil keeps running)"),
                        Err(e) => tracing::error!("anchor client wait: {e}"),
                    });
                }
                Err(e) => tracing::error!("spawn {:?} failed: {e}", argv),
            }
        }
    }

    // ── Drive the loop ────────────────────────────────────────────────────────
    event_loop
        .run(Some(Duration::from_millis(16)), &mut data, |data| {
            // Post-dispatch hook: flush again to push anything generated during dispatch.
            let _ = data.display.flush_clients();
            if stop.load(Ordering::Relaxed) || !data.state.running {
                data.state.running = false;
                loop_signal.stop();
            }
        })
        .map_err(|e| io::Error::other(format!("run: {e}")))?;

    // Belt-and-suspenders: every intentional shutdown path (HOME, Ctrl-C)
    // already calls this directly, and crashes go through the panic
    // hook/signal handlers — but cover whatever exit route got us here too.
    // Idempotent; unlinking an already-gone socket is a harmless no-op.
    crate::vt::emergency_restore();

    Ok(())
}

#[cfg(test)]
mod env_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn inhibit_escape_is_alt_shift_escape_only() {
        use smithay::input::keyboard::keysyms::{KEY_Escape, KEY_Tab};
        assert!(is_inhibit_escape(KEY_Escape, true, true));
        assert!(!is_inhibit_escape(KEY_Escape, true, false));
        assert!(!is_inhibit_escape(KEY_Escape, false, true));
        assert!(!is_inhibit_escape(KEY_Tab, true, true));
    }

    #[test]
    fn xwayland_display_goes_to_children_not_our_environment() {
        let before = std::env::var_os("DISPLAY");
        XWAYLAND_DISPLAY.store(7, Ordering::Relaxed);
        let mut cmd = Command::new("true");
        apply_wayland_env(&mut cmd, "wayland-9");
        XWAYLAND_DISPLAY.store(XWAYLAND_UNSET, Ordering::Relaxed);
        let env = |k: &str| {
            cmd.get_envs()
                .find(|(key, _)| *key == k)
                .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        assert_eq!(env("DISPLAY").as_deref(), Some(":7"));
        assert_eq!(env("WAYLAND_DISPLAY").as_deref(), Some("wayland-9"));
        assert_eq!(std::env::var_os("DISPLAY"), before, "process env untouched");
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use smithay::wayland::shm::BufferData;

    fn bd(w: i32, h: i32, stride: i32, offset: i32, format: wl_shm::Format) -> BufferData {
        BufferData {
            offset,
            width: w,
            height: h,
            stride,
            format,
        }
    }

    #[test]
    fn shm_argb_swaps_channels_and_keeps_alpha() {
        let raw = [10u8, 20, 30, 128, 1, 2, 3, 255]; // two BGRA px
        let mut out = Vec::new();
        let got = shm_to_rgba_into(&raw, &bd(2, 1, 8, 0, wl_shm::Format::Argb8888), &mut out);
        assert_eq!(got, Some((2, 1)));
        assert_eq!(out, [30, 20, 10, 128, 3, 2, 1, 255]);
    }

    #[test]
    fn shm_x_formats_force_opaque() {
        let raw = [10u8, 20, 30, 0];
        let mut out = Vec::new();
        shm_to_rgba_into(&raw, &bd(1, 1, 4, 0, wl_shm::Format::Xrgb8888), &mut out).unwrap();
        assert_eq!(out, [30, 20, 10, 255]);
        shm_to_rgba_into(&raw, &bd(1, 1, 4, 0, wl_shm::Format::Xbgr8888), &mut out).unwrap();
        assert_eq!(out, [10, 20, 30, 255]);
    }

    #[test]
    fn shm_offset_is_part_of_the_bounds_check() {
        let mut out = Vec::new();
        let raw = [9u8, 9, 9, 9, 1, 2, 3, 4];
        let f = wl_shm::Format::Abgr8888;
        assert!(shm_to_rgba_into(&raw, &bd(1, 1, 4, 4, f), &mut out).is_some());
        assert_eq!(out, [1, 2, 3, 4]);
        // offset + row no longer fits: must be None, not a slice panic (S7).
        assert!(shm_to_rgba_into(&raw[..7], &bd(1, 1, 4, 4, f), &mut out).is_none());
        // last row of a multi-row buffer overruns once offset is added
        assert!(shm_to_rgba_into(&raw, &bd(1, 2, 4, 4, f), &mut out).is_none());
    }

    #[test]
    fn shm_rejects_hostile_geometry_without_touching_out() {
        let raw = [0u8; 64];
        let f = wl_shm::Format::Argb8888;
        let mut out = vec![1u8, 2, 3];
        for d in [
            bd(-1, 1, 4, 0, f),
            bd(1, -1, 4, 0, f),
            bd(1, 1, -4, 0, f),
            bd(1, 1, 4, -4, f),
            bd(0, 1, 4, 0, f),
            bd(4, 1, 8, 0, f), // stride < row bytes
            bd(i32::MAX, i32::MAX, i32::MAX, i32::MAX, f),
            bd(1, 1, 4, 0, wl_shm::Format::Rgb565), // unsupported format
        ] {
            assert!(shm_to_rgba_into(&raw, &d, &mut out).is_none());
            assert_eq!(out, [1, 2, 3], "dest must survive a rejected buffer (S5)");
        }
    }

    #[test]
    fn surface_size_caps() {
        assert!(surface_size_ok(1920, 1080));
        assert!(!surface_size_ok(0, 10));
        assert!(!surface_size_ok(MAX_SURFACE_DIM + 1, 1));
        assert!(!surface_size_ok(usize::MAX, usize::MAX));
    }

    fn buf(px: [u8; 4]) -> SurfaceBuf {
        SurfaceBuf { rgba: px.to_vec(), w: 1, h: 1 }
    }

    #[test]
    fn blit_opaque_transparent_and_premultiplied() {
        let mut back = vec![100u8, 100, 100, 255];
        blit(&mut back, 1, 1, &buf([7, 8, 9, 255]), 0, 0);
        assert_eq!(back, [7, 8, 9, 255]);

        let mut back = vec![100u8, 100, 100, 255];
        blit(&mut back, 1, 1, &buf([50, 50, 50, 0]), 0, 0);
        assert_eq!(back, [100, 100, 100, 255], "alpha 0 leaves dst alone");

        // premultiplied: out = src + dst * (255 - a) / 255 = 50 + 100 * 127 / 255
        let mut back = vec![100u8, 100, 100, 255];
        blit(&mut back, 1, 1, &buf([50, 50, 50, 128]), 0, 0);
        assert_eq!(back, [99, 99, 99, 255]);

        // colour brighter than alpha (invalid but seen) clamps instead of wrapping
        let mut back = vec![200u8, 200, 200, 255];
        blit(&mut back, 1, 1, &buf([250, 250, 250, 128]), 0, 0);
        assert_eq!(back, [255, 255, 255, 255]);
    }

    #[test]
    fn blit_clips_to_the_back_buffer() {
        let mut back = vec![0u8; 2 * 2 * 4];
        let src = SurfaceBuf { rgba: vec![255; 3 * 3 * 4], w: 3, h: 3 };
        blit(&mut back, 2, 2, &src, -1, -1); // overhangs top-left
        blit(&mut back, 2, 2, &src, 5, 5); // entirely outside: no-op, no panic
        assert!(back.iter().all(|&b| b == 255));
    }
}
