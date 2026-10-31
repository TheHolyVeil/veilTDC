use mlua::Lua;
use std::path::{Path, PathBuf};

/// Which GPU the frame encoder asks for (`gpu_power` in config.lua).
/// `High` is the default: fastest adapter, never a surprise slowdown on
/// weak hardware. `Low` prefers the integrated GPU, so a hybrid-graphics
/// laptop doesn't wake the discrete one for two tiny compute shaders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GpuPower {
    #[default]
    High,
    Low,
}

impl GpuPower {
    pub fn parse_str(s: &str) -> Self {
        match s {
            "low" => Self::Low,
            _ => Self::High,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
        }
    }
}

/// Render quality ladder (`quality` in config.lua), best to last resort:
/// Kitty graphics, then halfblock in truecolor / 256 / 16 colours, then ASCII.
/// `Sixel` parses but is not implemented; it resolves like `Auto`.
#[derive(Debug, Clone, PartialEq)]
pub enum Quality {
    Auto,
    Kitty,     // kitty graphics protocol (real pixels)
    Pixel,     // halfblock ▀, 24-bit truecolor: the readable baseline
    Pixel256,  // halfblock, xterm 256-colour palette
    Pixel16,   // halfblock, 16-colour palette, ordered dither (Linux VT)
    AsciiLuma, // greyscale luma chars: works everywhere
    AsciiEdge, // luma + edge detection overlay
    Sixel,     // not implemented; falls back to auto-detection
}

impl Quality {
    pub fn parse_str(s: &str) -> Self {
        match s {
            "auto" => Self::Auto,
            "kitty" => Self::Kitty,
            "pixel" => Self::Pixel,
            "pixel256" => Self::Pixel256,
            "pixel16" => Self::Pixel16,
            "ascii" | "ascii_luma" => Self::AsciiLuma,
            "ascii_edge" => Self::AsciiEdge,
            "sixel" => Self::Sixel,
            _ => Self::Auto,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kitty => "kitty",
            Self::Pixel => "pixel",
            Self::Pixel256 => "pixel256",
            Self::Pixel16 => "pixel16",
            Self::AsciiLuma => "ascii_luma",
            Self::AsciiEdge => "ascii_edge",
            Self::Sixel => "sixel",
        }
    }
}

/// Detect the best supported render mode from the process environment.
/// The one resolver: `config.lua`'s `quality = "auto"` and the terminal
/// backend both go through it. Never returns `Auto` or `Sixel`.
pub fn detect_quality() -> Quality {
    detect_quality_with(|k| std::env::var(k).ok())
}

/// [`detect_quality`] over an arbitrary environment lookup (testable).
pub fn detect_quality_with(env: impl Fn(&str) -> Option<String>) -> Quality {
    let get = |k: &str| env(k).unwrap_or_default();
    let term = get("TERM");
    let colorterm = get("COLORTERM");
    let program = get("TERM_PROGRAM").to_lowercase();

    // Graphics protocol terminals. (Env sniffing only; no DA1/`a=q` probe yet.)
    if term == "xterm-kitty"
        || term == "xterm-ghostty"
        || program.contains("wezterm")
        || program == "ghostty"
        || env("KITTY_WINDOW_ID").is_some()
    {
        return Quality::Kitty;
    }

    // Halfblock needs UTF-8. Downgrade only when a locale IS set and is not
    // UTF-8; an unset locale (services, minimal sessions) is not evidence.
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .map(|k| get(k))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_lowercase();
    if !locale.is_empty() && !locale.contains("utf-8") && !locale.contains("utf8") {
        return Quality::AsciiLuma;
    }

    if colorterm == "truecolor" || colorterm == "24bit" {
        return Quality::Pixel;
    }
    if term.contains("256color") {
        return Quality::Pixel256;
    }
    // Linux console and plain xterm-family/multiplexer terminals: 16 colours.
    if term == "linux"
        || term == "xterm"
        || term == "screen"
        || term == "tmux"
        || term.starts_with("rxvt")
    {
        return Quality::Pixel16;
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

/// Which display mode the DRM backend drives (`refresh` in Lua). The
/// resolution is always the connector's preferred mode's; this only picks the
/// refresh rate among modes of that size. `Preferred` is the EDID's first
/// mode, which many high-refresh panels advertise at 60 Hz.
/// `refresh = "max"` | `"preferred"` | `144` (a number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPref {
    Preferred,
    Max,
    Hz(u32),
}

impl RefreshPref {
    fn parse_str(s: &str) -> Self {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "max" | "highest" => Self::Max,
            "" | "preferred" | "auto" => Self::Preferred,
            n => n.parse::<u32>().map_or(Self::Preferred, Self::Hz),
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
            "ctrl" => Self::Ctrl,
            "alt" => Self::Alt,
            "shift" => Self::Shift,
            _ => Self::Super,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Super => "Super",
            Self::Ctrl => "Ctrl",
            Self::Alt => "Alt",
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
    /// Switch between Dwindle and Scroll tiling modes.
    ToggleLayout,
    /// Fullscreen the focused window (compositor-triggered — same effect as
    /// a client's own fullscreen request, just initiated from a keybind
    /// instead of waiting for the app to ask).
    ToggleFullscreen,
    /// Hot-reload config file on demand.
    ReloadConfig,
    /// Open (or close) the wallpaper chooser overlay.
    WallpaperChooser,
    /// Switch the active workspace (1-indexed 1..=9, matches the Super+1..9
    /// keys visually — converted to a 0-indexed array slot in veil-host).
    SwitchWorkspace(u8),
    /// Move the focused window to a workspace and follow it there (matches
    /// niri's default convention — move-and-stay would leave you looking at
    /// an empty workspace). 1-indexed, same as `SwitchWorkspace`.
    MoveToWorkspace(u8),
    VolumeUp,
    VolumeDown,
    VolumeMute,
    BrightnessUp,
    BrightnessDown,
    Launch(String),
}

impl Action {
    pub fn label(&self) -> String {
        match self {
            Self::FocusLeft => "focus left".to_string(),
            Self::FocusRight => "focus right".to_string(),
            Self::FocusUp => "focus up".to_string(),
            Self::FocusDown => "focus down".to_string(),
            Self::Swap => "swap with next".to_string(),
            Self::Rotate => "rotate split".to_string(),
            Self::Close => "close window".to_string(),
            Self::ResizeGrow => "resize grow".to_string(),
            Self::ResizeShrink => "resize shrink".to_string(),
            Self::ToggleLayout => "toggle layout mode".to_string(),
            Self::ToggleFullscreen => "toggle fullscreen".to_string(),
            Self::ReloadConfig => "reload config".to_string(),
            Self::WallpaperChooser => "wallpaper chooser".to_string(),
            Self::SwitchWorkspace(n) => format!("switch to workspace {n}"),
            Self::MoveToWorkspace(n) => format!("move to workspace {n}"),
            Self::VolumeUp => "volume up".to_string(),
            Self::VolumeDown => "volume down".to_string(),
            Self::VolumeMute => "toggle mute".to_string(),
            Self::BrightnessUp => "brightness up".to_string(),
            Self::BrightnessDown => "brightness down".to_string(),
            Self::Launch(cmd) => format!("launch: {cmd}"),
        }
    }
}

/// Number of workspaces (Super+1..9). Fixed rather than configurable — a
/// variable count would mean variable-length digit keybinds too, which
/// complicates the OSD/help display for no real benefit; 9 covers what
/// digit keys can address on a standard keyboard anyway.
pub const WORKSPACE_COUNT: u8 = 9;

/// US/QWERTY shift+digit-row symbols, index 0 == workspace 1 — used for the
/// default move-window-to-workspace binds (Super+Shift+1..9). No special
/// input-handling code needed for the "Shift" part: xkb already produces
/// these symbol keysyms when Shift is held over the digit row, so this
/// rides the exact same char-lookup path as every other keybind. Layout-
/// dependent like the rest of this file's char-based binds already are
/// (h/l/j/k assume a QWERTY-ish physical layout too) — a non-US layout
/// that shifts the digit row differently will need a `keybinds.apps`
/// override for this to land on the keys you'd expect.
const SHIFTED_DIGIT_SYMBOLS: [char; 9] = ['!', '@', '#', '$', '%', '^', '&', '*', '('];

/// Parsed `keybinds` table. `binds` preserves declaration order — the Super+/
/// help overlay lists them verbatim, so config order is display order.
#[derive(Debug, Clone)]
pub struct Keybinds {
    pub mod_key: ModKey,
    pub binds: Vec<(char, Action)>,
}

impl Keybinds {
    pub fn action_for(&self, key: char) -> Option<Action> {
        self.binds
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, a)| a.clone())
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
                ('w', Action::ToggleLayout),
                ('f', Action::ToggleFullscreen),
                ('c', Action::ReloadConfig),
                ('y', Action::WallpaperChooser),
            ]
            // Super+1..9 workspace switch — appended rather than inlined
            // above so the numeric range stays visually obvious and doesn't
            // need updating if WORKSPACE_COUNT ever changes.
            .into_iter()
            .chain((1..=WORKSPACE_COUNT).map(|n| {
                (
                    char::from_digit(n as u32, 10).unwrap(),
                    Action::SwitchWorkspace(n),
                )
            }))
            // Super+Shift+1..9 — move focused window to that workspace and
            // follow it there.
            .chain((1..=WORKSPACE_COUNT).map(|n| {
                (
                    SHIFTED_DIGIT_SYMBOLS[(n - 1) as usize],
                    Action::MoveToWorkspace(n),
                )
            }))
            .collect(),
        }
    }
}

/// Config key name to the char the input path matches. `enter`/`return` map
/// to `'\r'` (what the keyboard reports for Return); anything else is the
/// first character, lowercased.
fn bind_key(s: &str) -> Option<char> {
    match s.trim().to_ascii_lowercase().as_str() {
        "enter" | "return" => Some('\r'),
        k => k.chars().next(),
    }
}

/// (Lua field name, Action): order here is the help-menu display order.
const KEYBIND_FIELDS: [(&str, Action); 18] = [
    ("focus_left", Action::FocusLeft),
    ("focus_right", Action::FocusRight),
    ("focus_up", Action::FocusUp),
    ("focus_down", Action::FocusDown),
    ("swap", Action::Swap),
    ("rotate", Action::Rotate),
    ("close", Action::Close),
    ("resize_grow", Action::ResizeGrow),
    ("resize_shrink", Action::ResizeShrink),
    ("toggle_layout", Action::ToggleLayout),
    ("fullscreen", Action::ToggleFullscreen),
    ("reload_config", Action::ReloadConfig),
    ("wallpaper_chooser", Action::WallpaperChooser),
    ("volume_up", Action::VolumeUp),
    ("volume_down", Action::VolumeDown),
    ("volume_mute", Action::VolumeMute),
    ("brightness_up", Action::BrightnessUp),
    ("brightness_down", Action::BrightnessDown),
];

fn parse_keybinds(gl: &mlua::Table) -> Keybinds {
    let default = Keybinds::default();
    // No `keybinds` table: fall back to the globals table itself, so a bare
    // top-level `apps = {..}` still works (it was silently ignored before).
    let t = gl
        .get::<mlua::Table>("keybinds")
        .unwrap_or_else(|_| gl.clone());

    let mod_key = t
        .get::<String>("mod_key")
        .map(|s| ModKey::from_str(&s))
        .unwrap_or(default.mod_key);

    let mut binds: Vec<(char, Action)> = KEYBIND_FIELDS
        .iter()
        .filter_map(|(field, action)| {
            let key = match t.get::<mlua::Value>(*field) {
                // `field = false` or `field = ""` unbinds the action.
                Ok(mlua::Value::Boolean(false)) => return None,
                Ok(mlua::Value::String(s)) => {
                    s.to_string_lossy().chars().next()?.to_ascii_lowercase()
                }
                // Absent (or not a string): the default key, if it has one.
                _ => default
                    .binds
                    .iter()
                    .find(|(_, a)| a == action)
                    .map(|(c, _)| *c)?,
            };
            Some((key, action.clone()))
        })
        .collect();

    // Super+1..9 workspace switch and Super+Shift+1..9 move-to-workspace —
    // not in FIELDS above (no natural single Lua field name for eighteen
    // keys), so seeded straight from the defaults rather than looped
    // per-field. Still user-overridable: the `apps` loop below runs after
    // this and will happily replace either if someone wants those keys for
    // something else instead.
    binds.extend(
        default
            .binds
            .iter()
            .filter(|(_, a)| matches!(a, Action::SwitchWorkspace(_) | Action::MoveToWorkspace(_)))
            .cloned(),
    );

    // App keybinds, from `keybinds.apps` or a top-level `apps`. Two forms:
    //   ordered:  apps = { { "enter", "kitty" }, { key = "b", exec = "helium-browser" } }
    //   map:      apps = { b = "helium-browser" }   (applied in key order)
    // Each Launch bind also becomes a bar tile, in this order, so the ordered
    // form is what gives a stable bar layout (Lua map iteration order isn't).
    let app_table = t
        .get::<mlua::Table>("apps")
        .or_else(|_| gl.get::<mlua::Table>("apps"));

    if let Ok(apps) = app_table {
        let mut entries: Vec<(String, String)> = Vec::new();
        for v in apps.sequence_values::<mlua::Table>().flatten() {
            let key = v.get::<String>("key").or_else(|_| v.get::<String>(1));
            let exec = v.get::<String>("exec").or_else(|_| v.get::<String>(2));
            if let (Ok(k), Ok(e)) = (key, exec) {
                entries.push((k, e));
            }
        }
        let mut map: Vec<(String, String)> = apps.pairs::<String, String>().flatten().collect();
        map.sort();
        entries.extend(map);

        for (key_str, cmd) in entries {
            if let Some(ch) = bind_key(&key_str) {
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

/// Which built-in color preset `theme = "..."` in config.lua selects.
/// `Default` is the original hardcoded amethyst/purple look — a config.lua
/// with no `theme` key resolves here, so existing configs render unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Default,
    Nord,
    Dracula,
    Catppuccin,
    Gruvbox,
    Everforest,
    TokyoNight,
    Solarized,
    RosePine,
    Monokai,
    OneDark,
    /// Abyss's personal pick — deep, desaturated sage green. Not derived
    /// from any published palette; tuned by hand.
    DeepSage,
    /// Colours derived from the wallpaper (see `veil-palette`). Until a
    /// wallpaper has loaded the host chrome shows the Deep Sage set.
    Wallpaper,
}

impl ThemeName {
    fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "nord" => Self::Nord,
            "dracula" => Self::Dracula,
            "catppuccin" | "mocha" => Self::Catppuccin,
            "gruvbox" => Self::Gruvbox,
            "everforest" => Self::Everforest,
            "tokyonight" | "tokyo_night" | "tokyo-night" => Self::TokyoNight,
            "solarized" => Self::Solarized,
            "rosepine" | "rose_pine" | "rose-pine" => Self::RosePine,
            "monokai" => Self::Monokai,
            "onedark" | "one_dark" | "one-dark" => Self::OneDark,
            "deepsage" | "deep_sage" | "deep-sage" | "sage" => Self::DeepSage,
            "wallpaper" => Self::Wallpaper,
            _ => Self::Default,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Nord => "nord",
            Self::Dracula => "dracula",
            Self::Catppuccin => "catppuccin",
            Self::Gruvbox => "gruvbox",
            Self::Everforest => "everforest",
            Self::TokyoNight => "tokyonight",
            Self::Solarized => "solarized",
            Self::RosePine => "rosepine",
            Self::Monokai => "monokai",
            Self::OneDark => "onedark",
            Self::DeepSage => "deepsage",
            Self::Wallpaper => "wallpaper",
        }
    }
}

/// Resolved color set for the launcher overlay, help overlay, and (sidebar/
/// bar, once built) chrome. Deliberately does NOT touch hosted-app window
/// contents — veil-host never recolors a client's own buffer.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Void-fill color shown wherever no window covers (RGB, no alpha).
    /// This is only the *preset's* default value — an explicit
    /// `background = "#RRGGBB"` in config.lua still overrides it, same
    /// precedence `background` always had, theme or not.
    pub background: [u8; 3],
    pub panel_bg: [u8; 4],
    pub border: [u8; 4],
    pub header: [u8; 4],
    pub text: [u8; 4],
    pub text_dim: [u8; 4],
    pub accent: [u8; 4],
    pub highlight: [u8; 4],
}

impl Theme {
    pub fn for_name(name: ThemeName) -> Theme {
        match name {
            ThemeName::Default => Theme {
                background: [0x8c, 0x8c, 0x8c],
                panel_bg: [10, 0, 16, 245],
                border: [199, 146, 234, 255],
                header: [255, 215, 0, 255],
                text: [199, 146, 234, 255],
                text_dim: [150, 150, 180, 255],
                accent: [128, 222, 234, 255],
                highlight: [65, 35, 85, 255],
            },
            ThemeName::Nord => Theme {
                background: [0x2e, 0x34, 0x40],
                panel_bg: [0x3b, 0x42, 0x52, 245],
                border: [0x88, 0xc0, 0xd0, 255],
                header: [0xeb, 0xcb, 0x8b, 255],
                text: [0xe5, 0xe9, 0xf0, 255],
                text_dim: [0x61, 0x6e, 0x88, 255],
                accent: [0x81, 0xa1, 0xc1, 255],
                highlight: [0x43, 0x4c, 0x5e, 255],
            },
            ThemeName::Dracula => Theme {
                background: [0x28, 0x2a, 0x36],
                panel_bg: [0x1e, 0x1f, 0x29, 245],
                border: [0xbd, 0x93, 0xf9, 255],
                header: [0xf1, 0xfa, 0x8c, 255],
                text: [0xf8, 0xf8, 0xf2, 255],
                text_dim: [0x62, 0x72, 0xa4, 255],
                accent: [0x8b, 0xe9, 0xfd, 255],
                highlight: [0x44, 0x47, 0x5a, 255],
            },
            ThemeName::Catppuccin => Theme {
                background: [0x1e, 0x1e, 0x2e],
                panel_bg: [0x18, 0x18, 0x25, 245],
                border: [0xcb, 0xa6, 0xf7, 255],
                header: [0xf9, 0xe2, 0xaf, 255],
                text: [0xcd, 0xd6, 0xf4, 255],
                text_dim: [0x6c, 0x70, 0x86, 255],
                accent: [0x89, 0xdc, 0xeb, 255],
                highlight: [0x31, 0x32, 0x44, 255],
            },
            ThemeName::Gruvbox => Theme {
                background: [0x28, 0x28, 0x28],
                panel_bg: [0x1d, 0x20, 0x21, 245],
                border: [0xd3, 0x86, 0x9b, 255],
                header: [0xfa, 0xbd, 0x2f, 255],
                text: [0xeb, 0xdb, 0xb2, 255],
                text_dim: [0x92, 0x83, 0x74, 255],
                accent: [0x83, 0xa5, 0x98, 255],
                highlight: [0x3c, 0x38, 0x36, 255],
            },
            ThemeName::Everforest => Theme {
                background: [0x2d, 0x35, 0x3b],
                panel_bg: [0x23, 0x2a, 0x2e, 245],
                border: [0xd6, 0x99, 0xb6, 255],
                header: [0xdb, 0xbc, 0x7f, 255],
                text: [0xd3, 0xc6, 0xaa, 255],
                text_dim: [0x7a, 0x82, 0x87, 255],
                accent: [0x7f, 0xbb, 0xb3, 255],
                highlight: [0x3d, 0x48, 0x4d, 255],
            },
            ThemeName::TokyoNight => Theme {
                background: [0x1a, 0x1b, 0x26],
                panel_bg: [0x16, 0x16, 0x1e, 245],
                border: [0xbb, 0x9a, 0xf7, 255],
                header: [0xe0, 0xaf, 0x68, 255],
                text: [0xc0, 0xca, 0xf5, 255],
                text_dim: [0x56, 0x5f, 0x89, 255],
                accent: [0x7d, 0xcf, 0xff, 255],
                highlight: [0x29, 0x2e, 0x42, 255],
            },
            ThemeName::Solarized => Theme {
                background: [0x00, 0x2b, 0x36],
                panel_bg: [0x07, 0x36, 0x42, 245],
                border: [0x6c, 0x71, 0xc4, 255],
                header: [0xb5, 0x89, 0x00, 255],
                text: [0x83, 0x94, 0x96, 255],
                text_dim: [0x58, 0x6e, 0x75, 255],
                accent: [0x2a, 0xa1, 0x98, 255],
                highlight: [0x0a, 0x45, 0x52, 255],
            },
            ThemeName::RosePine => Theme {
                background: [0x19, 0x17, 0x24],
                panel_bg: [0x1f, 0x1d, 0x2e, 245],
                border: [0xc4, 0xa7, 0xe7, 255],
                header: [0xf6, 0xc1, 0x77, 255],
                text: [0xe0, 0xde, 0xf4, 255],
                text_dim: [0x6e, 0x6a, 0x86, 255],
                accent: [0x9c, 0xcf, 0xd8, 255],
                highlight: [0x26, 0x23, 0x3a, 255],
            },
            ThemeName::Monokai => Theme {
                background: [0x27, 0x28, 0x22],
                panel_bg: [0x1e, 0x1f, 0x1c, 245],
                border: [0xae, 0x81, 0xff, 255],
                header: [0xe6, 0xdb, 0x74, 255],
                text: [0xf8, 0xf8, 0xf2, 255],
                text_dim: [0x75, 0x71, 0x5e, 255],
                accent: [0x66, 0xd9, 0xef, 255],
                highlight: [0x3e, 0x3d, 0x32, 255],
            },
            ThemeName::OneDark => Theme {
                background: [0x28, 0x2c, 0x34],
                panel_bg: [0x21, 0x25, 0x2b, 245],
                border: [0xc6, 0x78, 0xdd, 255],
                header: [0xe5, 0xc0, 0x7b, 255],
                text: [0xab, 0xb2, 0xbf, 255],
                text_dim: [0x5c, 0x63, 0x70, 255],
                accent: [0x56, 0xb6, 0xc2, 255],
                highlight: [0x2c, 0x31, 0x3a, 255],
            },
            // Deep Sage — Abyss's own. Desaturated dark green-charcoal base,
            // muted sage border, warm tan-gold header for contrast against
            // all that green, soft teal-sage accent. Not sourced from any
            // published palette — hand-tuned, so touch it before anyone
            // else's preferences do.
            ThemeName::DeepSage | ThemeName::Wallpaper => Theme {
                background: [0x23, 0x2b, 0x26],
                panel_bg: [0x1a, 0x20, 0x1c, 245],
                border: [0x87, 0xa0, 0x8d, 255],
                header: [0xc9, 0xb4, 0x58, 255],
                text: [0xdf, 0xe6, 0xe0, 255],
                text_dim: [0x6b, 0x7a, 0x70, 255],
                accent: [0x7f, 0xae, 0x9b, 255],
                highlight: [0x2f, 0x38, 0x30, 255],
            },
        }
    }
}

/// `theme_mode = "dark" | "light" | "auto"`: which way `theme = "wallpaper"`
/// leans. `auto` goes light only for a very bright wallpaper. Presets ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
    Auto,
}

impl ThemeMode {
    fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "light" => Self::Light,
            "auto" => Self::Auto,
            _ => Self::Dark,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Auto => "auto",
        }
    }
}

/// Where the bar sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarPosition {
    Top,
    Bottom,
}

impl BarPosition {
    fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "top" => Self::Top,
            _ => Self::Bottom,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
        }
    }
}

/// `bar = { ... }` in config.lua. Only what's configurable about the bar —
/// tile rendering (workspace widget, clock, app shortcuts) is a fixed
/// three-column layout in veil-host, not something exposed here. Notably
/// NOT configured here: which apps show up as shortcuts — those are
/// derived straight from `keybinds.apps`/top-level `apps` (every
/// `Action::Launch` bind becomes a tile) instead of a separate list, so an
/// app never needs declaring twice.
#[derive(Debug, Clone, PartialEq)]
pub struct BarConfig {
    pub enabled: bool,
    pub position: BarPosition,
    /// Run the supervised `veil shell bar` (layer-shell) instead of the
    /// built-in 12px bar. If it dies the built-in bar comes back by itself.
    pub shell: bool,
    /// Shell bar height in logical px at the default font size (it scales
    /// with `font.size`). Clamped to 24..=64.
    pub height: u32,
    /// Widget names for each section of the shell bar.
    pub left: Vec<String>,
    pub center: Vec<String>,
    pub right: Vec<String>,
    /// `strftime` format of the clock widget.
    pub clock_format: String,
    /// Mount point the disk widget reports.
    pub disk_mount: String,
    /// Coordinates for the weather widget. There is no auto-location.
    pub weather: Option<BarWeather>,
    /// `custom:<name>` widgets, in the order they were written.
    pub custom: Vec<BarCustom>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BarWeather {
    pub lat: f64,
    pub lon: f64,
    /// `"metric"` or `"imperial"`.
    pub units: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BarCustom {
    pub name: String,
    pub command: String,
    /// Seconds between runs.
    pub interval: u32,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            position: BarPosition::Top,
            shell: false,
            height: 36,
            left: vec!["workspaces".into(), "window_title".into()],
            center: vec!["clock".into()],
            right: vec!["cpu".into(), "memory".into(), "battery".into(), "audio".into()],
            clock_format: "%a %b %d  %H:%M".into(),
            disk_mount: "/".into(),
            weather: None,
            custom: Vec::new(),
        }
    }
}

/// How a wallpaper is fitted to the output (`fit` in Lua).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallpaperFit {
    /// Cover the output, cropping the overflow from the centre (default).
    Fill,
    /// Whole image visible, letterboxed on the background colour.
    Fit,
    /// Stretch to the output, ignoring aspect ratio.
    Stretch,
}

impl WallpaperFit {
    fn parse_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "fit" | "contain" => Self::Fit,
            "stretch" => Self::Stretch,
            _ => Self::Fill,
        }
    }
}

/// Static-image wallpaper (`wallpaper` in Lua). A path may be a file or a
/// directory (its images are used, non-recursive).
///
/// ```lua
/// wallpaper = "~/Pictures/a.png"
/// wallpaper = { engine = "static", paths = { "~/a.png", "~/walls" },
///               interval = 600,   -- seconds between images; 0 = pick once
///               shuffle = true, fit = "fill" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallpaperConfig {
    pub paths: Vec<PathBuf>,
    pub interval_secs: u64,
    pub shuffle: bool,
    pub fit: WallpaperFit,
}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

fn parse_wallpaper(gl: &mlua::Table) -> Option<WallpaperConfig> {
    let v = gl.get::<mlua::Value>("wallpaper").ok()?;
    let (paths, interval_secs, shuffle, fit) = match v {
        mlua::Value::String(s) => (
            vec![expand_home(&s.to_string_lossy())],
            0,
            false,
            WallpaperFit::Fill,
        ),
        mlua::Value::Table(t) => {
            if let Ok(engine) = t.get::<String>("engine") {
                if !engine.eq_ignore_ascii_case("static") {
                    eprintln!("[config] wallpaper.engine = {engine:?} is not supported (only \"static\")");
                    return None;
                }
            }
            // `paths = {..}`, a single `path = ".."`, or the bare sequence.
            let mut raw: Vec<String> = t.get::<Vec<String>>("paths").unwrap_or_default();
            if let Ok(p) = t.get::<String>("path") {
                raw.push(p);
            }
            raw.extend(t.sequence_values::<String>().filter_map(Result::ok));
            (
                raw.iter().map(|p| expand_home(p)).collect(),
                t.get::<u64>("interval").unwrap_or(0),
                get_bool(&t, "shuffle", false),
                t.get::<String>("fit")
                    .map_or(WallpaperFit::Fill, |f| WallpaperFit::parse_str(&f)),
            )
        }
        _ => return None,
    };
    (!paths.is_empty()).then_some(WallpaperConfig {
        paths,
        interval_secs,
        shuffle,
        fit,
    })
}

/// `font = { ... }`: the one place fonts are chosen. Every Veil surface (the
/// bar, the launcher, the help overlay, ...) reads this, and editing it takes
/// effect live. All fields are optional; unset ones fall back to a sensible
/// installed font. A family can be a name ("Inter") or an absolute path.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FontSettings {
    pub family: Option<String>,
    pub mono: Option<String>,
    pub icons: Option<String>,
    /// Tried in order for characters the main font does not have.
    pub fallback: Vec<String>,
    /// Base size in pixels. Surfaces scale their text from it. `None` = 14.
    pub size: Option<f32>,
    pub weight: FontWeight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontWeight {
    #[default]
    Regular,
    Bold,
}

impl FontWeight {
    fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "bold" => Self::Bold,
            _ => Self::Regular,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Bold => "bold",
        }
    }
}

/// `font = "Inter"` is shorthand for `font = { family = "Inter" }`.
fn parse_font(gl: &mlua::Table, default: &FontSettings) -> FontSettings {
    let non_empty = |s: String| Some(s).filter(|s| !s.trim().is_empty());
    match gl.get::<mlua::Value>("font") {
        Ok(mlua::Value::String(s)) => FontSettings {
            family: s.to_str().ok().and_then(|s| non_empty(s.to_string())),
            ..default.clone()
        },
        Ok(mlua::Value::Table(t)) => {
            let get = |key: &str| t.get::<String>(key).ok().and_then(non_empty);
            FontSettings {
                family: get("family"),
                mono: get("mono"),
                icons: get("icons"),
                fallback: t
                    .get::<Vec<String>>("fallback")
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(non_empty)
                    .collect(),
                // Clamp so a typo ("size = 400") can't make the UI unusable.
                size: t.get::<f32>("size").ok().filter(|s| *s > 0.0).map(|s| s.clamp(8.0, 48.0)),
                weight: t
                    .get::<String>("weight")
                    .map(|s| FontWeight::from_str(&s))
                    .unwrap_or(default.weight),
            }
        }
        _ => default.clone(),
    }
}

fn parse_bar(gl: &mlua::Table, default: &BarConfig) -> BarConfig {
    let Ok(bt) = gl.get::<mlua::Table>("bar") else {
        return default.clone();
    };
    let enabled = get_bool(&bt, "enabled", default.enabled);
    let position = bt
        .get::<String>("position")
        .map(|s| BarPosition::from_str(&s))
        .unwrap_or(default.position);
    let shell = get_bool(&bt, "shell", default.shell);

    let height = match bt.get::<mlua::Value>("height") {
        Ok(mlua::Value::Integer(n)) if n > 0 => n as u32,
        Ok(mlua::Value::Number(n)) if n > 0.0 => n as u32,
        _ => default.height,
    };
    let list = |key: &str, d: &Vec<String>| -> Vec<String> {
        match bt.get::<mlua::Value>(key) {
            Ok(mlua::Value::Table(t)) => t
                .sequence_values::<String>()
                .filter_map(Result::ok)
                .collect(),
            _ => d.clone(),
        }
    };

    // Per-widget options live under `bar.widgets`, so the sections stay a
    // plain list of names.
    let widgets = bt.get::<mlua::Table>("widgets").ok();
    let sub = |name: &str| widgets.as_ref().and_then(|w| w.get::<mlua::Table>(name).ok());
    let text = |t: &Option<mlua::Table>, key: &str| -> Option<String> {
        t.as_ref().and_then(|t| t.get::<String>(key).ok()).filter(|s| !s.trim().is_empty())
    };

    let clock_format = text(&sub("clock"), "format").unwrap_or_else(|| default.clock_format.clone());
    let disk_mount = text(&sub("disk"), "mount").unwrap_or_else(|| default.disk_mount.clone());

    let weather = match sub("weather") {
        Some(w) => match (w.get::<f64>("lat"), w.get::<f64>("lon")) {
            (Ok(lat), Ok(lon)) => Some(BarWeather {
                lat,
                lon,
                units: text(&Some(w.clone()), "units").unwrap_or_else(|| "metric".into()),
            }),
            _ => default.weather.clone(),
        },
        None => default.weather.clone(),
    };

    // `custom = { uptime = { command = "uptime -p", interval = 30 } }`. Lua
    // map order is unspecified, so sort by name for a stable bar.
    let mut custom: Vec<BarCustom> = Vec::new();
    if let Some(ct) = sub("custom") {
        for pair in ct.pairs::<String, mlua::Table>().filter_map(Result::ok) {
            let (name, t) = pair;
            let Ok(command) = t.get::<String>("command") else { continue };
            let interval = match t.get::<mlua::Value>("interval") {
                Ok(mlua::Value::Integer(n)) if n > 0 => n as u32,
                Ok(mlua::Value::Number(n)) if n >= 1.0 => n as u32,
                _ => 10,
            };
            custom.push(BarCustom { name, command, interval });
        }
        custom.sort_by(|a, b| a.name.cmp(&b.name));
    } else {
        custom = default.custom.clone();
    }

    BarConfig {
        enabled,
        position,
        shell,
        height,
        left: list("left", &default.left),
        center: list("center", &default.center),
        right: list("right", &default.right),
        clock_format,
        disk_mount,
        weather,
        custom,
    }
}

/// A Lua boolean field with a default. `get::<bool>` alone is a trap: mlua
/// turns a missing (nil) field into `false`, so a `true` default was ignored.
fn get_bool(t: &mlua::Table, key: &str, default: bool) -> bool {
    match t.get::<mlua::Value>(key) {
        Ok(mlua::Value::Boolean(b)) => b,
        _ => default,
    }
}

#[derive(Debug, Clone)]
pub struct VeilConfig {
    pub quality: Quality,
    /// Compositor frame-rate cap. `0` = match the display's refresh rate.
    pub fps: u32,
    /// Offer the host compositor's clipboard to clients inside Veil, and push
    /// their copies back out (`clipboard_sync` in Lua). Turn off for privacy.
    pub clipboard_sync: bool,
    /// DRM display-mode refresh selection (`refresh` in Lua).
    pub refresh: RefreshPref,
    pub cage_timeout_secs: u32,
    pub input: bool,
    /// Whether to use GPU compute shaders for frame encoding (halfblock/luma).
    /// Defaults to true. Set `gpu_render = false` in config.lua to disable.
    pub gpu_render: bool,
    /// Which GPU to prefer when `gpu_render` is on: "high" (default) or "low".
    pub gpu_power: GpuPower,
    pub keybinds: Keybinds,
    pub output: OutputPref,
    /// Bare background color (RGB), shown wherever no window covers —
    /// otherwise it's plain black, an actual void with zero windows open
    /// (which now happens on purpose, since Alt+D lets you get there).
    /// `background = "#RRGGBB"` in Lua. Defaults to a cement grey.
    pub background: [u8; 3],
    /// Resolved color set for launcher/help/sidebar chrome. `theme = "..."`
    /// in config.lua selects the preset; `background` above already carries
    /// the resolved override precedence (preset default, unless overridden).
    pub theme: Theme,
    /// Which preset `theme` came from — kept alongside the resolved colors
    /// purely for round-tripping to things like `veil-host probe`, which
    /// wants to display the name, not the raw color bytes.
    pub theme_name: ThemeName,
    /// `theme_mode` — only meaningful for `theme = "wallpaper"`.
    pub theme_mode: ThemeMode,
    /// `font = { ... }` — see [`FontSettings`].
    pub font: FontSettings,
    /// `bar = { ... }` config — see `BarConfig`.
    pub bar: BarConfig,
    /// Static wallpaper, if configured.
    pub wallpaper: Option<WallpaperConfig>,
}

impl Default for VeilConfig {
    fn default() -> Self {
        Self {
            quality: Quality::Auto,
            // 0 = match the display (so a 180 Hz panel is not capped to 60).
            fps: 0,
            clipboard_sync: true,
            refresh: RefreshPref::Preferred,
            cage_timeout_secs: 8,
            input: true,
            gpu_render: true,
            gpu_power: GpuPower::High,
            keybinds: Keybinds::default(),
            output: OutputPref::Auto,
            background: [0x8c, 0x8c, 0x8c],
            theme: Theme::for_name(ThemeName::Default),
            theme_name: ThemeName::Default,
            theme_mode: ThemeMode::Dark,
            font: FontSettings::default(),
            bar: BarConfig::default(),
            wallpaper: None,
        }
    }
}

// ---- unknown-key warnings --------------------------------------------------
// The parsers read what they know and ignore the rest, so a typo (or a key the
// code never read, like the old `bar.apps`) used to fail silently. These
// tables mirror what the parsers above read; extend them with the parsers.

const TOP_KEYS: &[&str] = &[
    "quality", "fps", "clipboard_sync", "refresh", "cage_timeout_secs", "input",
    "gpu_render", "gpu_power", "output", "background", "theme", "theme_mode",
    "keybinds", "apps", "font", "bar", "wallpaper",
];
const FONT_KEYS: &[&str] = &["family", "mono", "icons", "fallback", "size", "weight"];
const WALLPAPER_KEYS: &[&str] = &["engine", "paths", "path", "interval", "shuffle", "fit"];
const BAR_KEYS: &[&str] = &[
    "enabled", "position", "shell", "height", "left", "center", "right", "widgets",
];
const WIDGET_KEYS: &[&str] = &["clock", "disk", "weather", "custom"];

/// Keys a `keybinds` table accepts (also valid at top level when there is no
/// `keybinds` table, because the parser falls back to the globals).
fn keybind_keys() -> Vec<&'static str> {
    ["mod_key", "apps"]
        .into_iter()
        .chain(KEYBIND_FIELDS.iter().map(|(f, _)| *f))
        .collect()
}

fn global_names(gl: &mlua::Table) -> std::collections::HashSet<String> {
    gl.pairs::<String, mlua::Value>()
        .flatten()
        .map(|(k, _)| k)
        .collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The known key closest to `name`, if it is plausibly a typo of it.
fn closest<'a>(name: &str, known: &[&'a str]) -> Option<&'a str> {
    known
        .iter()
        .map(|k| (edit_distance(name, k), *k))
        .filter(|(d, _)| *d <= 2 && *d < name.chars().count())
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// Keys that parse fine in Lua but that no code ever read, with what to do.
fn dead_key_hint(full: &str) -> Option<&'static str> {
    match full {
        "bar.apps" => Some("bar tiles come from the Launch binds in `apps`"),
        _ => None,
    }
}

fn unknown_key_message(path: &str, name: &str, known: &[&str]) -> String {
    let full = if path.is_empty() { name.to_string() } else { format!("{path}.{name}") };
    if let Some(hint) = dead_key_hint(&full) {
        return format!("[config] `{full}` is not read: {hint}");
    }
    match closest(name, known) {
        Some(c) => format!("[config] unknown key `{full}` (ignored); did you mean `{c}`?"),
        None => format!("[config] unknown key `{full}` (ignored)"),
    }
}

/// Warn about named keys of `t` that are not in `known`. Positional entries
/// (integer keys, like `wallpaper = { "a", "b" }`) are not names, so skipped.
fn lint_table(path: &str, t: &mlua::Table, known: &[&str], out: &mut Vec<String>) {
    let mut names: Vec<String> = t
        .pairs::<mlua::Value, mlua::Value>()
        .flatten()
        .filter_map(|(k, _)| match k {
            mlua::Value::String(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .filter(|n| !known.contains(&n.as_str()))
        .collect();
    names.sort();
    out.extend(names.iter().map(|n| unknown_key_message(path, n, known)));
}

/// Every warning for a loaded config. `builtin` is the set of globals that
/// existed before the user's script ran (the Lua stdlib), so only keys the
/// script itself defined are checked. Helper functions are left alone.
fn lint_config(gl: &mlua::Table, builtin: &std::collections::HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let kb = gl.get::<mlua::Table>("keybinds").ok();
    let mut top_known: Vec<&str> = TOP_KEYS.to_vec();
    if kb.is_none() {
        top_known.extend(keybind_keys());
    }

    let mut user: Vec<String> = gl
        .pairs::<String, mlua::Value>()
        .flatten()
        .filter(|(k, v)| !builtin.contains(k) && !matches!(v, mlua::Value::Function(_)))
        .map(|(k, _)| k)
        .filter(|k| !top_known.contains(&k.as_str()))
        .collect();
    user.sort();
    out.extend(user.iter().map(|k| unknown_key_message("", k, &top_known)));

    if let Some(kb) = &kb {
        lint_table("keybinds", kb, &keybind_keys(), &mut out);
    }
    if let Ok(t) = gl.get::<mlua::Table>("font") {
        lint_table("font", &t, FONT_KEYS, &mut out);
    }
    if let Ok(t) = gl.get::<mlua::Table>("wallpaper") {
        lint_table("wallpaper", &t, WALLPAPER_KEYS, &mut out);
    }
    if let Ok(bar) = gl.get::<mlua::Table>("bar") {
        lint_table("bar", &bar, BAR_KEYS, &mut out);
        if let Ok(w) = bar.get::<mlua::Table>("widgets") {
            lint_table("bar.widgets", &w, WIDGET_KEYS, &mut out);
            for (name, keys) in [
                ("clock", &["format"][..]),
                ("disk", &["mount"][..]),
                ("weather", &["lat", "lon", "units"][..]),
            ] {
                if let Ok(t) = w.get::<mlua::Table>(name) {
                    lint_table(&format!("bar.widgets.{name}"), &t, keys, &mut out);
                }
            }
            if let Ok(c) = w.get::<mlua::Table>("custom") {
                for (name, t) in c.pairs::<String, mlua::Table>().flatten() {
                    lint_table(
                        &format!("bar.widgets.custom.{name}"),
                        &t,
                        &["command", "interval"],
                        &mut out,
                    );
                }
            }
        }
    }
    out
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
    /// Resolve the configured quality to a concrete mode. `Auto` runs the
    /// detector; `Sixel` (unimplemented) is treated like `Auto` with a warning.
    pub fn resolved_quality(&self) -> Quality {
        match self.quality {
            Quality::Auto => detect_quality(),
            Quality::Sixel => {
                eprintln!("[veil-config] quality = \"sixel\" is not implemented; auto-detecting");
                detect_quality()
            }
            ref q => q.clone(),
        }
    }
}

/// Canonical config directory `$XDG_CONFIG_HOME/veil` or `~/.config/veil`.
fn dirs_config() -> std::path::PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config")
        })
        .join("veil")
}

/// Resolve the canonical config file path. Checks cwd `./config.lua` first,
/// then `$XDG_CONFIG_HOME/veil/config.lua` (or `~/.config/veil/config.lua`).
pub fn config_path() -> Option<std::path::PathBuf> {
    [
        std::path::PathBuf::from("config.lua"),
        dirs_config().join("config.lua"),
    ]
    .into_iter()
    .find(|p| p.exists())
}

/// Load config from default locations (`config_path()`). Falls back to defaults.
pub fn load_user_config() -> VeilConfig {
    config_path().map(|p| load(&p)).unwrap_or_default()
}

/// Load config from `path`. Returns `Err(error_msg)` on read or parse failure.
pub fn try_load(path: &Path) -> Result<VeilConfig, String> {
    if !path.exists() {
        return Ok(VeilConfig::default());
    }

    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return Err(format!("Read error: {e}")),
    };

    let lua = Lua::new();
    let builtin = global_names(&lua.globals());
    if let Err(e) = lua.load(&src).exec() {
        return Err(format!("{e}"));
    }
    for w in lint_config(&lua.globals(), &builtin) {
        eprintln!("{w}");
    }

    let d = VeilConfig::default();
    let gl = lua.globals();

    // Theme resolves first: it supplies the *default* background, which an
    // explicit `background = "#RRGGBB"` key then overrides — same
    // precedence `background` always had, theme or not.
    let theme_name = gl
        .get::<String>("theme")
        .map(|s| ThemeName::from_str(&s))
        .unwrap_or(d.theme_name);
    let theme = Theme::for_name(theme_name);

    Ok(VeilConfig {
        quality: gl
            .get::<String>("quality")
            .map(|s| Quality::parse_str(&s))
            .unwrap_or(d.quality),
        fps: gl.get::<u32>("fps").unwrap_or(d.fps),
        clipboard_sync: get_bool(&gl, "clipboard_sync", d.clipboard_sync),
        refresh: gl
            .get::<String>("refresh")
            .map(|s| RefreshPref::parse_str(&s))
            .unwrap_or(d.refresh),
        cage_timeout_secs: gl
            .get::<u32>("cage_timeout_secs")
            .unwrap_or(d.cage_timeout_secs),
        input: get_bool(&gl, "input", d.input),
        gpu_render: get_bool(&gl, "gpu_render", d.gpu_render),
        gpu_power: gl
            .get::<String>("gpu_power")
            .map(|s| GpuPower::parse_str(&s))
            .unwrap_or(d.gpu_power),
        keybinds: parse_keybinds(&gl),
        output: gl
            .get::<String>("output")
            .map(|s| OutputPref::from_str(&s))
            .unwrap_or(d.output),
        background: gl
            .get::<String>("background")
            .ok()
            .and_then(|s| parse_hex_color(&s))
            .unwrap_or(theme.background),
        theme,
        theme_name,
        theme_mode: gl
            .get::<String>("theme_mode")
            .map(|s| ThemeMode::from_str(&s))
            .unwrap_or(d.theme_mode),
        bar: parse_bar(&gl, &d.bar),
        font: parse_font(&gl, &d.font),
        wallpaper: parse_wallpaper(&gl),
    })
}

/// Load config from `path`. Falls back to defaults on any error or missing file.
pub fn load(path: &Path) -> VeilConfig {
    match try_load(path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[config] failed to parse {:?}: {}, using defaults", path, e);
            VeilConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detect_ladder() {
        let q = |p: &[(&str, &str)]| detect_quality_with(env_of(p));
        assert_eq!(q(&[("TERM", "xterm-kitty")]), Quality::Kitty);
        assert_eq!(q(&[("TERM_PROGRAM", "WezTerm")]), Quality::Kitty);
        assert_eq!(
            q(&[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]),
            Quality::Pixel
        );
        assert_eq!(q(&[("TERM", "xterm-256color")]), Quality::Pixel256);
        assert_eq!(q(&[("TERM", "linux")]), Quality::Pixel16);
        assert_eq!(q(&[("TERM", "vt100")]), Quality::AsciiLuma);
        assert_eq!(q(&[]), Quality::AsciiLuma);
    }

    #[test]
    fn detect_locale_downgrades_only_when_set_and_not_utf8() {
        let tc = ("COLORTERM", "truecolor");
        let q = |p: &[(&str, &str)]| detect_quality_with(env_of(p));
        assert_eq!(q(&[tc, ("LANG", "C")]), Quality::AsciiLuma);
        assert_eq!(q(&[tc, ("LANG", "en_US.UTF-8")]), Quality::Pixel);
        assert_eq!(q(&[tc]), Quality::Pixel); // unset locale is not evidence
        // LC_ALL wins over LANG.
        assert_eq!(
            q(&[tc, ("LC_ALL", "en_US.utf8"), ("LANG", "C")]),
            Quality::Pixel
        );
        // Kitty graphics don't depend on glyphs.
        assert_eq!(q(&[("TERM", "xterm-kitty"), ("LANG", "C")]), Quality::Kitty);
    }

    #[test]
    fn quality_roundtrip() {
        for q in [
            Quality::Auto,
            Quality::Kitty,
            Quality::Pixel,
            Quality::Pixel256,
            Quality::Pixel16,
            Quality::AsciiLuma,
            Quality::AsciiEdge,
            Quality::Sixel,
        ] {
            assert_eq!(Quality::parse_str(q.as_str()), q);
        }
    }

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

    #[test]
    fn default_keybinds_includes_reload() {
        let kb = Keybinds::default();
        assert_eq!(kb.action_for('c'), Some(Action::ReloadConfig));
    }

    #[test]
    fn parse_custom_reload_keybind() {
        let lua = Lua::new();
        lua.load(
            r#"
            keybinds = {
                mod_key = "alt",
                reload_config = "x",
            }
        "#,
        )
        .exec()
        .unwrap();
        let kb = parse_keybinds(&lua.globals());
        assert_eq!(kb.mod_key, ModKey::Alt);
        assert_eq!(kb.action_for('x'), Some(Action::ReloadConfig));
    }
}

#[cfg(test)]
mod app_bind_tests {
    use super::*;

    fn load_str(name: &str, lua: &str) -> VeilConfig {
        let p = std::env::temp_dir().join(format!("veil-cfg-{name}-{}.lua", std::process::id()));
        std::fs::write(&p, lua).unwrap();
        let cfg = try_load(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        cfg
    }

    fn launches(cfg: &VeilConfig) -> Vec<(char, String)> {
        cfg.keybinds
            .binds
            .iter()
            .filter_map(|(k, a)| match a {
                Action::Launch(c) => Some((*k, c.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn false_or_empty_string_unbinds_an_action() {
        let cfg = load_str(
            "unbind",
            r#"keybinds = { wallpaper_chooser = false, reload_config = "", close = "x" }"#,
        );
        let kb = &cfg.keybinds;
        // Unbound: no key maps to them any more.
        assert!(!kb.binds.iter().any(|(_, a)| *a == Action::WallpaperChooser));
        assert!(!kb.binds.iter().any(|(_, a)| *a == Action::ReloadConfig));
        // A normal override still works, and untouched actions keep defaults.
        assert_eq!(kb.action_for('x'), Some(Action::Close));
        assert_eq!(kb.action_for('l'), Some(Action::FocusRight));
    }

    #[test]
    fn clipboard_sync_defaults_on_and_can_be_turned_off() {
        assert!(load_str("cb-default", "fps = 0").clipboard_sync);
        assert!(!load_str("cb-off", "clipboard_sync = false").clipboard_sync);
    }

    #[test]
    fn missing_booleans_keep_their_true_defaults() {
        // mlua reads a nil field as `false`; absent keys must keep defaults.
        let c = load_str("bools-absent", "fps = 0");
        assert!(c.input && c.gpu_render && c.clipboard_sync);
        let c = load_str("bools-off", "input = false\ngpu_render = false");
        assert!(!c.input && !c.gpu_render);
    }

    #[test]
    fn fps_defaults_to_match_the_display() {
        assert_eq!(load_str("fps-default", "quality = \"auto\"").fps, 0);
        assert_eq!(load_str("fps-set", "fps = 90").fps, 90);
    }

    #[test]
    fn absent_field_keeps_its_default() {
        let cfg = load_str("absent", r#"keybinds = { close = "x" }"#);
        assert_eq!(cfg.keybinds.action_for('y'), Some(Action::WallpaperChooser));
    }

    #[test]
    fn ordered_apps_keep_declared_order_and_enter_maps_to_cr() {
        let cfg = load_str(
            "ordered",
            r#"apps = { { "enter", "kitty" }, { key = "b", exec = "helium-browser" }, { "e", "dolphin" } }"#,
        );
        assert_eq!(
            launches(&cfg),
            vec![
                ('\r', "kitty".into()),
                ('b', "helium-browser".into()),
                ('e', "dolphin".into())
            ]
        );
        assert_eq!(
            cfg.keybinds.action_for('\r'),
            Some(Action::Launch("kitty".into()))
        );
    }

    #[test]
    fn map_form_still_works_in_key_order() {
        let cfg = load_str("map", r#"apps = { t = "foot", b = "firefox" }"#);
        assert_eq!(
            launches(&cfg),
            vec![('b', "firefox".into()), ('t', "foot".into())]
        );
    }
}

#[cfg(test)]
mod font_tests {
    use super::*;

    fn parse(src: &str) -> FontSettings {
        let lua = mlua::Lua::new();
        lua.load(src).exec().unwrap();
        parse_font(&lua.globals(), &FontSettings::default())
    }

    #[test]
    fn string_shorthand_sets_the_family() {
        let f = parse(r#"font = "Inter""#);
        assert_eq!(f.family.as_deref(), Some("Inter"));
        assert_eq!(f.weight, FontWeight::Regular);
    }

    #[test]
    fn table_form_reads_every_field() {
        let f = parse(r#"font = { family = "Inter", mono = "JetBrains Mono", icons = "Symbols Nerd Font", fallback = { "Noto Sans CJK JP", "Noto Color Emoji" }, size = 15, weight = "bold" }"#);
        assert_eq!(f.family.as_deref(), Some("Inter"));
        assert_eq!(f.mono.as_deref(), Some("JetBrains Mono"));
        assert_eq!(f.icons.as_deref(), Some("Symbols Nerd Font"));
        assert_eq!(f.fallback, vec!["Noto Sans CJK JP", "Noto Color Emoji"]);
        assert_eq!(f.size, Some(15.0));
        assert_eq!(f.weight, FontWeight::Bold);
    }

    #[test]
    fn missing_or_junk_values_fall_back_to_defaults() {
        assert_eq!(parse("x = 1"), FontSettings::default());
        assert_eq!(parse(r#"font = { family = "  " }"#).family, None);
        assert_eq!(parse("font = 42"), FontSettings::default());
    }

    #[test]
    fn absurd_sizes_are_clamped_not_trusted() {
        assert_eq!(parse("font = { size = 400 }").size, Some(48.0));
        assert_eq!(parse("font = { size = 1 }").size, Some(8.0));
        assert_eq!(parse("font = { size = -3 }").size, None);
    }
}

#[cfg(test)]
mod bar_tests {
    use super::*;

    fn parse(src: &str) -> BarConfig {
        let lua = mlua::Lua::new();
        lua.load(src).exec().unwrap();
        parse_bar(&lua.globals(), &BarConfig::default())
    }

    #[test]
    fn no_bar_table_keeps_defaults() {
        assert_eq!(parse(""), BarConfig::default());
    }

    #[test]
    fn sections_height_and_position() {
        let b = parse(
            r#"bar = { position = "top", height = 40,
                left = { "activities", "workspaces" }, center = {}, right = { "cpu", "clock" } }"#,
        );
        assert_eq!(b.position, BarPosition::Top);
        assert_eq!(b.height, 40);
        assert_eq!(b.left, ["activities", "workspaces"]);
        assert!(b.center.is_empty(), "an explicit empty section stays empty");
        assert_eq!(b.right, ["cpu", "clock"]);
    }

    #[test]
    fn widget_options() {
        let b = parse(
            r#"bar = { widgets = {
                clock = { format = "%H:%M" },
                disk = { mount = "/home" },
                weather = { lat = 48.1, lon = 11.5, units = "imperial" },
                custom = { zeta = { command = "echo z" }, alpha = { command = "uptime -p", interval = 30 } },
            } }"#,
        );
        assert_eq!(b.clock_format, "%H:%M");
        assert_eq!(b.disk_mount, "/home");
        assert_eq!(b.weather, Some(BarWeather { lat: 48.1, lon: 11.5, units: "imperial".into() }));
        let names: Vec<_> = b.custom.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"], "sorted by name, not Lua map order");
        assert_eq!(b.custom[0].interval, 30);
        assert_eq!(b.custom[1].interval, 10, "default interval");
    }

    #[test]
    fn weather_without_coordinates_is_off() {
        assert_eq!(parse(r#"bar = { widgets = { weather = { units = "metric" } } }"#).weather, None);
    }

    #[test]
    fn existing_keys_still_work() {
        let b = parse(r#"bar = { enabled = false, position = "bottom", shell = true }"#);
        assert!(!b.enabled && b.shell);
        assert_eq!(b.position, BarPosition::Bottom);
    }
}

#[cfg(test)]
mod lint_tests {
    use super::*;

    fn lint(src: &str) -> Vec<String> {
        let lua = Lua::new();
        let builtin = global_names(&lua.globals());
        lua.load(src).exec().unwrap();
        lint_config(&lua.globals(), &builtin)
    }

    #[test]
    fn a_typo_names_the_key_and_suggests_the_fix() {
        let w = lint(r#"bar = { heigth = 40 }"#);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("bar.heigth") && w[0].contains("did you mean `height`"), "{w:?}");
        let w = lint(r#"thme = "nord""#);
        assert!(w[0].contains("`thme`") && w[0].contains("`theme`"), "{w:?}");
    }

    #[test]
    fn the_old_bar_apps_list_gets_a_pointer_not_a_guess() {
        let w = lint(r#"bar = { apps = { { name = "x", exec = "x" } } }"#);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("not read") && w[0].contains("`apps`"), "{w:?}");
    }

    #[test]
    fn nested_widget_and_font_keys_are_checked() {
        let w = lint(
            r#"font = { famly = "x" }
               bar = { widgets = { weather = { lat = 1, lon = 2, unit = "metric" },
                                   custom = { up = { cmd = "uptime" } } } }"#,
        );
        let joined = w.join("\n");
        assert!(joined.contains("font.famly") && joined.contains("`family`"), "{w:?}");
        assert!(joined.contains("bar.widgets.weather.unit"), "{w:?}");
        assert!(joined.contains("bar.widgets.custom.up.cmd"), "{w:?}");
    }

    #[test]
    fn valid_shapes_stay_quiet() {
        // top-level keybind keys (no `keybinds` table), positional wallpaper
        // entries, helper functions and locals are all fine
        assert!(lint(r#"mod_key = "alt"
                         focus_left = "h"
                         wallpaper = { "a.png", "b.png", shuffle = true }
                         function helper() end"#).is_empty());
        assert!(lint(r#"keybinds = { mod_key = "alt", fullscreen = "f", apps = { { "enter", "kitty" } } }
                         font = "Inter""#).is_empty());
    }

    #[test]
    fn the_shipped_config_has_no_warnings() {
        // keeps ./config.lua honest as the parser grows
        assert_eq!(lint(include_str!("../../../config.lua")), Vec::<String>::new());
    }
}
