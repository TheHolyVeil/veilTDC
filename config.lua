-- veil-host configuration file
-- Place at ./config.lua or ~/.config/veil/config.lua

quality = "auto" -- auto | kitty | pixel | ascii | ascii_edge
fps = 60 -- compositor frame rate cap
cage_timeout_secs = 8 -- timeout for caged output
input = true -- enable input forwarding
gpu_render = true -- use GPU for frame encoding

-- Bare background color, shown wherever no window covers. Otherwise
-- uncovered space is pure black — an actual void, which you can now
-- deliberately land on at zero windows (Alt+D launcher).
background = "#8c8c8c" -- hex, "#RRGGBB" or "RRGGBB" — cement grey default

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
}
-- Help overlay: <mod_key>+/ (e.g. Alt+/ above) always toggles a keybind
-- cheat-sheet — hardcoded, not itself a config entry.
--
-- Shift+Alt+E: graceful quit — fixed, not scaled to mod_key (same reasoning
-- as Ctrl+C below: your escape hatch shouldn't move when you remap the
-- everyday chord). Works in both terminal and DRM/bare-TTY mode; Ctrl+C
-- relies on the tty's line discipline generating SIGINT, which bare-TTY
-- evdev-grab mode may not deliver the same way.
