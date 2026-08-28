-- veil-host configuration file
-- Place at ./config.lua or ~/.config/veil/config.lua

quality = "auto" -- auto | kitty | pixel | ascii | ascii_edge
fps = 60 -- compositor frame rate cap
cage_timeout_secs = 8 -- timeout for caged output
input = true -- enable input forwarding
gpu_render = true -- use GPU for frame encoding

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
--
theme = "default" -- default | nord | dracula | catppuccin | gruvbox | everforest
                   -- | tokyonight | solarized | rosepine | monokai | onedark | deepsage

-- Bare background color, shown wherever no window covers. Otherwise
-- uncovered space is pure black — an actual void, which you can now
-- deliberately land on at zero windows (Alt+D launcher).
--
-- This OVERRIDES the theme's own background when set — comment this line
-- out entirely to let `theme` above pick the background for you instead.
background = "#8c8c8c" -- hex, "#RRGGBB" or "RRGGBB" — cement grey default

-- Status bar: workspace widget + clock + app shortcuts. Fixed three-column
-- layout (workspace widget 16 chars, clock 8 chars, shortcuts fill the
-- rest), 12px tall, rendered with the same built-in 5x7 bitmap font as the
-- help/launcher overlays — no new font engine. Themed by `theme` above
-- (panel_bg for the strip, text/text_dim/accent for the columns).
--
-- App labels render UPPERCASE regardless of case here — the bitmap font is
-- caps-only, same as the help overlay's keybind list.
bar = {
	enabled = true,   -- false hides the bar entirely and gives tiled windows
	                   -- back the full output height
	position = "bottom", -- top | bottom

	-- Click any tile to launch it. This is separate from `keybinds.apps` /
	-- top-level `apps` above — that table gives a shortcut a KEYBIND, this
	-- one puts it on the BAR. Set both (matching name + exec) if you want
	-- an app both clickable and keybound, e.g. `apps = { f = "firefox" }`
	-- above alongside the firefox entry below for Alt+F too.
	apps = {
		{ name = "firefox",  exec = "firefox" },
		{ name = "foot",     exec = "foot" },
		{ name = "nautilus", exec = "nautilus" },
		{ name = "code",     exec = "code" },
	},
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
	resize_grow = "=", -- Alt+= (grow primary pane)
	resize_shrink = "-",
	toggle_layout = "w", --Switch between Dwindle and scroll window management, scroll has no window cap.
	reload_config = ";", -- Hot-reload config on demand (Alt+;)
	volume_up = "[", -- Alt+[
	volume_down = "]", -- Alt+]
	-- volume_mute = "m",
	-- brightness_up = "]",
	-- brightness_down = "[",
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
