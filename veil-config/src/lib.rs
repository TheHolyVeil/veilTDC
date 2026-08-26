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
    /// Switch between Dwindle and Scroll tiling modes.
    ToggleLayout,
    /// Fullscreen the focused window (compositor-triggered — same effect as
    /// a client's own fullscreen request, just initiated from a keybind
    /// instead of waiting for the app to ask).
    ToggleFullscreen,
    /// Hot-reload config file on demand.
    ReloadConfig,
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
            Self::FocusLeft        => "focus left".to_string(),
            Self::FocusRight       => "focus right".to_string(),
            Self::FocusUp          => "focus up".to_string(),
            Self::FocusDown        => "focus down".to_string(),
            Self::Swap             => "swap with next".to_string(),
            Self::Rotate           => "rotate split".to_string(),
            Self::Close            => "close window".to_string(),
            Self::ResizeGrow       => "resize grow".to_string(),
            Self::ResizeShrink     => "resize shrink".to_string(),
            Self::ToggleLayout     => "toggle layout mode".to_string(),
            Self::ToggleFullscreen => "toggle fullscreen".to_string(),
            Self::ReloadConfig     => "reload config".to_string(),
            Self::VolumeUp         => "volume up".to_string(),
            Self::VolumeDown       => "volume down".to_string(),
            Self::VolumeMute       => "toggle mute".to_string(),
            Self::BrightnessUp     => "brightness up".to_string(),
            Self::BrightnessDown   => "brightness down".to_string(),
            Self::Launch(cmd)      => format!("launch: {cmd}"),
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
                ('w', Action::ToggleLayout),
                ('f', Action::ToggleFullscreen),
                ('c', Action::ReloadConfig),
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
    const FIELDS: [(&str, Action); 17] = [
        ("focus_left",      Action::FocusLeft),
        ("focus_right",     Action::FocusRight),
        ("focus_up",        Action::FocusUp),
        ("focus_down",      Action::FocusDown),
        ("swap",            Action::Swap),
        ("rotate",          Action::Rotate),
        ("close",           Action::Close),
        ("resize_grow",     Action::ResizeGrow),
        ("resize_shrink",   Action::ResizeShrink),
        ("toggle_layout",   Action::ToggleLayout),
        ("fullscreen",      Action::ToggleFullscreen),
        ("reload_config",   Action::ReloadConfig),
        ("volume_up",       Action::VolumeUp),
        ("volume_down",     Action::VolumeDown),
        ("volume_mute",     Action::VolumeMute),
        ("brightness_up",   Action::BrightnessUp),
        ("brightness_down", Action::BrightnessDown),
    ];

    let mut binds: Vec<(char, Action)> = FIELDS.iter().filter_map(|(field, action)| {
        let key = t.get::<String>(*field).ok()
            .and_then(|s| s.chars().next())
            .map(|c| c.to_ascii_lowercase())
            .or_else(|| {
                default.binds.iter().find(|(_, a)| a == action).map(|(c, _)| *c)
            })?;
        Some((key, action.clone()))
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
}

impl ThemeName {
    fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "nord"                       => Self::Nord,
            "dracula"                     => Self::Dracula,
            "catppuccin" | "mocha"       => Self::Catppuccin,
            "gruvbox"                     => Self::Gruvbox,
            "everforest"                  => Self::Everforest,
            "tokyonight" | "tokyo_night" | "tokyo-night" => Self::TokyoNight,
            "solarized"                   => Self::Solarized,
            "rosepine" | "rose_pine" | "rose-pine" => Self::RosePine,
            "monokai"                     => Self::Monokai,
            "onedark" | "one_dark" | "one-dark" => Self::OneDark,
            "deepsage" | "deep_sage" | "deep-sage" | "sage" => Self::DeepSage,
            _                             => Self::Default,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Default    => "default",
            Self::Nord       => "nord",
            Self::Dracula    => "dracula",
            Self::Catppuccin => "catppuccin",
            Self::Gruvbox    => "gruvbox",
            Self::Everforest => "everforest",
            Self::TokyoNight => "tokyonight",
            Self::Solarized  => "solarized",
            Self::RosePine   => "rosepine",
            Self::Monokai    => "monokai",
            Self::OneDark    => "onedark",
            Self::DeepSage   => "deepsage",
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
    pub panel_bg:   [u8; 4],
    pub border:     [u8; 4],
    pub header:     [u8; 4],
    pub text:       [u8; 4],
    pub text_dim:   [u8; 4],
    pub accent:     [u8; 4],
    pub highlight:  [u8; 4],
}

impl Theme {
    pub fn for_name(name: ThemeName) -> Theme {
        match name {
            ThemeName::Default => Theme {
                background: [0x8c, 0x8c, 0x8c],
                panel_bg:   [10, 0, 16, 245],
                border:     [199, 146, 234, 255],
                header:     [255, 215, 0, 255],
                text:       [199, 146, 234, 255],
                text_dim:   [150, 150, 180, 255],
                accent:     [128, 222, 234, 255],
                highlight:  [65, 35, 85, 255],
            },
            ThemeName::Nord => Theme {
                background: [0x2e, 0x34, 0x40],
                panel_bg:   [0x3b, 0x42, 0x52, 245],
                border:     [0x88, 0xc0, 0xd0, 255],
                header:     [0xeb, 0xcb, 0x8b, 255],
                text:       [0xe5, 0xe9, 0xf0, 255],
                text_dim:   [0x61, 0x6e, 0x88, 255],
                accent:     [0x81, 0xa1, 0xc1, 255],
                highlight:  [0x43, 0x4c, 0x5e, 255],
            },
            ThemeName::Dracula => Theme {
                background: [0x28, 0x2a, 0x36],
                panel_bg:   [0x1e, 0x1f, 0x29, 245],
                border:     [0xbd, 0x93, 0xf9, 255],
                header:     [0xf1, 0xfa, 0x8c, 255],
                text:       [0xf8, 0xf8, 0xf2, 255],
                text_dim:   [0x62, 0x72, 0xa4, 255],
                accent:     [0x8b, 0xe9, 0xfd, 255],
                highlight:  [0x44, 0x47, 0x5a, 255],
            },
            ThemeName::Catppuccin => Theme {
                background: [0x1e, 0x1e, 0x2e],
                panel_bg:   [0x18, 0x18, 0x25, 245],
                border:     [0xcb, 0xa6, 0xf7, 255],
                header:     [0xf9, 0xe2, 0xaf, 255],
                text:       [0xcd, 0xd6, 0xf4, 255],
                text_dim:   [0x6c, 0x70, 0x86, 255],
                accent:     [0x89, 0xdc, 0xeb, 255],
                highlight:  [0x31, 0x32, 0x44, 255],
            },
            ThemeName::Gruvbox => Theme {
                background: [0x28, 0x28, 0x28],
                panel_bg:   [0x1d, 0x20, 0x21, 245],
                border:     [0xd3, 0x86, 0x9b, 255],
                header:     [0xfa, 0xbd, 0x2f, 255],
                text:       [0xeb, 0xdb, 0xb2, 255],
                text_dim:   [0x92, 0x83, 0x74, 255],
                accent:     [0x83, 0xa5, 0x98, 255],
                highlight:  [0x3c, 0x38, 0x36, 255],
            },
            ThemeName::Everforest => Theme {
                background: [0x2d, 0x35, 0x3b],
                panel_bg:   [0x23, 0x2a, 0x2e, 245],
                border:     [0xd6, 0x99, 0xb6, 255],
                header:     [0xdb, 0xbc, 0x7f, 255],
                text:       [0xd3, 0xc6, 0xaa, 255],
                text_dim:   [0x7a, 0x82, 0x87, 255],
                accent:     [0x7f, 0xbb, 0xb3, 255],
                highlight:  [0x3d, 0x48, 0x4d, 255],
            },
            ThemeName::TokyoNight => Theme {
                background: [0x1a, 0x1b, 0x26],
                panel_bg:   [0x16, 0x16, 0x1e, 245],
                border:     [0xbb, 0x9a, 0xf7, 255],
                header:     [0xe0, 0xaf, 0x68, 255],
                text:       [0xc0, 0xca, 0xf5, 255],
                text_dim:   [0x56, 0x5f, 0x89, 255],
                accent:     [0x7d, 0xcf, 0xff, 255],
                highlight:  [0x29, 0x2e, 0x42, 255],
            },
            ThemeName::Solarized => Theme {
                background: [0x00, 0x2b, 0x36],
                panel_bg:   [0x07, 0x36, 0x42, 245],
                border:     [0x6c, 0x71, 0xc4, 255],
                header:     [0xb5, 0x89, 0x00, 255],
                text:       [0x83, 0x94, 0x96, 255],
                text_dim:   [0x58, 0x6e, 0x75, 255],
                accent:     [0x2a, 0xa1, 0x98, 255],
                highlight:  [0x0a, 0x45, 0x52, 255],
            },
            ThemeName::RosePine => Theme {
                background: [0x19, 0x17, 0x24],
                panel_bg:   [0x1f, 0x1d, 0x2e, 245],
                border:     [0xc4, 0xa7, 0xe7, 255],
                header:     [0xf6, 0xc1, 0x77, 255],
                text:       [0xe0, 0xde, 0xf4, 255],
                text_dim:   [0x6e, 0x6a, 0x86, 255],
                accent:     [0x9c, 0xcf, 0xd8, 255],
                highlight:  [0x26, 0x23, 0x3a, 255],
            },
            ThemeName::Monokai => Theme {
                background: [0x27, 0x28, 0x22],
                panel_bg:   [0x1e, 0x1f, 0x1c, 245],
                border:     [0xae, 0x81, 0xff, 255],
                header:     [0xe6, 0xdb, 0x74, 255],
                text:       [0xf8, 0xf8, 0xf2, 255],
                text_dim:   [0x75, 0x71, 0x5e, 255],
                accent:     [0x66, 0xd9, 0xef, 255],
                highlight:  [0x3e, 0x3d, 0x32, 255],
            },
            ThemeName::OneDark => Theme {
                background: [0x28, 0x2c, 0x34],
                panel_bg:   [0x21, 0x25, 0x2b, 245],
                border:     [0xc6, 0x78, 0xdd, 255],
                header:     [0xe5, 0xc0, 0x7b, 255],
                text:       [0xab, 0xb2, 0xbf, 255],
                text_dim:   [0x5c, 0x63, 0x70, 255],
                accent:     [0x56, 0xb6, 0xc2, 255],
                highlight:  [0x2c, 0x31, 0x3a, 255],
            },
            // Deep Sage — Abyss's own. Desaturated dark green-charcoal base,
            // muted sage border, warm tan-gold header for contrast against
            // all that green, soft teal-sage accent. Not sourced from any
            // published palette — hand-tuned, so touch it before anyone
            // else's preferences do.
            ThemeName::DeepSage => Theme {
                background: [0x23, 0x2b, 0x26],
                panel_bg:   [0x1a, 0x20, 0x1c, 245],
                border:     [0x87, 0xa0, 0x8d, 255],
                header:     [0xc9, 0xb4, 0x58, 255],
                text:       [0xdf, 0xe6, 0xe0, 255],
                text_dim:   [0x6b, 0x7a, 0x70, 255],
                accent:     [0x7f, 0xae, 0x9b, 255],
                highlight:  [0x2f, 0x38, 0x30, 255],
            },
        }
    }
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
    /// Resolved color set for launcher/help/sidebar chrome. `theme = "..."`
    /// in config.lua selects the preset; `background` above already carries
    /// the resolved override precedence (preset default, unless overridden).
    pub theme:              Theme,
    /// Which preset `theme` came from — kept alongside the resolved colors
    /// purely for round-tripping to things like `veil-host probe`, which
    /// wants to display the name, not the raw color bytes.
    pub theme_name:         ThemeName,
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
            theme:             Theme::for_name(ThemeName::Default),
            theme_name:        ThemeName::Default,
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
    config_path()
        .map(|p| load(&p))
        .unwrap_or_default()
}

/// Load config from `path`. Returns `Err(error_msg)` on read or parse failure.
pub fn try_load(path: &Path) -> Result<VeilConfig, String> {
    if !path.exists() {
        return Ok(VeilConfig::default());
    }

    let src = match std::fs::read_to_string(path) {
        Ok(s)  => s,
        Err(e) => return Err(format!("Read error: {e}")),
    };

    let lua = Lua::new();
    if let Err(e) = lua.load(&src).exec() {
        return Err(format!("{e}"));
    }

    let d  = VeilConfig::default();
    let gl = lua.globals();

    // Theme resolves first: it supplies the *default* background, which an
    // explicit `background = "#RRGGBB"` key then overrides — same
    // precedence `background` always had, theme or not.
    let theme_name = gl.get::<String>("theme")
        .map(|s| ThemeName::from_str(&s))
        .unwrap_or(d.theme_name);
    let theme = Theme::for_name(theme_name);

    Ok(VeilConfig {
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
            .unwrap_or(theme.background),
        theme,
        theme_name,
    })
}

/// Load config from `path`. Falls back to defaults on any error or missing file.
pub fn load(path: &Path) -> VeilConfig {
    match try_load(path) {
        Ok(cfg) => cfg,
        Err(e)  => {
            eprintln!("[config] failed to parse {:?}: {}, using defaults", path, e);
            VeilConfig::default()
        }
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

    #[test]
    fn default_keybinds_includes_reload() {
        let kb = Keybinds::default();
        assert_eq!(kb.action_for('c'), Some(Action::ReloadConfig));
    }

    #[test]
    fn parse_custom_reload_keybind() {
        let lua = Lua::new();
        lua.load(r#"
            keybinds = {
                mod_key = "alt",
                reload_config = "x",
            }
        "#).exec().unwrap();
        let kb = parse_keybinds(&lua.globals());
        assert_eq!(kb.mod_key, ModKey::Alt);
        assert_eq!(kb.action_for('x'), Some(Action::ReloadConfig));
    }
}
