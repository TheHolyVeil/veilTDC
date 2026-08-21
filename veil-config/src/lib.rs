use mlua::Lua;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum Quality {
    Auto,
    Pixel,      // color halfblock (▀, 24-bit truecolor) — best for GUI apps
    AsciiLuma,  // greyscale luma chars — works everywhere
    AsciiEdge,  // luma + edge detection overlay
    Sixel,      // sixel inline images (not yet implemented)
    Kitty,      // kitty graphics protocol (not yet implemented)
}

impl Quality {
    pub fn parse_str(s: &str) -> Self {
        match s {
            "auto"       => Self::Auto,
            "pixel"      => Self::Pixel,
            "ascii"      => Self::AsciiLuma,
            "ascii_luma" => Self::AsciiLuma,
            "ascii_edge" => Self::AsciiEdge,
            "sixel"      => Self::Sixel,
            "kitty"      => Self::Kitty,
            _            => Self::Auto,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto      => "auto",
            Self::Pixel     => "pixel",
            Self::AsciiLuma => "ascii_luma",
            Self::AsciiEdge => "ascii_edge",
            Self::Sixel     => "sixel",
            Self::Kitty     => "kitty",
        }
    }
}

/// Detect the best supported render mode from terminal environment variables.
pub fn detect_quality() -> Quality {
    let term      = std::env::var("TERM").unwrap_or_default();
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();

    // Kitty graphics protocol
    if term == "xterm-kitty" {
        return Quality::Kitty;
    }
    if let Ok(prog) = std::env::var("TERM_PROGRAM") {
        if prog == "WezTerm" || prog.to_lowercase().contains("wezterm") {
            return Quality::Kitty;
        }
    }

    // Sixel
    if term.contains("sixel") || term == "mlterm" {
        return Quality::Sixel;
    }

    // Truecolor — halfblock pixel rendering works great
    if colorterm == "truecolor" || colorterm == "24bit" {
        return Quality::Pixel;
    }

    Quality::AsciiLuma
}

/// Output backend preference (`output` in Lua). `Auto` keeps the existing
/// env-based heuristic (WAYLAND_DISPLAY/DISPLAY/SSH_* → terminal, else try
/// DRM/KMS). `Drm` skips the heuristic entirely and always goes straight for
/// DRM/KMS on `run` — for a bare-TTY daily driver with real GPU hardware,
/// where the heuristic's SSH/env checks can misfire (e.g. a stale
/// WAYLAND_DISPLAY left in the shell from a prior graphical session).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputPref {
    Auto,
    Drm,
    Terminal,
}

impl OutputPref {
    fn from_str(s: &str) -> Self {
        match s {
            "drm" | "kms" => Self::Drm,
            "terminal" | "term" => Self::Terminal,
            _ => Self::Auto,
        }
    }
}

/// The modifier held down with a tiling keybind (`keybinds.mod_key` in Lua).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModKey {
    Super,
    Ctrl,
    Alt,
    Shift,
}

impl ModKey {
    fn from_str(s: &str) -> Self {
        match s {
            "ctrl"  => Self::Ctrl,
            "alt"   => Self::Alt,
            "shift" => Self::Shift,
            _       => Self::Super,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Super => "Super",
            Self::Ctrl  => "Ctrl",
            Self::Alt   => "Alt",
            Self::Shift => "Shift",
        }
    }
}

/// A tiling-layout operation or custom app launch a keybind can trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    Swap,
    Rotate,
    Close,
    ResizeGrow,
    ResizeShrink,
    Launch(String),
}

impl Action {
    pub fn label(&self) -> String {
        match self {
            Self::FocusLeft    => "focus left".to_string(),
            Self::FocusRight   => "focus right".to_string(),
            Self::FocusUp      => "focus up".to_string(),
            Self::FocusDown    => "focus down".to_string(),
            Self::Swap         => "swap with next".to_string(),
            Self::Rotate       => "rotate split".to_string(),
            Self::Close        => "close window".to_string(),
            Self::ResizeGrow   => "resize grow".to_string(),
            Self::ResizeShrink => "resize shrink".to_string(),
            Self::Launch(cmd)  => format!("launch: {cmd}"),
        }
    }
}

/// Parsed `keybinds` table. `binds` preserves declaration order — the Super+/
/// help overlay lists them verbatim, so config order is display order.
#[derive(Debug, Clone)]
pub struct Keybinds {
    pub mod_key: ModKey,
    pub binds:   Vec<(char, Action)>,
}

impl Keybinds {
    pub fn action_for(&self, key: char) -> Option<Action> {
        self.binds.iter().find(|(k, _)| *k == key).map(|(_, a)| a.clone())
    }
}

impl Default for Keybinds {
    fn default() -> Self {
        Self {
            mod_key: ModKey::Super,
            binds: vec![
                ('h', Action::FocusLeft),
                ('l', Action::FocusRight),
                ('k', Action::FocusUp),
                ('j', Action::FocusDown),
                ('s', Action::Swap),
                ('r', Action::Rotate),
                ('q', Action::Close),
                ('=', Action::ResizeGrow),
                ('-', Action::ResizeShrink),
            ],
        }
    }
}

fn parse_keybinds(gl: &mlua::Table) -> Keybinds {
    let default = Keybinds::default();
    let Ok(t) = gl.get::<mlua::Table>("keybinds") else {
        return default;
    };

    let mod_key = t.get::<String>("mod_key")
        .map(|s| ModKey::from_str(&s))
        .unwrap_or(default.mod_key);

    // (Lua field name, Action) — order here is the help-menu display order.
    const FIELDS: [(&str, Action); 9] = [
        ("focus_left",    Action::FocusLeft),
        ("focus_right",   Action::FocusRight),
        ("focus_up",      Action::FocusUp),
        ("focus_down",    Action::FocusDown),
        ("swap",          Action::Swap),
        ("rotate",        Action::Rotate),
        ("close",         Action::Close),
        ("resize_grow",   Action::ResizeGrow),
        ("resize_shrink", Action::ResizeShrink),
    ];

    let mut binds: Vec<(char, Action)> = FIELDS.iter().map(|(field, action)| {
        let key = t.get::<String>(*field).ok()
            .and_then(|s| s.chars().next())
            .map(|c| c.to_ascii_lowercase())
            .unwrap_or_else(|| {
                default.binds.iter().find(|(_, a)| a == action).unwrap().0
            });
        (key, action.clone())
    }).collect();

    // App keybinds: keybinds.apps = { b = "helium", t = "kitty" } or global apps table
    let app_table = t.get::<mlua::Table>("apps")
        .or_else(|_| gl.get::<mlua::Table>("apps"));

    if let Ok(apps) = app_table {
        for pair in apps.pairs::<String, String>().flatten() {
            let (key_str, cmd) = pair;
            if let Some(ch) = key_str.chars().next().map(|c| c.to_ascii_lowercase()) {
                if !cmd.trim().is_empty() {
                    if let Some(existing) = binds.iter_mut().find(|(k, _)| *k == ch) {
                        existing.1 = Action::Launch(cmd);
                    } else {
                        binds.push((ch, Action::Launch(cmd)));
                    }
                }
            }
        }
    }

    Keybinds { mod_key, binds }
}

#[derive(Debug, Clone)]
pub struct VeilConfig {
    pub quality:           Quality,
    pub fps:               u32,
    pub cage_timeout_secs: u32,
    pub input:             bool,
    /// Whether to use GPU compute shaders for frame encoding (halfblock/luma).
    /// Defaults to true. Set `gpu_render = false` in config.lua to disable.
    pub gpu_render:        bool,
    pub keybinds:          Keybinds,
    pub output:            OutputPref,
    /// Bare background color (RGB), shown wherever no window covers —
    /// otherwise it's plain black, an actual void with zero windows open
    /// (which now happens on purpose, since Alt+D lets you get there).
    /// `background = "#RRGGBB"` in Lua. Defaults to a cement grey.
    pub background:        [u8; 3],
}

impl Default for VeilConfig {
    fn default() -> Self {
        Self {
            quality:           Quality::Auto,
            fps:               60,
            cage_timeout_secs: 8,
            input:             true,
            gpu_render:        true,
            keybinds:          Keybinds::default(),
            output:            OutputPref::Auto,
            background:        [0x8c, 0x8c, 0x8c],
        }
    }
}

/// Parse `"#RRGGBB"` or `"RRGGBB"` into RGB bytes. `None` on anything else —
/// callers fall back to the default rather than erroring the whole config.
pub fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some([r, g, b])
}

impl VeilConfig {
    /// Resolve `Quality::Auto` to a concrete mode based on terminal capabilities.
    pub fn resolved_quality(&self) -> Quality {
        if self.quality == Quality::Auto {
            detect_quality()
        } else {
            self.quality.clone()
        }
    }
}

/// Load config from `path`. Falls back to defaults on any error or missing file.
pub fn load(path: &Path) -> VeilConfig {
    if !path.exists() {
        return VeilConfig::default();
    }

    let src = match std::fs::read_to_string(path) {
        Ok(s)  => s,
        Err(_) => return VeilConfig::default(),
    };

    let lua = Lua::new();
    if lua.load(&src).exec().is_err() {
        eprintln!("[config] failed to parse {:?}, using defaults", path);
        return VeilConfig::default();
    }

    let d  = VeilConfig::default();
    let gl = lua.globals();

    VeilConfig {
        quality: gl.get::<String>("quality")
            .map(|s| Quality::parse_str(&s))
            .unwrap_or(d.quality),
        fps: gl.get::<u32>("fps")
            .unwrap_or(d.fps),
        cage_timeout_secs: gl.get::<u32>("cage_timeout_secs")
            .unwrap_or(d.cage_timeout_secs),
        input: gl.get::<bool>("input")
            .unwrap_or(d.input),
        gpu_render: gl.get::<bool>("gpu_render")
            .unwrap_or(d.gpu_render),
        keybinds: parse_keybinds(&gl),
        output: gl.get::<String>("output")
            .map(|s| OutputPref::from_str(&s))
            .unwrap_or(d.output),
        background: gl.get::<String>("background")
            .ok()
            .and_then(|s| parse_hex_color(&s))
            .unwrap_or(d.background),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_color_with_hash() {
        assert_eq!(parse_hex_color("#4b5d45"), Some([0x4b, 0x5d, 0x45]));
    }

    #[test]
    fn hex_color_without_hash() {
        assert_eq!(parse_hex_color("4B5D45"), Some([0x4b, 0x5d, 0x45]));
    }

    #[test]
    fn hex_color_rejects_garbage() {
        assert_eq!(parse_hex_color("not a color"), None);
        assert_eq!(parse_hex_color("#fff"), None); // no 3-digit shorthand
        assert_eq!(parse_hex_color(""), None);
    }
}
