# veil

**Nested Wayland compositor → terminal / bare-TTY renderer.**
Run GUI apps inside your terminal (or straight to a bare TTY framebuffer) —
full colour, live input, clipboard, tiling, resize.

> [!IMPORTANT]
> **AI-Assisted Project:** Developed with AI assistance. Code reviewed but not guaranteed production-ready.

> [!NOTE]
> **The original screencopy-based pipeline (`veil-compositor` / `veil-cli`) is discontinued.**
> It has been archived on the [`legacy/v1`](../../tree/legacy/v1) branch — cage isolation, wlr-screencopy capture, halfblock renderer. All development is now on `veil-host`.

---

## How it works

`veil-host` is a Smithay-based nested Wayland compositor. It:

1. Opens its own `WAYLAND_DISPLAY` socket
2. Spawns the target app(s) pointed at that socket — up to 4 tiled toplevels, dwindle-style
3. Receives SHM buffer commits (shm path; GPU/dmabuf apps fall back to software automatically)
4. Composites surfaces + subsurfaces + popups + cursor + a configurable background into a single RGBA frame
5. Sends that frame to one of two output backends:
   - **Terminal**: Kitty graphics protocol / half-block Unicode / ASCII, over your existing terminal emulator
   - **DRM/KMS**: direct framebuffer on a bare TTY (own VT, `libseat`-managed), for running without any host compositor or terminal at all

Input flows back: keyboard, mouse (click, drag, scroll), and clipboard (both directions) are fully bridged. On bare TTY, `Ctrl+Alt+F1`–`F12` switches VTs normally — veil suspends/resumes DRM output around the switch instead of eating the keys.

---

## Architecture

```
veil/
├── veil-host/      Smithay nested compositor + standalone binary
│   ├── server.rs        Wayland globals, dispatch loop, software compositor, overlays
│   ├── main.rs           CLI, config resolution, output dispatch
│   ├── layout.rs         Dwindle tiling (up to 4 windows) + resize
│   ├── input.rs          InputCmd enum
│   ├── input_backend/    Terminal stdin + evdev (bare-TTY) input sources
│   ├── output/           TerminalOutput (kitty/halfblock/ascii) + DrmOutput (DRM/KMS)
│   ├── launcher.rs        Alt+D app launcher (.desktop scan + raw command exec)
│   ├── lockfile.rs        Single-instance lock, -O override, -a auto-discovery
│   ├── font5x7.rs         Bitmap font for the help/launcher overlays
│   ├── seat.rs / vt.rs    libseat session + VT switch handling
│   └── sink.rs            Frame struct
├── veil-render/    Render engines (kitty, halfblock, ascii, ascii-edge)
└── veil-config/    Lua config loader, keybinds, output/quality/background parsing
```

### Compositor globals

| Global | Status |
|---|---|
| `wl_compositor` + `wl_subcompositor` | ✓ full |
| `xdg_shell` (toplevel + popup) | ✓ full |
| `wl_seat` (keyboard, pointer, touch) | ✓ full |
| `wl_shm` | ✓ full |
| `wl_data_device_manager` (clipboard) | ✓ full |
| `wl_output` + `xdg_output` | ✓ full |
| `xdg_activation` | ✓ stub (always grant) |
| `xdg_decoration` | ✓ client-side always |
| `zwp_linux_dmabuf_v1` | GPU import (GBM/EGL) with shm fallback on failure |
| `wp_viewporter` / `wp_fractional_scale` / `wp_presentation_time` | globals only |
| XWayland | ✓ spawned at startup, X11 apps work |

---

## Install

### Abyss — Login Manager

**Abyss** (package: `velogin`) is a graphical TTY login manager for the Void ecosystem. 
Runs on bare metal with DRM/KMS, handles PAM auth, session selection, and avatars — perfect for booting straight into Veil on a fresh system.

**→ See [`veil-login/README.md`](./veil-login/README.md) for full docs.**

Quick install:

```bash
curl -fsSL https://raw.githubusercontent.com/TheHolyVeil/veilTDC/refs/heads/main/veil-login/dist/install.sh | sudo bash
```

---

### Veil Compositor

Install the main compositor:

```bash
curl -fsSL https://raw.githubusercontent.com/TheHolyVeil/veilTDC/refs/heads/main/install.sh | bash
```

or with wget:

```bash
wget -qO- https://raw.githubusercontent.com/TheHolyVeil/veilTDC/refs/heads/main/install.sh | bash
```

Installs to `/usr/local/bin/veil-host` (uses `sudo` if needed). Supports `x86_64` and `aarch64`.

Options:

```bash
# Install to ~/.local/bin (no sudo)
INSTALL_DIR=~/.local/bin curl -fsSL .../install.sh | bash

# Pin a specific version
VERSION=v0.1.0 curl -fsSL .../install.sh | bash
```

### Velogin

Install the login manager directly from the repo script:

```bash
curl -fsSL https://raw.githubusercontent.com/viewerofall/veilTDC/main/veil-login/dist/install.sh | sudo bash
```

or with wget:

```bash
wget -qO- https://raw.githubusercontent.com/viewerofall/veilTDC/main/veil-login/dist/install.sh | sudo bash
```

By default this downloads `velogin.tar.gz` from the latest GitHub release, installs:

- `/usr/local/bin/velogin`
- `/etc/pam.d/velogin`
- `/etc/systemd/system/velogin.service`

You can pin a release tag:

```bash
VERSION=v1.2.0 curl -fsSL https://raw.githubusercontent.com/viewerofall/veilTDC/main/veil-login/dist/install.sh | sudo bash
```

After install:

```bash
sudo systemctl enable --now seatd.service
sudo usermod -aG <username>
sudo systemctl disable getty@tty1.service
sudo systemctl enable velogin.service
sudo reboot (You have to do this or it will NOT work and you cant login)

```

### From source

Requirements: Rust stable. Runs nested under any Wayland/X11 compositor (Niri, Hyprland, etc.), or standalone on a bare TTY with a real GPU (DRM/KMS + `libseat`).

```bash
git clone https://github.com/viewerofall/veilTDC.git
cd veilTDC

# Build
cargo build --release -p veil-host   # → target/release/veil-host

# Install system-wide
sudo install -Dm755 target/release/veil-host /usr/local/bin/veil-host

# Install to ~/.local (no sudo)
install -Dm755 target/release/veil-host ~/.local/bin/veil-host
```

---

## Usage

```bash
# Run any GUI app
veil-host run thunar
veil-host run firefox
veil-host run weston-terminal

# Add a second app to the running instance (tiles alongside the first, up to 4)
veil-host run -a dolphin

# Flags
veil-host run -d thunar                  # debug log → $XDG_STATE_HOME/veil/veil.log
veil-host run -m halfblock thunar        # force render mode
veil-host run -m ascii-edge thunar       # edge-detection ascii
veil-host run -w 1920 -h 1080 thunar     # explicit compositor resolution
veil-host run -s wayland-veil-1 thunar   # custom socket name
veil-host run -O -s wayland-veil-2 xterm # second, unregistered instance (bypasses the lock)

# Stop the running default instance from anywhere (graceful, SIGTERM)
veil-host stop

# Probe terminal capabilities and resolved config
veil-host probe
veil-host list-modes
```

Press `Ctrl+C`, hit `Shift+Alt+E` in the compositor, or run `veil-host stop` from another terminal/VT — all three shut down cleanly and wipe the socket and lock file. `Shift+Alt+E` is the one guaranteed to work in DRM/bare-TTY mode: it's a compositor keybind, not a terminal signal, so it doesn't depend on the tty's line discipline generating SIGINT (which bare-TTY evdev-grab mode may not do).

### Single instance / lock file

Running `veil-host run` a second time on the default socket refuses to start and tells you an instance is already running (PID + socket, read from the lock file). Options:

- `-a` / `--append` — don't start a new compositor, launch the app into the already-running one instead. With no `-s`, it auto-discovers the socket name from the lock file, so it works across VTs without you tracking the socket name yourself.
- `-O` / `--override` — bypass the lock and start a second, independent instance. It's unregistered (no lock file), so it needs its own `-s` to bind (the default socket's taken), and `-a` auto-discovery can't find it — reach it with an explicit `-s` on both ends.

### Multi-app tiling

Up to 4 toplevels tile in a dwindle layout automatically as you add/close them:

```
1 → fullscreen
2 → left / right split
3 → left half + right column split in two
4 → 2×2 grid
```

A 5th window stacks on the last cell. Click a tile to focus it.

### Keybinds

Set in `config.lua`'s `keybinds` table (see below) — held modifier + a single key. A hardcoded help overlay lists the live config: `<mod_key>+/`.

| Action | Default key |
|---|---|
| Focus left/right/up/down | `h` / `l` / `k` / `j` |
| Swap with neighbor | `s` |
| Rotate split axis | `r` |
| Close focused window | `q` |
| Grow / shrink primary pane | `=` / `-` |
| Help overlay | `/` |
| App launcher | `d` |
| Quit (graceful) | `Shift+Alt+E`, fixed — or `Ctrl+C` / `veil-host stop` from outside |

> In terminal mode (nested under a WM), use `mod_key = "alt"` or `"ctrl"` — terminals never forward the Super/Logo key, the WM eats it first. `"super"` only works in bare-TTY (DRM/evdev) mode.

### App launcher

`<mod_key>+d` opens a panel listing installed `.desktop` entries (scanned from standard XDG app dirs) plus a raw command box — type and run anything. Meant to get you out of a zero-window state without touching a second terminal.

### VT switching (bare TTY / DRM mode)

`Ctrl+Alt+F1`–`F12` switches virtual terminals like normal — veil-host intercepts it, suspends DRM output, hands off via `libseat`, and resumes cleanly when you switch back.

---

## Render modes

| Mode | Flag | Terminal requirement | Notes |
|---|---|---|---|
| **Kitty** | `kitty` | Kitty graphics protocol | Best quality, native pixel output |
| **Halfblock** | `halfblock` | 24-bit truecolor | `▀` Unicode, 2× effective vertical resolution |
| **ASCII** | `ascii` | Any | Luma → ASCII character map |
| **ASCII Edge** | `ascii-edge` | Any | Luma with hysteresis edge-detection for sharper output |

Without `-m`, the mode is resolved automatically:

1. Check `quality` in `config.lua`
2. If `auto`, detect from `$TERM` / `$COLORTERM`:
   - `xterm-kitty` or WezTerm → Kitty
   - `truecolor` / `24bit` → Halfblock
   - Otherwise → ASCII

## Output backend

Independent of render mode — decides *where* frames go, not how they're drawn:

- **Terminal** — draws into your existing terminal emulator via the render mode above.
- **DRM/KMS** — direct framebuffer on a bare TTY, no terminal involved at all. Needs `libseat` and `/dev/dri` access.

Resolution order: `VEIL_OUTPUT` env var (`drm`/`terminal`, always wins, errors loudly on failure) → `output` in `config.lua` (`"drm"`/`"terminal"`, persistent equivalent, but never overrides the nested-compositor check) → auto-detect (nested Wayland/X11 env → terminal; SSH session → terminal; otherwise try DRM/KMS, fall back to terminal on failure).

---

## config.lua

Place at `./config.lua` or `~/.config/veil/config.lua`.

```lua
quality           = "auto"   -- auto | kitty | pixel | ascii | ascii_edge
fps               = 60       -- compositor frame rate cap
cage_timeout_secs = 8        -- timeout for caged output
input             = true     -- enable input forwarding
gpu_render        = true     -- use GPU for frame encoding

background = "#8c8c8c"       -- hex "#RRGGBB" or "RRGGBB" — shown wherever no window covers

output = "auto"              -- auto | drm | terminal

keybinds = {
  mod_key       = "alt",     -- shift | ctrl | alt | super
  focus_left    = "h",
  focus_right   = "l",
  focus_up      = "k",
  focus_down    = "j",
  swap          = "s",
  rotate        = "r",
  close         = "q",
  resize_grow   = "=",
  resize_shrink = "-",
}
-- Help overlay (<mod_key>+/) and app launcher (<mod_key>+d) are hardcoded,
-- not themselves config entries.
```

`veil-host probe` shows which config file was loaded and the resolved mode.

---

## Clipboard

Full bidirectional bridge:

- **App → host**: when the hosted app copies text, it appears in your host clipboard
- **Host → app**: host clipboard is polled every second and offered to the hosted app when it has no active selection

---

## Running browsers (and other keyring apps) under veil

> [!NOTE]
> **A browser opened under veil can look like its profile was wiped — logged out
> everywhere, saved passwords gone. Your data is safe.** It's the same profile
> directory on the same disk; nothing is deleted or moved.
>
> Chromium/Firefox encrypt cookies and passwords with a key held in your
> **keyring** (`gnome-keyring` / `org.freedesktop.secrets`), which your display
> manager unlocks via PAM at login. A session started *outside* that login path —
> e.g. a bare compositor (`niri`, `Hyprland`) hosted under veil — never gets that
> key handed to it, so the browser can't decrypt your existing logins and shows a
> fresh-looking session. Bookmarks and history (unencrypted) are still there.
>
> **It returns to normal in your real login session** — the key is available
> again and the untouched files decrypt as usual. To avoid churn, close the
> browser before leaving veil and don't bother re-signing-in inside it.
>
> If you use **profile-sync-daemon (psd)**, be extra careful: psd mirrors a
> profile to tmpfs and syncs it back on exit, so a "reset"-looking session could
> be written over your good backup. Close psd-managed browsers before running
> them under a nested session.

## Limitations

- **Cell resolution mouse** — under terminal output, mouse events are cell-granularity, not pixel-granularity (DRM/KMS output is full pixel resolution).
- **4-window tiling cap** — a 5th+ window stacks on the last cell rather than adding a new tile.

---

## What's next

- Sixel / iTerm2 protocol render modes
- AT-SPI text overlay accessibility pass
- `veil-cli` migration to consume veil-host

---

**veil** — your apps, your terminal.
