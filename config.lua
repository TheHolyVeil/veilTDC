-- veil-host configuration file
-- Place at ./config.lua or ~/.config/veil/config.lua

quality = "auto" -- auto | kitty | pixel | pixel256 | pixel16 | ascii | ascii_edge
fps = 0 -- compositor frame rate cap; 0 = match the display
clipboard_sync = true -- share the host clipboard with Veil apps (both ways); false = fully isolated
refresh = "max" -- DRM mode at the native size: preferred | max | <Hz, e.g. 144>
cage_timeout_secs = 8 -- timeout for caged output
input = true -- enable input forwarding
gpu_render = true -- use GPU for frame encoding
gpu_power = "high" -- high | low  (low = prefer the integrated GPU on hybrid laptops)

-- Color theme applied to the launcher overlay, help overlay, and background
-- (sidebar/bar will pick it up too once built). Does NOT recolor hosted app
-- windows — veil-host never touches a client's own buffer, only its own
-- chrome. Built-in presets:
--
--   "default"    -- original hardcoded look (amethyst/purple on cement grey).
--                    This is what you get with no `theme` line at all, so
--                    an existing config.lua renders pixel-identical to
--                    before theming existed.
--   "nord"       -- Arctic, bluish (nordtheme.com palette)
--   "dracula"    -- Dark purple/pink, high contrast (draculatheme.com)
--   "catppuccin" -- Mocha variant — soft pastel dark (catppuccin.com)
--                    ("mocha" also accepted as an alias)
--   "gruvbox"    -- Warm, retro-groove dark (morhetz/gruvbox)
--   "everforest" -- Green-tinted, low-contrast forest dark (sainnhe/everforest)
--   "tokyonight" -- Deep blue-purple night city (enkia/tokyo-night)
--                    ("tokyo_night" / "tokyo-night" also accepted)
--   "solarized"  -- Classic low-contrast blue-green (ethanschoonover.com)
--   "rosepine"   -- Muted rose/iris dark (rosepinetheme.com)
--                    ("rose_pine" / "rose-pine" also accepted)
--   "monokai"    -- Classic high-contrast editor theme
--   "onedark"    -- Atom's One Dark (JS ecosystem staple)
--                    ("one_dark" / "one-dark" also accepted)
--   "deepsage"   -- Abyss's own: deep, desaturated sage green, hand-tuned,
--                    not sourced from any published palette.
--                    ("deep_sage" / "deep-sage" / "sage" also accepted)
--   "wallpaper"  -- Colours derived from the current wallpaper (veil-palette).
--                    Shows the Deep Sage set until a wallpaper has loaded.
--                    Leans dark/light per `theme_mode` below.
--
theme = "default" -- default | nord | dracula | catppuccin | gruvbox | everforest
                   -- | tokyonight | solarized | rosepine | monokai | onedark | deepsage | wallpaper
theme_mode = "dark" -- dark | light | auto; only used by theme = "wallpaper" (auto goes light only for a very bright wallpaper)

-- Bare background color, shown wherever no window covers. Otherwise
-- uncovered space is pure black — an actual void, which you can now
-- deliberately land on at zero windows (Alt+D launcher).
--
-- This OVERRIDES the theme's own background when set — comment this line
-- out entirely to let `theme` above pick the background for you instead.
-- background = "#8c8c8c" -- hex, "#RRGGBB" or "RRGGBB" — cement grey default

-- Fonts: the one place every Veil surface (bar, launcher, help overlay, ...)
-- reads, applied live on reload. Every field is optional; anything unset falls
-- back to an installed font, and the built-in 5x7 bitmap font is the last
-- resort. A family is a name or an absolute path to a font file.
-- `font = "Inter"` is shorthand for `font = { family = "Inter" }`.
-- font = {
--   family = "Inter",             -- UI text
--   mono = "JetBrains Mono",      -- monospace text
--   icons = "Symbols Nerd Font",  -- icon glyphs
--   fallback = { "Noto Sans CJK JP", "Noto Color Emoji" }, -- missing glyphs, tried in order
--   size = 14,                    -- base px, clamped 8..48; the bar height scales with it
--   weight = "regular",           -- regular | bold
-- }

-- Status bar. `shell = true` runs the supervised shell bar (veil-bar, over
-- layer-shell) and falls back to the built-in 12px bitmap bar if it crashes;
-- `shell = false` means the built-in bar only. The shell bar takes its fonts
-- from `font` above and its colours from `theme`.
--
-- Widgets (names for left / center / right, drawn in the order listed):
--   workspaces  window_title  clock  battery  cpu  memory  disk  temp
--   network  audio  media  weather  systray  notifications
--   control_center  activities  custom:<name>
-- An unknown or unconfigured widget (e.g. weather without coordinates) is
-- skipped with a log line; it never takes the bar down.
--
-- App tiles come from the Launch binds in `apps` below. There is no
-- `bar.apps` list; the parser never read one.
bar = {
	enabled = true,   -- false hides the bar entirely and gives tiled windows
	                   -- back the full output height
	position = "top", -- top | bottom
	height = 36,      -- px at the default font size (scales with font.size); clamped 24..64
	shell = true,

	left   = { "window_title", "workspaces", "systray", "notifications", "activities" },
	center = { "clock", "weather" },
	right  = { "media", "cpu", "memory", "temp", "disk", "network", "audio", "control_center" },

	-- Per-widget options.
	widgets = {
		clock = { format = "%a %b %d  %H:%M" }, -- strftime
		disk  = { mount = "/" },
		-- Fill in your coordinates (no auto-location) and uncomment; until then weather is skipped.
		-- weather = { lat = 0.0, lon = 0.0, units = "metric" }, -- metric | imperial
		-- custom = { uptime = { command = "uptime -p", interval = 30 } }, -- then add "custom:uptime" to a section
	},
}

-- Notifications. The supervised veil-notify owns org.freedesktop.Notifications
-- and draws the toasts (top right) and the history panel behind the bar's bell.
-- `shell = false` stops Veil starting it, for when you run another daemon
-- (veil-notify queues behind an existing owner of the name rather than
-- taking it, so the other daemon wins either way).
notify = {
	shell = true,         -- start veil-notify with the session
	timeout = 5000,       -- ms for toasts whose app set none; 0 = until dismissed (500..3600000)
	max_visible = 5,      -- toasts on screen at once, 1..10; the rest wait
	dnd_critical = true,  -- critical notifications still show under do-not-disturb
}

-- App binds (mod_key + key). Each one also appears as a clickable tile on the
-- bar, in the order listed. Format: { key, command }; "enter" = Return.
apps = {
	{ "enter", "kitty" },          -- Alt+Enter
	{ "b",     "helium-browser" }, -- Alt+B
	{ "e",     "dolphin" },        -- Alt+E
}

-- Output backend. "auto" (default) picks terminal under a WM/SSH, DRM/KMS on
-- bare TTY. Set "drm" to always go straight for DRM/KMS on `run` (same as
-- VEIL_OUTPUT=drm, but you don't have to re-type it every time) — good for a
-- bare-TTY daily driver with real GPU hardware, where the auto heuristic's
-- env checks can occasionally misfire (e.g. a stale WAYLAND_DISPLAY left in
-- the shell). VEIL_OUTPUT still overrides this if set.
output = "auto" -- auto | drm | terminal

-- Layout keybinds
-- NOTE: In terminal mode (under a WM), use "alt" or "ctrl" — terminals never
--       report the Super/Logo key to an app (the WM eats it first), so
--       mod_key = "super" only works in bare-TTY (DRM/evdev) mode.
keybinds = {
	mod_key = "alt", -- shift | ctrl | alt | super (alt works best in terminal mode)
	focus_left = "h", -- Alt+H
	focus_right = "l", -- Alt+L
	focus_up = "k", -- Alt+K
	focus_down = "j", -- Alt+J
	swap = "s", -- Alt+S (swap with neighbor)
	rotate = "r", -- Alt+R (rotate split axis)
	close = "q", -- Alt+Q (close window)
	fullscreen = "f", -- Alt+F (fullscreen the focused window)
	resize_grow = "=", -- Alt+= (grow primary pane)
	resize_shrink = "-",
	toggle_layout = "w", --Switch between Dwindle and scroll window management, scroll has no window cap.
	reload_config = ";", -- Hot-reload config on demand (Alt+;)
	wallpaper_chooser = "y", -- Wallpaper chooser (Alt+Y): preview grid, click or Enter to set
	volume_up = "[", -- Alt+[
	volume_down = "]", -- Alt+]
	-- volume_mute = "m",
	-- brightness_up = ".",   -- not "]" / "[": those are the volume binds above
	-- brightness_down = ",",
}
-- Help overlay: <mod_key>+/ (e.g. Alt+/ above) always toggles a keybind
-- cheat-sheet — hardcoded, not itself a config entry.
--
-- Workspaces: <mod_key>+1..9 switches workspace (e.g. Alt+1..Alt+9 above),
-- <mod_key>+Shift+1..9 moves the focused window to that workspace and
-- follows it there (Alt+Shift+1..Alt+Shift+9). Both are default binds, not
-- individual `keybinds.*` fields above — there's no natural single-word Lua
-- field name for eighteen keys — but they're still remappable the same way
-- any other key is: add an entry to `keybinds.apps` (or the top-level
-- `apps` table) keyed by the digit or shift-symbol character you want
-- instead, same as overriding any app-launch key. Per-workspace tiling
-- state (focus, split ratio, flip, dwindle/scroll mode) is remembered
-- independently — switching back to a workspace finds it exactly as you
-- left it, not reset.
--
-- NOTE: the move-to-workspace shift-symbol keys (!@#$%^&*() assume a
-- US/QWERTY-ish physical layout, same class of assumption this file's
-- h/l/j/k focus binds already make. A layout that shifts the digit row
-- differently will need the `keybinds.apps` override above to land on the
-- keys you'd expect.
--
-- Shift+Alt+E: graceful quit — fixed, not scaled to mod_key (same reasoning
-- as Ctrl+C below: your escape hatch shouldn't move when you remap the
-- everyday chord). Works in both terminal and DRM/bare-TTY mode; Ctrl+C
-- relies on the tty's line discipline generating SIGINT, which bare-TTY
-- evdev-grab mode may not deliver the same way.

-- Static wallpaper (optional). A path may be a file or a directory of images.
-- wallpaper = "~/Pictures/wall.png"
-- wallpaper = { engine = "static", paths = { "~/Pictures/walls" },
--               interval = 600,      -- seconds between images; 0 = pick once
--               shuffle = true, fit = "fill" }  -- fill | fit | stretch
