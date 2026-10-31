//! System tray: StatusNotifierItem icons (the protocol Electron, Qt and GTK
//! apps use for their tray icons).
//!
//! One worker thread talks to the session bus. If nothing owns
//! `org.kde.StatusNotifierWatcher` the bar becomes the watcher itself (so
//! apps have somewhere to register); if another shell already does, the bar
//! just hosts the items that watcher lists. Icons come from the item's
//! `IconPixmap` property, or are looked up on disk by `IconName`; anything
//! else gets a lettered placeholder. Left click sends `Activate`.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_skia::{FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};
use veil_text::{Role, TextSystem};
use veil_ui::{fill_rounded_rect, Action, Metrics, Theme, Widget};

const POLL: Duration = Duration::from_secs(2);
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";

// ─── data ────────────────────────────────────────────────────────────────────

/// Premultiplied RGBA, the layout tiny-skia wants.
#[derive(Debug, Clone, PartialEq)]
pub struct Icon {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrayItem {
    pub bus: String,
    pub path: String,
    pub title: String,
    pub icon: Option<Icon>,
}

#[derive(Default)]
struct Shared {
    items: Mutex<Vec<TrayItem>>,
    version: AtomicU64,
}

enum Cmd {
    Activate { bus: String, path: String },
}

// ─── pure helpers ────────────────────────────────────────────────────────────

/// Split what an app registered into (bus name, object path). Apps pass a
/// bus name, a path (the bus name is then the message sender), or
/// `name/path`.
pub fn parse_service(service: &str, sender: Option<&str>) -> Option<(String, String)> {
    if service.starts_with('/') {
        return Some((sender?.to_string(), service.to_string()));
    }
    match service.split_once('/') {
        Some((bus, rest)) if !bus.is_empty() => Some((bus.to_string(), format!("/{rest}"))),
        Some(_) => None,
        None if service.is_empty() => None,
        None => Some((service.to_string(), DEFAULT_ITEM_PATH.to_string())),
    }
}

/// The string a registered item is stored and announced as.
fn key_of(bus: &str, path: &str) -> String {
    format!("{bus}{path}")
}

/// StatusNotifier pixmaps are ARGB32 in network byte order (bytes A,R,G,B).
/// Convert to premultiplied RGBA. `None` for sizes that do not match.
pub fn argb_to_premul_rgba(w: i32, h: i32, data: &[u8]) -> Option<Icon> {
    if w <= 0 || h <= 0 || data.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    for px in data.as_chunks::<4>().0 {
        let (a, r, g, b) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        out.extend_from_slice(&[(r * a / 255) as u8, (g * a / 255) as u8, (b * a / 255) as u8, a as u8]);
    }
    Some(Icon { w: w as u32, h: h as u32, rgba: out })
}

/// The pixmap to show out of the sizes an item offers: the smallest one at
/// least `want` px, else the largest.
pub fn pick_pixmap(pixmaps: Vec<(i32, i32, Vec<u8>)>, want: i32) -> Option<Icon> {
    let mut sized: Vec<_> = pixmaps.into_iter().filter(|(w, h, d)| *w > 0 && *h > 0 && d.len() == (*w * *h * 4) as usize).collect();
    sized.sort_by_key(|(w, _, _)| *w);
    let chosen = sized.iter().find(|(w, _, _)| *w >= want).or_else(|| sized.last())?;
    argb_to_premul_rgba(chosen.0, chosen.1, &chosen.2)
}

/// Find an icon file for `name`: an absolute path, a file in the item's own
/// `IconThemePath`, or a PNG in the hicolor/Adwaita themes and pixmaps.
pub fn find_icon_file(name: &str, theme_path: &str, roots: &[PathBuf], want: u32) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let abs = Path::new(name);
    if abs.is_absolute() {
        return abs.is_file().then(|| abs.to_path_buf());
    }
    if !theme_path.is_empty() {
        let p = Path::new(theme_path).join(format!("{name}.png"));
        if p.is_file() {
            return Some(p);
        }
    }
    const SIZES: [u32; 9] = [16, 22, 24, 32, 48, 64, 96, 128, 256];
    let mut found: Vec<(u32, PathBuf)> = Vec::new();
    for root in roots {
        for theme in ["hicolor", "Adwaita"] {
            for size in SIZES {
                let p = root.join(theme).join(format!("{size}x{size}")).join("apps").join(format!("{name}.png"));
                if p.is_file() {
                    found.push((size, p));
                }
            }
        }
    }
    found.sort_by_key(|(s, _)| *s);
    if let Some((_, p)) = found.iter().find(|(s, _)| *s >= want).or_else(|| found.last()) {
        return Some(p.clone());
    }
    let pix = PathBuf::from("/usr/share/pixmaps").join(format!("{name}.png"));
    pix.is_file().then_some(pix)
}

pub fn load_png(path: &Path) -> Option<Icon> {
    let mut decoder = png::Decoder::new(BufReader::new(File::open(path).ok()?));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let data = &buf[..info.buffer_size()];
    let mut rgba = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    match info.color_type {
        png::ColorType::Rgba => {
            for p in data.as_chunks::<4>().0 {
                let a = p[3] as u32;
                rgba.extend_from_slice(&[(p[0] as u32 * a / 255) as u8, (p[1] as u32 * a / 255) as u8, (p[2] as u32 * a / 255) as u8, p[3]]);
            }
        }
        png::ColorType::Rgb => {
            for p in data.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for p in data.as_chunks::<2>().0 {
                let a = p[1] as u32;
                let v = (p[0] as u32 * a / 255) as u8;
                rgba.extend_from_slice(&[v, v, v, p[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for &v in data {
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        png::ColorType::Indexed => return None, // normalize_to_color8 expands these
    }
    Some(Icon { w: info.width, h: info.height, rgba })
}

fn icon_roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        v.push(PathBuf::from(home).join(".local/share/icons"));
    }
    v.push(PathBuf::from("/usr/share/icons"));
    v
}

// ─── widget ──────────────────────────────────────────────────────────────────

struct Slot {
    x0: f32,
    x1: f32,
    bus: String,
    path: String,
}

pub struct SystrayWidget {
    shared: Arc<Shared>,
    seen: u64,
    tx: mpsc::Sender<Cmd>,
    /// Pixmaps for the current items, rebuilt when the worker publishes.
    items: Vec<(TrayItem, Option<Pixmap>)>,
    slots: Vec<Slot>,
}

impl Default for SystrayWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl SystrayWidget {
    pub fn new() -> Self {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel();
        let worker = shared.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("veil-bar-tray".into())
            .spawn(move || futures_lite::future::block_on(worker_loop(worker, rx)))
        {
            tracing::warn!("bar: could not start the tray worker: {e}");
        }
        Self { shared, seen: 0, tx, items: Vec::new(), slots: Vec::new() }
    }
}

impl Widget for SystrayWidget {
    fn name(&self) -> &str {
        "systray"
    }

    fn update(&mut self, _now: Instant) -> bool {
        let v = self.shared.version.load(Ordering::Acquire);
        if v == self.seen {
            return false;
        }
        self.seen = v;
        let items = self.shared.items.lock().unwrap_or_else(|e| e.into_inner()).clone();
        self.items = items
            .into_iter()
            .map(|it| {
                let pm = it
                    .icon
                    .as_ref()
                    .and_then(|i| Pixmap::from_vec(i.rgba.clone(), IntSize::from_wh(i.w, i.h)?));
                (it, pm)
            })
            .collect();
        true
    }

    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &mut TextSystem,
        theme: &Theme,
        m: &Metrics,
        x: f32,
        y: f32,
        h: f32,
    ) -> f32 {
        if self.items.is_empty() {
            self.slots.clear();
            return 0.0;
        }
        let size = (h - m.px(12.0)).max(m.px(12.0));
        let gap = m.px(6.0);
        let top = y + (h - size) / 2.0;
        let mut cx = x + m.pad * 0.5;
        self.slots.clear();
        for (item, icon) in &self.items {
            match icon {
                Some(ic) => {
                    let s = size / ic.width().max(ic.height()) as f32;
                    pixmap.draw_pixmap(
                        0,
                        0,
                        ic.as_ref(),
                        &PixmapPaint { quality: FilterQuality::Bilinear, ..Default::default() },
                        Transform::from_row(s, 0.0, 0.0, s, cx, top),
                        None,
                    );
                }
                None => {
                    // Placeholder: the first letter of the title on a pill.
                    fill_rounded_rect(pixmap, cx, top, size, size, m.radius, theme.surface_hi);
                    let letter: String = item.title.chars().next().unwrap_or('?').to_uppercase().collect();
                    let (lw, _) = text.measure(&letter, Role::UI, m.small);
                    text.draw(pixmap, &letter, cx + (size - lw) / 2.0, top + (size - m.small) / 2.0, Role::UI, m.small, theme.text);
                }
            }
            self.slots.push(Slot { x0: cx - x, x1: cx - x + size, bus: item.bus.clone(), path: item.path.clone() });
            cx += size + gap;
        }
        cx - x - gap + m.pad * 1.5
    }

    fn on_click(&mut self, x: f32) -> Option<Action> {
        let slot = self.slots.iter().find(|s| x >= s.x0 && x < s.x1 + 3.0)?;
        let _ = self.tx.send(Cmd::Activate { bus: slot.bus.clone(), path: slot.path.clone() });
        None
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ─── the watcher we serve when nobody else does ─────────────────────────────

struct Watcher {
    items: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &mut self,
        service: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        let sender = header.sender().map(|s| s.as_str().to_string());
        let Some((bus, path)) = parse_service(service, sender.as_deref()) else {
            return;
        };
        let key = key_of(&bus, &path);
        let added = {
            let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
            if items.contains(&key) {
                false
            } else {
                items.push(key.clone());
                true
            }
        };
        if added {
            let _ = Self::status_notifier_item_registered(&emitter, &key).await;
        }
    }

    fn register_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(emitter: &zbus::object_server::SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(emitter: &zbus::object_server::SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

// ─── worker ──────────────────────────────────────────────────────────────────

/// Where the list of items comes from.
enum Source {
    /// We are the watcher: its list is local.
    Own(Arc<Mutex<Vec<String>>>),
    /// Someone else is: read their `RegisteredStatusNotifierItems`.
    Remote,
}

async fn connect() -> Option<(zbus::Connection, Source)> {
    let list = Arc::new(Mutex::new(Vec::new()));
    let own = async {
        zbus::connection::Builder::session()
            .name(WATCHER_NAME)
            .serve_at(WATCHER_PATH, Watcher { items: list.clone() })
            .build()
            .await
            .ok()
    };
    if let Some(conn) = own.await {
        // Announce a host so apps that check for one start registering.
        if let Ok(iface) = conn.object_server().interface::<_, Watcher>(WATCHER_PATH).await {
            let _ = Watcher::status_notifier_host_registered(iface.signal_emitter()).await;
        }
        return Some((conn, Source::Own(list)));
    }
    let conn = zbus::Connection::session().await.ok()?;
    Some((conn, Source::Remote))
}

async fn registered_items(conn: &zbus::Connection, source: &Source) -> Vec<String> {
    match source {
        Source::Own(list) => list.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        Source::Remote => {
            let Ok(p) = zbus::Proxy::new(conn, WATCHER_NAME, WATCHER_PATH, WATCHER_NAME).await else {
                return Vec::new();
            };
            p.get_property::<Vec<String>>("RegisteredStatusNotifierItems").await.unwrap_or_default()
        }
    }
}

/// What a poll of one registered item found.
enum Probe {
    Shown(TrayItem),
    /// Alive but asked not to be shown (`Status` is `Passive`).
    Hidden,
    /// Not answering: its app quit.
    Dead,
}

async fn read_item(conn: &zbus::Connection, bus: &str, path: &str, roots: &[PathBuf], want: u32) -> Probe {
    let Ok(p) = zbus::Proxy::new(conn, bus.to_string(), path.to_string(), ITEM_IFACE).await else {
        return Probe::Dead;
    };
    let status = p.get_property::<String>("Status").await.ok();
    let title = p.get_property::<String>("Title").await.unwrap_or_default();
    if status.as_deref() == Some("Passive") {
        return Probe::Hidden;
    }
    let mut icon = None;
    if let Ok(px) = p.get_property::<Vec<(i32, i32, Vec<u8>)>>("IconPixmap").await {
        icon = pick_pixmap(px, want as i32);
    }
    if icon.is_none() {
        let name = p.get_property::<String>("IconName").await.unwrap_or_default();
        let theme = p.get_property::<String>("IconThemePath").await.unwrap_or_default();
        icon = find_icon_file(&name, &theme, roots, want).and_then(|f| load_png(&f));
    }
    if status.is_none() && icon.is_none() && title.is_empty() {
        return Probe::Dead;
    }
    Probe::Shown(TrayItem { bus: bus.into(), path: path.into(), title, icon })
}

async fn worker_loop(shared: Arc<Shared>, cmds: mpsc::Receiver<Cmd>) {
    let Some((conn, source)) = connect().await else {
        tracing::info!("bar: tray off, no session bus");
        return;
    };
    let roots = icon_roots();
    // Ask for icons around the size the bar draws; a little oversized
    // downscales better than undersized upscales.
    let want = 32;
    loop {
        let mut items = Vec::new();
        let mut dead = Vec::new();
        for key in registered_items(&conn, &source).await {
            let Some((bus, path)) = parse_service(&key, None) else { continue };
            match read_item(&conn, &bus, &path, &roots, want).await {
                Probe::Shown(it) => items.push(it),
                Probe::Hidden => {}
                Probe::Dead => dead.push(key),
            }
        }
        // Items whose app quit leave our own list (a remote watcher prunes
        // its own). Hidden items stay: they are alive, just not shown.
        if let Source::Own(list) = &source {
            if !dead.is_empty() {
                list.lock().unwrap_or_else(|e| e.into_inner()).retain(|k| !dead.contains(k));
            }
        }
        publish(&shared, items);

        match cmds.recv_timeout(POLL) {
            Ok(Cmd::Activate { bus, path }) => {
                eprintln!("DBG worker got Activate {bus} {path}");
                eprintln!("DBG worker got Activate {bus} {path}");
                if let Ok(p) = zbus::Proxy::new(&conn, bus.clone(), path, ITEM_IFACE).await {
                    if let Err(e) = p.call_method("Activate", &(0i32, 0i32)).await {
                        eprintln!("DBG tray Activate on {bus} failed: {e}"); eprintln!("DBG tray Activate on {bus} failed: {e}"); tracing::debug!("bar: tray Activate on {bus} failed: {e}");
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn publish(shared: &Shared, items: Vec<TrayItem>) {
    let mut g = shared.items.lock().unwrap_or_else(|e| e.into_inner());
    if *g != items {
        *g = items;
        shared.version.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_registration_forms() {
        assert_eq!(parse_service("/org/ayatana/NotificationItem/x", Some(":1.7")), Some((":1.7".into(), "/org/ayatana/NotificationItem/x".into())));
        assert_eq!(parse_service("org.kde.StatusNotifierItem-12-1", Some(":1.7")), Some(("org.kde.StatusNotifierItem-12-1".into(), "/StatusNotifierItem".into())));
        assert_eq!(parse_service(":1.9/StatusNotifierItem", None), Some((":1.9".into(), "/StatusNotifierItem".into())));
        assert_eq!(parse_service("/path", None), None, "a bare path needs a sender");
        assert_eq!(parse_service("", Some(":1.1")), None);
        assert_eq!(parse_service("/x", Some(":1.1")).map(|(b, _)| b), Some(":1.1".into()));
    }

    #[test]
    fn key_round_trips_through_parse() {
        let k = key_of(":1.55", "/StatusNotifierItem");
        assert_eq!(parse_service(&k, None), Some((":1.55".into(), "/StatusNotifierItem".into())));
    }

    #[test]
    fn argb_becomes_premultiplied_rgba() {
        // one opaque red pixel, one half-transparent white pixel (A,R,G,B)
        let icon = argb_to_premul_rgba(2, 1, &[255, 255, 0, 0, 128, 255, 255, 255]).unwrap();
        assert_eq!(icon.rgba, vec![255, 0, 0, 255, 128, 128, 128, 128]);
        assert_eq!((icon.w, icon.h), (2, 1));
    }

    #[test]
    fn malformed_pixmaps_are_rejected() {
        assert!(argb_to_premul_rgba(2, 2, &[0; 4]).is_none());
        assert!(argb_to_premul_rgba(0, 1, &[]).is_none());
        assert!(argb_to_premul_rgba(-1, 1, &[0; 4]).is_none());
    }

    #[test]
    fn picks_the_smallest_pixmap_that_is_big_enough() {
        let px = |s: i32| (s, s, vec![255; (s * s * 4) as usize]);
        assert_eq!(pick_pixmap(vec![px(48), px(16), px(24)], 20).unwrap().w, 24);
        assert_eq!(pick_pixmap(vec![px(16), px(24)], 64).unwrap().w, 24, "falls back to the largest");
        assert!(pick_pixmap(vec![], 16).is_none());
        // a pixmap whose byte count lies about its size is ignored
        assert!(pick_pixmap(vec![(8, 8, vec![0; 4])], 8).is_none());
    }

    fn write_png(path: &Path, w: u32, h: u32, color: png::ColorType, data: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut enc = png::Encoder::new(File::create(path).unwrap(), w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(data).unwrap();
    }

    #[test]
    fn loads_rgba_and_rgb_pngs() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a.png");
        write_png(&a, 1, 1, png::ColorType::Rgba, &[200, 100, 50, 128]);
        let icon = load_png(&a).unwrap();
        assert_eq!(icon.rgba, vec![100, 50, 25, 128], "premultiplied");
        let b = t.path().join("b.png");
        write_png(&b, 1, 1, png::ColorType::Rgb, &[1, 2, 3]);
        assert_eq!(load_png(&b).unwrap().rgba, vec![1, 2, 3, 255]);
        assert!(load_png(&t.path().join("missing.png")).is_none());
    }

    #[test]
    fn icon_lookup_prefers_a_big_enough_size_then_pixmaps() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("icons");
        for s in [16, 48, 128] {
            write_png(&root.join(format!("hicolor/{s}x{s}/apps/foo.png")), 1, 1, png::ColorType::Rgb, &[0, 0, 0]);
        }
        let found = find_icon_file("foo", "", std::slice::from_ref(&root), 32).unwrap();
        assert!(found.to_string_lossy().contains("48x48"));
        let largest = find_icon_file("foo", "", std::slice::from_ref(&root), 512).unwrap();
        assert!(largest.to_string_lossy().contains("128x128"));
        assert!(find_icon_file("nope", "", &[root], 32).is_none());
        assert!(find_icon_file("", "", &[], 32).is_none());
    }

    #[test]
    fn icon_lookup_honours_the_items_own_theme_path_and_absolute_names() {
        let t = tempfile::tempdir().unwrap();
        let own = t.path().join("own");
        write_png(&own.join("bar.png"), 1, 1, png::ColorType::Rgb, &[0, 0, 0]);
        assert_eq!(find_icon_file("bar", own.to_str().unwrap(), &[], 32), Some(own.join("bar.png")));
        let abs = own.join("bar.png");
        assert_eq!(find_icon_file(abs.to_str().unwrap(), "", &[], 32), Some(abs));
        assert!(find_icon_file("/no/such/file.png", "", &[], 32).is_none());
    }

    #[test]
    fn publishing_only_bumps_the_version_on_change() {
        let s = Shared::default();
        publish(&s, Vec::new());
        assert_eq!(s.version.load(Ordering::Relaxed), 0);
        let item = TrayItem { bus: ":1.1".into(), path: "/x".into(), title: "t".into(), icon: None };
        publish(&s, vec![item.clone()]);
        publish(&s, vec![item]);
        assert_eq!(s.version.load(Ordering::Relaxed), 1);
    }

    static ACTIVATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    struct FakeItem;

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl FakeItem {
        #[zbus(property)]
        fn status(&self) -> String {
            "Active".into()
        }
        #[zbus(property)]
        fn title(&self) -> String {
            "Fake App".into()
        }
        #[zbus(property)]
        fn icon_name(&self) -> String {
            String::new()
        }
        #[zbus(property)]
        fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
            // 2x2, opaque red (A,R,G,B)
            vec![(2, 2, [255u8, 255, 0, 0].repeat(4))]
        }
        fn activate(&self, _x: i32, _y: i32) {
            ACTIVATED.store(true, Ordering::SeqCst);
        }
    }

    /// End to end against the real session bus: a fake app registers with the
    /// tray, the tray shows its icon, and a click reaches the app as
    /// `Activate`. Needs a session bus; claims the StatusNotifierWatcher name
    /// if nothing else has it, so it is not part of the default run.
    #[test]
    #[ignore = "needs a session bus"]
    fn live_tray_end_to_end() {
        let mut tray = SystrayWidget::new();
        let _item_conn = futures_lite::future::block_on(async {
            let conn = zbus::connection::Builder::session()
                .name("org.veil.TestTrayItem")
                .serve_at(DEFAULT_ITEM_PATH, FakeItem)
                .build()
                .await
                .expect("fake item on the bus");
            // The tray's worker may still be starting: retry registering.
            let start = Instant::now();
            loop {
                let registered = async {
                    let p = zbus::Proxy::new(&conn, WATCHER_NAME, WATCHER_PATH, WATCHER_NAME).await.ok()?;
                    p.call_method("RegisterStatusNotifierItem", &("org.veil.TestTrayItem",)).await.ok()
                }
                .await;
                if registered.is_some() {
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(8), "no StatusNotifierWatcher appeared");
                std::thread::sleep(Duration::from_millis(200));
            }
            conn
        });

        // The icon shows up.
        let start = Instant::now();
        loop {
            tray.update(Instant::now());
            if tray.items.iter().any(|(it, pm)| it.title == "Fake App" && pm.is_some()) {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "tray never showed the fake item: {:?}", tray.items.len());
            std::thread::sleep(Duration::from_millis(200));
        }

        // Draw once so the tray knows where its icons are, then click one.
        let mut pm = Pixmap::new(200, 36).unwrap();
        let mut text = TextSystem::bitmap_only();
        let theme = Theme::default();
        let w = tray.draw(&mut pm, &mut text, &theme, &Metrics::default(), 0.0, 0.0, 36.0);
        assert!(w > 0.0);
        let x = tray.slots[0].x0 + 2.0;
        assert_eq!(tray.on_click(x), None);
        let start = Instant::now();
        while !ACTIVATED.load(Ordering::SeqCst) {
            assert!(start.elapsed() < Duration::from_secs(5), "the click never reached the app");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
