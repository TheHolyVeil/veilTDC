//! App icons: find the file for an `Icon=` name the way the freedesktop Icon
//! Theme Specification says (the current theme, then what it inherits, then
//! `hicolor`, then `pixmaps`), and draw it at the size a launcher row needs.
//!
//! Lookups happen lazily, one icon at a time as rows are drawn, and are cached
//! including misses, so a keystroke never touches the disk for an icon it has
//! already seen. The theme comes from the GTK settings (`gtk-icon-theme-name`),
//! which is where a desktop with no settings daemon keeps it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

/// Icons larger than this are not read (a damaged or hostile file).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Cached lookups kept before the cache is emptied.
const MAX_CACHED: usize = 512;
const MAX_INHERIT_DEPTH: usize = 6;
/// Sizes tried when a theme has no usable `index.theme`.
const GUESSED_SIZES: [u32; 9] = [48, 64, 32, 128, 256, 24, 22, 16, 512];

// ------------------------------------------------------------ theme metadata

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Fixed,
    Scalable,
    Threshold,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThemeDir {
    path: String,
    size: u32,
    scale: u32,
    min: u32,
    max: u32,
    threshold: u32,
    kind: Kind,
    applications: bool,
}

impl ThemeDir {
    /// How far this directory's sizes are from `want` pixels, per the spec's
    /// `DirectorySizeDistance`. Zero means the directory serves it exactly.
    fn distance(&self, want: u32) -> u32 {
        let (size, min, max, th) = (self.size * self.scale, self.min * self.scale, self.max * self.scale, self.threshold * self.scale);
        match self.kind {
            Kind::Fixed => size.abs_diff(want),
            Kind::Scalable => {
                if want < min {
                    min - want
                } else {
                    want.saturating_sub(max)
                }
            }
            Kind::Threshold => {
                let (lo, hi) = (size.saturating_sub(th), size + th);
                if want < lo {
                    lo - want
                } else {
                    want.saturating_sub(hi)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Theme {
    inherits: Vec<String>,
    dirs: Vec<ThemeDir>,
}

fn parse_theme(text: &str) -> Theme {
    let mut sections: Vec<(String, HashMap<String, String>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((name.to_string(), HashMap::new()));
        } else if let (Some((k, v)), Some((_, map))) = (line.split_once('='), sections.last_mut()) {
            map.entry(k.trim().to_string()).or_insert_with(|| v.trim().to_string());
        }
    }

    let csv = |s: &str| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect::<Vec<_>>();
    let main = sections.iter().find(|(n, _)| n == "Icon Theme").map(|(_, m)| m);
    let mut theme = Theme { inherits: main.and_then(|m| m.get("Inherits")).map(|v| csv(v)).unwrap_or_default(), dirs: Vec::new() };
    let Some(main) = main else { return theme };

    let mut names = main.get("Directories").map(|v| csv(v)).unwrap_or_default();
    names.extend(main.get("ScaledDirectories").map(|v| csv(v)).unwrap_or_default());
    for name in names {
        let Some((_, m)) = sections.iter().find(|(n, _)| *n == name) else { continue };
        let num = |k: &str| m.get(k).and_then(|v| v.parse::<u32>().ok());
        let Some(size) = num("Size").filter(|s| *s > 0) else { continue };
        let kind = match m.get("Type").map(String::as_str) {
            Some("Fixed") => Kind::Fixed,
            Some("Scalable") => Kind::Scalable,
            _ => Kind::Threshold,
        };
        theme.dirs.push(ThemeDir {
            applications: m.get("Context").is_some_and(|c| c == "Applications"),
            scale: num("Scale").unwrap_or(1).max(1),
            min: num("MinSize").unwrap_or(size),
            max: num("MaxSize").unwrap_or(size),
            threshold: num("Threshold").unwrap_or(2),
            path: name,
            size,
            kind,
        });
    }
    theme
}

// --------------------------------------------------------------- environment

/// Where icons live. Built from the environment, or by hand in tests.
#[derive(Debug, Clone, Default)]
pub struct IconEnv {
    /// The current theme's name.
    pub theme: String,
    /// `icons` directories, highest precedence first.
    pub dirs: Vec<PathBuf>,
    /// Flat directories of loose icons (`/usr/share/pixmaps`).
    pub pixmaps: Vec<PathBuf>,
}

impl IconEnv {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let config = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
        let data_home = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
        let data_dirs = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());

        let mut dirs = vec![data_home.join("icons"), home.join(".icons")];
        let mut pixmaps = Vec::new();
        for d in data_dirs.split(':').filter(|d| !d.is_empty()) {
            dirs.push(Path::new(d).join("icons"));
            pixmaps.push(Path::new(d).join("pixmaps"));
        }
        let mut seen = HashSet::new();
        dirs.retain(|d| seen.insert(d.clone()));
        pixmaps.retain(|d| seen.insert(d.clone()));

        IconEnv { theme: gtk_icon_theme(&config).unwrap_or_else(|| "hicolor".into()), dirs, pixmaps }
    }
}

/// `gtk-icon-theme-name` from the GTK 4 then GTK 3 settings.
fn gtk_icon_theme(config_home: &Path) -> Option<String> {
    ["gtk-4.0", "gtk-3.0"].iter().find_map(|v| {
        let text = std::fs::read_to_string(config_home.join(v).join("settings.ini")).ok()?;
        text.lines().find_map(|l| {
            let (k, val) = l.split_once('=')?;
            (k.trim() == "gtk-icon-theme-name").then(|| val.trim().trim_matches(['"', '\'']).to_string()).filter(|v| !v.is_empty())
        })
    })
}

// ------------------------------------------------------------------- lookup

/// Finds and caches icons for a row. Cheap to create; does nothing until asked.
pub struct IconCache {
    env: Option<IconEnv>,
    themes: HashMap<String, Rc<Theme>>,
    cache: HashMap<(String, u32), Option<Rc<Pixmap>>>,
}

impl IconCache {
    pub fn new(env: IconEnv) -> Self {
        IconCache { env: Some(env), themes: HashMap::new(), cache: HashMap::new() }
    }

    /// A cache that never finds anything (tests, and `--no-icons`).
    pub fn disabled() -> Self {
        IconCache { env: None, themes: HashMap::new(), cache: HashMap::new() }
    }

    /// The icon named `icon` as a `px` by `px` pixmap, or `None` when there is
    /// no such icon. Misses are remembered too.
    pub fn get(&mut self, icon: &str, px: u32) -> Option<Rc<Pixmap>> {
        if self.env.is_none() || icon.is_empty() || px == 0 {
            return None;
        }
        let key = (icon.to_string(), px);
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        if self.cache.len() >= MAX_CACHED {
            self.cache.clear();
        }
        let found = self.find(icon, px).and_then(|p| load(&p, px)).map(Rc::new);
        self.cache.insert(key, found.clone());
        found
    }

    /// The file for `icon` at about `px` pixels.
    pub fn find(&mut self, icon: &str, px: u32) -> Option<PathBuf> {
        let env = self.env.clone()?;
        if icon.starts_with('/') {
            return Path::new(icon).is_file().then(|| PathBuf::from(icon));
        }
        let name = clean_name(icon)?;

        for theme in self.chain(&env) {
            let t = self.theme(&env, &theme);
            // Application directories first, then nearest size, then scalable
            // over fixed at equal distance. (The spec ignores context, but an
            // `Icon=` is an application's icon: `doc` from `mimetypes` is not it.)
            let mut dirs: Vec<DirKey> = t
                .dirs
                .iter()
                .enumerate()
                .map(|(i, d)| (!d.applications, d.distance(px), d.kind != Kind::Scalable, i, d.path.clone()))
                .collect();
            if t.dirs.is_empty() {
                dirs = guessed_dirs(px);
            }
            dirs.sort();
            for (.., sub) in &dirs {
                for base in &env.dirs {
                    for ext in ["svg", "png"] {
                        let p = base.join(&theme).join(sub).join(format!("{name}.{ext}"));
                        if p.is_file() {
                            return Some(p);
                        }
                    }
                }
            }
        }
        for dir in &env.pixmaps {
            for ext in ["svg", "png"] {
                let p = dir.join(format!("{name}.{ext}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        None
    }

    /// The current theme, what it inherits (depth-first, no repeats), and
    /// `hicolor` last, which every theme falls back to.
    fn chain(&mut self, env: &IconEnv) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![(env.theme.clone(), 0usize)];
        while let Some((name, depth)) = stack.pop() {
            if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") || out.contains(&name) || depth > MAX_INHERIT_DEPTH {
                continue;
            }
            let parents = self.theme(env, &name).inherits.clone();
            out.push(name);
            for p in parents.into_iter().rev() {
                stack.push((p, depth + 1));
            }
        }
        out.retain(|t| t != "hicolor");
        out.push("hicolor".into());
        out
    }

    fn theme(&mut self, env: &IconEnv, name: &str) -> Rc<Theme> {
        if let Some(t) = self.themes.get(name) {
            return t.clone();
        }
        let parsed = env
            .dirs
            .iter()
            .find_map(|d| std::fs::read_to_string(d.join(name).join("index.theme")).ok())
            .map(|text| parse_theme(&text))
            .unwrap_or_default();
        let rc = Rc::new(parsed);
        self.themes.insert(name.to_string(), rc.clone());
        rc
    }
}

/// Sort key for a theme directory: (not an application dir, size distance,
/// not scalable, declared order, path).
type DirKey = (bool, u32, bool, usize, String);

/// An icon name is a bare file stem. Anything that could leave the theme
/// directory is refused, and a trailing image extension is dropped (some
/// `.desktop` files write `Icon=foo.png`).
fn clean_name(icon: &str) -> Option<String> {
    if icon.contains(['/', '\\', '\0']) || icon.contains("..") {
        return None;
    }
    let name = ["png", "svg", "xpm"].iter().find_map(|e| icon.strip_suffix(&format!(".{e}"))).unwrap_or(icon);
    (!name.is_empty()).then(|| name.to_string())
}

/// When a theme gives no directory list, try the layouts themes actually use.
fn guessed_dirs(px: u32) -> Vec<DirKey> {
    let mut subs = vec!["scalable/apps".to_string(), "apps/scalable".to_string()];
    for n in std::iter::once(px).chain(GUESSED_SIZES) {
        subs.push(format!("{n}x{n}/apps"));
        subs.push(format!("apps/{n}"));
    }
    subs.dedup();
    subs.into_iter().enumerate().map(|(i, s)| (false, 0, !s.contains("scalable"), i, s)).collect()
}

// ------------------------------------------------------------------- loading

/// Read an icon file and draw it into a `px` square, keeping its proportions.
pub fn load(path: &Path, px: u32) -> Option<Pixmap> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    let mut out = Pixmap::new(px, px)?;
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("svg") => draw_svg(&data, &mut out)?,
        Some("png") => draw_png(&data, &mut out)?,
        _ => return None,
    }
    Some(out)
}

fn fit(src_w: f32, src_h: f32, px: u32) -> Option<Transform> {
    if !(src_w > 0.0 && src_h > 0.0) || !src_w.is_finite() || !src_h.is_finite() {
        return None;
    }
    let px = px as f32;
    let s = (px / src_w).min(px / src_h);
    Some(Transform::from_scale(s, s).post_translate((px - src_w * s) / 2.0, (px - src_h * s) / 2.0))
}

fn draw_svg(data: &[u8], out: &mut Pixmap) -> Option<()> {
    let tree = resvg::usvg::Tree::from_data(data, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let ts = fit(size.width(), size.height(), out.width())?;
    resvg::render(&tree, ts, &mut out.as_mut());
    Some(())
}

fn draw_png(data: &[u8], out: &mut Pixmap) -> Option<()> {
    let src = Pixmap::decode_png(data).ok()?;
    let ts = fit(src.width() as f32, src.height() as f32, out.width())?;
    let paint = PixmapPaint { quality: FilterQuality::Bicubic, ..PixmapPaint::default() };
    out.draw_pixmap(0, 0, src.as_ref(), &paint, ts, None);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = "[Icon Theme]\nName=Test\nInherits=Parent,hicolor\nDirectories=16x16/apps,48x48/apps,scalable/apps,48x48/mimetypes\n\
[16x16/apps]\nSize=16\nContext=Applications\nType=Fixed\n\
[48x48/apps]\nSize=48\nContext=Applications\nType=Fixed\n\
[scalable/apps]\nSize=64\nMinSize=24\nMaxSize=512\nContext=Applications\nType=Scalable\n\
[48x48/mimetypes]\nSize=48\nContext=MimeTypes\nType=Fixed\n";

    fn svg(color: &str) -> String {
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><rect width="64" height="64" fill="{color}"/></svg>"#)
    }

    fn png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut pm = Pixmap::new(w, h).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(rgb[0], rgb[1], rgb[2], 255));
        pm.encode_png().unwrap()
    }

    fn put(root: &Path, rel: &str, bytes: impl AsRef<[u8]>) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn env(root: &Path) -> IconEnv {
        IconEnv { theme: "Test".into(), dirs: vec![root.join("icons")], pixmaps: vec![root.join("pixmaps")] }
    }

    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let p = pm.pixel(x, y).unwrap();
        [p.red(), p.green(), p.blue(), p.alpha()]
    }

    #[test]
    fn index_theme_is_parsed() {
        let t = parse_theme(INDEX);
        assert_eq!(t.inherits, ["Parent", "hicolor"]);
        assert_eq!(t.dirs.len(), 4);
        assert_eq!(t.dirs[0], ThemeDir { path: "16x16/apps".into(), size: 16, scale: 1, min: 16, max: 16, threshold: 2, kind: Kind::Fixed, applications: true });
        assert_eq!((t.dirs[2].min, t.dirs[2].max, t.dirs[2].kind.clone()), (24, 512, Kind::Scalable));
        assert!(!t.dirs[3].applications);
        // missing pieces and nonsense do not panic
        assert_eq!(parse_theme(""), Theme::default());
        assert_eq!(parse_theme("[Icon Theme]\nDirectories=a,b\n[a]\nSize=zero\n").dirs, vec![]);
        assert!(parse_theme("garbage\n=\n[x").dirs.is_empty());
    }

    #[test]
    fn size_distance_follows_the_spec() {
        let d = |kind, size, min, max, th| ThemeDir { path: String::new(), size, scale: 1, min, max, threshold: th, kind, applications: true };
        assert_eq!(d(Kind::Fixed, 48, 48, 48, 2).distance(48), 0);
        assert_eq!(d(Kind::Fixed, 48, 48, 48, 2).distance(32), 16);
        assert_eq!(d(Kind::Scalable, 64, 24, 512, 2).distance(32), 0);
        assert_eq!(d(Kind::Scalable, 64, 24, 512, 2).distance(16), 8);
        assert_eq!(d(Kind::Scalable, 64, 24, 512, 2).distance(600), 88);
        assert_eq!(d(Kind::Threshold, 32, 32, 32, 2).distance(33), 0);
        assert_eq!(d(Kind::Threshold, 32, 32, 32, 2).distance(40), 6);
        assert_eq!(d(Kind::Threshold, 32, 32, 32, 2).distance(20), 10);
        // a 2x directory counts double
        let hi = ThemeDir { scale: 2, ..d(Kind::Fixed, 24, 24, 24, 2) };
        assert_eq!(hi.distance(48), 0);
    }

    #[test]
    fn the_closest_size_wins_and_svg_beats_png_at_equal_distance() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "icons/Test/index.theme", INDEX);
        put(tmp.path(), "icons/Test/16x16/apps/app.png", png(16, 16, [1, 2, 3]));
        put(tmp.path(), "icons/Test/48x48/apps/app.png", png(48, 48, [4, 5, 6]));
        let mut c = IconCache::new(env(tmp.path()));
        assert!(c.find("app", 16).unwrap().ends_with("16x16/apps/app.png"));
        assert!(c.find("app", 48).unwrap().ends_with("48x48/apps/app.png"));
        assert!(c.find("app", 40).unwrap().ends_with("48x48/apps/app.png"));
        // a scalable svg serves 32 exactly (distance 0) and beats both pngs there
        put(tmp.path(), "icons/Test/scalable/apps/app.svg", svg("#00f"));
        let mut c = IconCache::new(env(tmp.path()));
        assert!(c.find("app", 32).unwrap().ends_with("scalable/apps/app.svg"));
        assert!(c.find("app", 48).unwrap().ends_with("scalable/apps/app.svg"), "svg ties with the 48 png and wins");
    }

    #[test]
    fn application_icons_beat_other_contexts() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "icons/Test/index.theme", INDEX);
        put(tmp.path(), "icons/Test/48x48/mimetypes/doc.png", png(48, 48, [9, 9, 9]));
        put(tmp.path(), "icons/Test/16x16/apps/doc.png", png(16, 16, [1, 1, 1]));
        let mut c = IconCache::new(env(tmp.path()));
        // the mimetype icon is the nearer size, but an app icon of the name exists
        assert!(c.find("doc", 48).unwrap().ends_with("16x16/apps/doc.png"));
    }

    #[test]
    fn it_falls_back_through_inherited_themes_hicolor_and_pixmaps() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "icons/Test/index.theme", INDEX);
        put(tmp.path(), "icons/Parent/index.theme", "[Icon Theme]\nInherits=Grand\nDirectories=scalable/apps\n[scalable/apps]\nSize=64\nMinSize=8\nMaxSize=512\nType=Scalable\nContext=Applications\n");
        put(tmp.path(), "icons/Parent/scalable/apps/inherited.svg", svg("#0f0"));
        put(tmp.path(), "icons/Grand/index.theme", "[Icon Theme]\nDirectories=32x32/apps\n[32x32/apps]\nSize=32\nType=Fixed\nContext=Applications\n");
        put(tmp.path(), "icons/Grand/32x32/apps/deep.png", png(32, 32, [7, 7, 7]));
        put(tmp.path(), "icons/hicolor/index.theme", "[Icon Theme]\nDirectories=48x48/apps\n[48x48/apps]\nSize=48\nType=Threshold\nContext=Applications\n");
        put(tmp.path(), "icons/hicolor/48x48/apps/basic.png", png(48, 48, [8, 8, 8]));
        put(tmp.path(), "pixmaps/loose.png", png(32, 32, [5, 5, 5]));
        let mut c = IconCache::new(env(tmp.path()));
        assert!(c.find("inherited", 32).unwrap().ends_with("Parent/scalable/apps/inherited.svg"));
        assert!(c.find("deep", 32).unwrap().ends_with("Grand/32x32/apps/deep.png"));
        assert!(c.find("basic", 32).unwrap().ends_with("hicolor/48x48/apps/basic.png"));
        assert!(c.find("loose", 32).unwrap().ends_with("pixmaps/loose.png"));
        assert!(c.find("missing", 32).is_none());
    }

    #[test]
    fn a_theme_without_an_index_is_still_searched() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "icons/Test/scalable/apps/plain.svg", svg("#123"));
        put(tmp.path(), "icons/Test/32x32/apps/sized.png", png(32, 32, [1, 1, 1]));
        let mut c = IconCache::new(env(tmp.path()));
        assert!(c.find("plain", 32).is_some());
        assert!(c.find("sized", 32).is_some());
    }

    #[test]
    fn inheritance_loops_and_deep_chains_terminate() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "icons/Test/index.theme", "[Icon Theme]\nInherits=B\nDirectories=\n");
        put(tmp.path(), "icons/B/index.theme", "[Icon Theme]\nInherits=Test,C\nDirectories=\n");
        put(tmp.path(), "icons/C/index.theme", "[Icon Theme]\nInherits=Test,B,../../etc\nDirectories=\n");
        let mut c = IconCache::new(env(tmp.path()));
        let chain = c.chain(&env(tmp.path()));
        assert_eq!(chain, ["Test", "B", "C", "hicolor"]);
    }

    #[test]
    fn absolute_paths_extensions_and_hostile_names() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "icons/Test/index.theme", INDEX);
        put(tmp.path(), "icons/Test/48x48/apps/app.png", png(48, 48, [1, 2, 3]));
        put(tmp.path(), "outside/abs.svg", svg("#fff"));
        put(tmp.path(), "secret.png", png(8, 8, [1, 1, 1]));
        let mut c = IconCache::new(env(tmp.path()));
        let abs = tmp.path().join("outside/abs.svg");
        assert_eq!(c.find(abs.to_str().unwrap(), 32), Some(abs));
        assert!(c.find("/does/not/exist.png", 32).is_none());
        assert!(c.find("app.png", 48).is_some(), "an extension in the name is tolerated");
        for evil in ["../secret", "../../secret.png", "a/b", "..", "", "x\0y", "\\evil"] {
            assert!(c.find(evil, 48).is_none(), "{evil:?}");
        }
    }

    #[test]
    fn svg_and_png_render_into_the_requested_square() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "a.svg", svg("#ff0000"));
        let pm = load(&tmp.path().join("a.svg"), 32).unwrap();
        assert_eq!((pm.width(), pm.height()), (32, 32));
        assert_eq!(px(&pm, 16, 16), [255, 0, 0, 255]);

        put(tmp.path(), "b.png", png(16, 16, [0, 255, 0]));
        let pm = load(&tmp.path().join("b.png"), 40).unwrap();
        assert_eq!((pm.width(), pm.height()), (40, 40));
        let c = px(&pm, 20, 20);
        assert!(c[1] > 240 && c[0] < 15 && c[3] == 255, "{c:?}");
    }

    #[test]
    fn non_square_icons_are_centred_not_stretched() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "wide.svg", r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32"><rect width="64" height="32" fill="#00f"/></svg>"##);
        let pm = load(&tmp.path().join("wide.svg"), 32).unwrap();
        assert_eq!(px(&pm, 16, 16)[2], 255, "middle is drawn");
        assert_eq!(px(&pm, 16, 2)[3], 0, "letterbox above is transparent");
        assert_eq!(px(&pm, 16, 29)[3], 0, "and below");
    }

    #[test]
    fn broken_huge_and_unknown_files_are_none_not_panics() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "bad.svg", "<svg><this is not xml");
        put(tmp.path(), "bad.png", b"\x89PNG\r\n\x1a\nnot really");
        put(tmp.path(), "empty.svg", "");
        put(tmp.path(), "icon.xpm", "/* XPM */");
        put(tmp.path(), "zero.svg", r##"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0"/>"##);
        let big = vec![b'a'; MAX_FILE_BYTES as usize + 1];
        put(tmp.path(), "big.svg", &big);
        for f in ["bad.svg", "bad.png", "empty.svg", "icon.xpm", "zero.svg", "big.svg", "missing.svg"] {
            assert!(load(&tmp.path().join(f), 32).is_none(), "{f}");
        }
        assert!(load(tmp.path(), 32).is_none(), "a directory");
        assert!(load(&tmp.path().join("a.svg"), 0).is_none());
    }

    #[test]
    fn a_hostile_svg_cannot_read_other_files() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "secret.png", png(8, 8, [255, 0, 0]));
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="32" height="32"><image width="32" height="32" xlink:href="{}"/></svg>"#,
            tmp.path().join("secret.png").display()
        );
        put(tmp.path(), "evil.svg", svg);
        // Raster images inside SVGs are not enabled, so nothing is drawn from the file.
        if let Some(pm) = load(&tmp.path().join("evil.svg"), 32) {
            assert_eq!(px(&pm, 16, 16)[3], 0);
        }
    }

    #[test]
    fn the_cache_remembers_hits_and_misses_and_disabled_finds_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "icons/Test/index.theme", INDEX);
        put(tmp.path(), "icons/Test/scalable/apps/app.svg", svg("#f0f"));
        let mut c = IconCache::new(env(tmp.path()));
        let a = c.get("app", 32).unwrap();
        let b = c.get("app", 32).unwrap();
        assert!(Rc::ptr_eq(&a, &b), "second lookup is the cached pixmap");
        assert!(!Rc::ptr_eq(&a, &c.get("app", 48).unwrap()), "another size is another entry");
        assert!(c.get("nope", 32).is_none());
        // a miss is cached: adding the file later does not change the answer
        put(tmp.path(), "icons/Test/scalable/apps/nope.svg", svg("#fff"));
        assert!(c.get("nope", 32).is_none());
        assert!(IconCache::disabled().get("app", 32).is_none());
        assert!(c.get("", 32).is_none() && c.get("app", 0).is_none());
    }

    #[test]
    fn the_cache_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = IconCache::new(env(tmp.path()));
        for i in 0..(MAX_CACHED * 2 + 5) {
            c.get(&format!("missing-{i}"), 32);
        }
        assert!(c.cache.len() <= MAX_CACHED);
    }

    #[test]
    fn the_theme_name_comes_from_gtk_settings() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "gtk-3.0/settings.ini", "[Settings]\ngtk-theme-name=Adwaita\ngtk-icon-theme-name=Papirus-Dark\n");
        assert_eq!(gtk_icon_theme(tmp.path()).as_deref(), Some("Papirus-Dark"));
        put(tmp.path(), "gtk-4.0/settings.ini", "[Settings]\ngtk-icon-theme-name = \"Four\"\n");
        assert_eq!(gtk_icon_theme(tmp.path()).as_deref(), Some("Four"), "gtk 4 wins");
        put(tmp.path(), "gtk-4.0/settings.ini", "[Settings]\ngtk-icon-theme-name=\n");
        assert_eq!(gtk_icon_theme(tmp.path()).as_deref(), Some("Papirus-Dark"), "an empty value falls through");
        assert_eq!(gtk_icon_theme(&tmp.path().join("nope")), None);
    }
}
