# TheHolyVeil / veilTDC
<sub>The name "veil" is due to change.</sub>


A modular desktop opt-in ecosystem built around a simple idea:

Veil is not meant to be a bloated “everything and the kitchen sink” desktop environment.
It is a fast, opinionated, productivity first desktop shell designed to give users a strong default environment without forcing them into a pile of extras they do not want.

Veil is designed to be:
- fast
- minimal by default
- configurable
- friendly to opt-in tools
- modular
- easy to extend
- built around recommendations, not clutter
- opt-in 
- embedded
- device-variable

This project is still in active development. The README is a current snapshot of the architecture and direction. May not have up-to-date information. Such as the config and how to edit it. Pray for yourself that the default config.lua has everything in it.

##  Installation

### 1. Veil Desktop Environment

Install `veil-host` using the installer script:

```bash
curl -fsSL https://raw.githubusercontent.com/viewerofall/veilTDC/main/install.sh | bash
```

Or with `wget`:

```bash
wget -qO- https://raw.githubusercontent.com/viewerofall/veilTDC/main/install.sh | bash
```

*Default install path:* `/usr/local/bin/veil-host` (or set `INSTALL_DIR=~/.local/bin` for non-root install).

### 2. Abyss Login Manager (`velogin`)

**Abyss** (package `velogin`) is a lightweight graphical TTY login manager designed for Veil and Void/Linux systems. It provides PAM authentication, session selection, avatar/wallpaper support, and boots straight into Veil on DRM/KMS hardware.

```bash
curl -fsSL https://raw.githubusercontent.com/viewerofall/veilTDC/main/veil-login/dist/install.sh | sudo bash
```

After installation, enable `velogin`:

```bash
sudo systemctl disable getty@tty1.service
sudo systemctl enable velogin.service
sudo reboot
```

### 3. Build from Source

Requirements: Rust stable & Vulkan/DRM development headers (`libseat`, `libpam`, `libgbm`).

```bash
git clone https://github.com/viewerofall/veilTDC.git
cd veilTDC

# Build all workspace crates in release mode
cargo build --release

# Install veil-host binary
sudo install -Dm755 target/release/veil-host /usr/local/bin/veil-host
```

---

##  Usage & Commands

```bash
# Launch a GUI app inside Veil
veil-host run thunar
veil-host run firefox

# Append an app to an already-running Veil instance (works cross-VT via IPC)
veil-host run -a dolphin

# Boot straight to an empty desktop with the launcher open
veil-host start

# Run with specific render mode or debug logging
veil-host run -m halfblock firefox
veil-host run -d thunar                      # Logs to $XDG_STATE_HOME/veil/veil.log

# Gracefully stop the running instance (SIGTERM)
veil-host stop

# Inspect resolved configuration and terminal capabilities
veil-host probe
veil-host list-modes
```

---

## Keybindings Reference

Default modifier key is `Super` (configurable to `Alt`, `Ctrl`, or `Shift` in `config.lua`). Press **`Super+/`** inside Veil anytime to open the live shortcut reference overlay.

| Category | Action | Keybinding |
|---|---|---|
| **Focus** | Move focus left / right / up / down | `Super + h` / `l` / `k` / `j` |
| **Tiling** | Swap focused window with neighbor | `Super + s` |
| | Rotate split axis | `Super + r` |
| | Toggle Layout (Dwindle ↔ Scroll) | `Super + w` |
| | Grow / Shrink primary pane | `Super + =` / `Super + -` |
| | Toggle Fullscreen | `Super + f` |
| | Close focused window | `Super + q` |
| **Workspaces** | Switch to Workspace 1 – 9 | `Super + 1` .. `9` |
| | Move window to Workspace 1 – 9 & Follow | `Super + Shift + 1` .. `9` |
| **DE Tools** | App Launcher (Fuzzy search / Command box) | `Super + d` |
| | Power Menu (Poweroff / Reboot / Logout) | `Super + p` |
| | Live Help Overlay | `Super + /` |
| | Reload Lua Configuration | `Super + c` |
| | Graceful Desktop Exit | `Shift + Alt + E` (or `veil-host stop`) |

---

## Configuration (`config.lua`)

Place your configuration file at `~/.config/veil/config.lua` or `./config.lua`.

```lua
-- Quality / Render mode: auto | pixel | ascii | ascii_edge | kitty
quality = "auto"

-- Target framerate cap
fps = 60

-- Output backend preference: auto | drm | terminal
output = "auto"

-- Enable GPU compute acceleration (wgpu Vulkan)
gpu_render = true

-- gpu power for laptops and such. Low for hybrids.
gpu_power = "high"

-- Desired refresh rate (up to what your monitor supports)
refresh = 60

-- Desktop Color Theme
-- Options: default | nord | dracula | catppuccin | gruvbox | everforest
--          tokyonight | solarized | rosepine | monokai | onedark | deepsage
theme = "deepsage"

-- Background void-fill color (hex string)
background = "#232b26"

-- Desktop Status Bar Configuration
bar = {
  enabled = true,
  position = "bottom" -- "top" | "bottom"
}

-- Keybindings Configuration
keybinds = {
  mod_key = "super", -- "super" | "alt" | "ctrl" | "shift"

  focus_left    = "h",
  focus_right   = "l",
  focus_up      = "k",
  focus_down    = "j",
  swap          = "s",
  rotate        = "r",
  close         = "q",
  toggle_layout = "w",
  fullscreen    = "f",
  reload_config = "c",
  resize_grow   = "=",
  resize_shrink = "-",
}

  -- Direct Application Launchers (<mod_key> + key)
  -- Apps defined here are automatically added to the status bar shortcuts!
  apps = {
    { "b", "firefox" },
    { "t", "kitty" },
  }
  
-- Wallpaper recommend config 
wallpaper = { engine = "static", paths = { "/home/abyss/Pictures/Wallpapers" },
              interval = 600, shuffle = true, fit = "fill" }

```

---



### Supported Wayland Globals

| Global | Support Level |
|---|---|
| `wl_compositor` + `wl_subcompositor` | ✓ Full |
| `xdg_shell` (toplevel + popup) | ✓ Full |
| `wl_seat` (keyboard, pointer, touch) | ✓ Full |
| `wl_shm` | ✓ Full |
| `wl_data_device_manager` + `primary_selection` | ✓ Full (Clipboard & Selection) |
| `wl_output` + `xdg_output` | ✓ Full (Multi-Monitor aware) |
| `zwp_linux_dmabuf_v1` | ✓ Full (GBM / EGL GPU import with SHM fallback) |
| `linux-drm-syncobj-v1` | ✓ Explicit synchronization |
| `xdg_decoration` / `xdg_activation` | ✓ Client-side decoration / instant grant |
| `wp_viewporter` / `wp_fractional_scale` / `wp_presentation_time` | ✓ Full |
| `tablet` / `idle_inhibit` / `cursor_shape` / `pointer_constraints` | ✓ Supported |
| `XWayland` | ✓ Spawned automatically for X11 application support |


---

## The bigger picture: TheHolyVeil

Veil is part of a larger ecosystem rather than a single standalone app.

TheHolyVeil is the umbrella identity for a layered stack:

- boot layer (limine fork)
  - trusted, optimized, and remade boot
  - security and control are the concerns of it
- OS & ISO layer
  - the operating system base
  - minimal by default, extensible by choice
  - reachable & extensible with an installer
- login layer
  - easy to use
  - virtual keyboard friendly
  - aestheticly pleasing & matching
  - login manager and environment bootstrap
- desktop / compositor layer
  - Veil itself
  - Full shebang
  - Configuration decides everything
  - Seperate graphics modes
  - Able to run over ssh
  - Batteries required DE
  - "We provide the foundation, and you build the house on it"
  - the actual user-facing desktop environment

Each layer is separate, optional, and intentionally independent. That is a core part of why veil is veil.

This matters because Veil is not trying to be absolute. It is trying to allow you to **use it without being in your way.**

---

## What Veil is

Veil is a lightweight Rust-based desktop environment focused on:

- bare-TTY and DRM/KMS operation
- fast workflow-oriented desktop control
- workspace management
- app launching
- system status and quick actions
- clean visual styling
- minimal configuration burden
- smart defaults over feature dumping

It is built to feel more like an intentionally designed desktop environment than a generic “do everything” shell.

---

## The philosophy

We believe desktop environments should be helpful, not noisy.

A desktop should:
- make work easier
- keep you productive
- keep the system responsive
- give the user a strong default
- avoid unnecessary bloat
- support customization without forcing complexity
- allow optional tools to be installed without becoming mandatory

Veil follows that principle.

This is the point of the whole direction:

- not “everything included”
- not "everything everywhere all at once"
- not “every package installed by default”
- not “the shell pretending to be a full operating environment”
- not "have a bloated system have fun figuring it out and debloating it"
- but “a solid base environment with a curated path forward”


---

## Batteries are not included

Veil is intentionally designed around a more thoughtful model:

- the base experience is clean and usable
- optional tools can be added by the user
- recommended packages and services can be provided
- the environment remains fast and honest
- users can opt in to whatever ecosystems they actually want
- general things people dont even use anyway are not installed, thats unnecessary bloat.

This is important.

A lot of desktop environments drift into becoming bloated packages that try to be everything:
- file manager
- browser
- package manager
- documentation layer
- launcher ecosystem
- productivity stack
- half-finished utilities
- forced-to-use utilies

Veil does not need to do that.

The preferred model is:

- the shell provides the desktop loop
- the user chooses the tools they want
- recommended apps and utilities are available without becoming mandatory
- configuration remains the place where the environment becomes personal

This keeps the project aligned with the idea of a friendly opt-in environment instead of forced “batteries included” bloat.

---

## Friendly opt-in environment

Veil aims to be a friendly desktop environment for real people:

- simple to configure
- easy to understand
- easy to extend
- not locked into heavy assumptions
- built around ergonomic defaults
- not judgmental about user preference

This means the project supports:

- custom app launches
- user-defined keybindings
- custom themes
- wallpaper configurability
- per-user setup
- optional system services
- recommended but not mandatory tools

The configuration layer is part of the design. It is not an afterthought.

The project exposes a configuration based settings file for:
- theme selection
- bar configuration
- app definitions
- wallpaper and background settings
- keybinding setup
- output preferences
- runtime behavior

This matches the philosophy of the project: a desktop that is direct and configurable, not a black box.

---

## Current feature set

The repository already includes a substantial set of desktop features and system capabilities.

### Core desktop shell
- compositor / Wayland desktop runtime
- window tiling and workspace management
- window focus and movement
- resizing and split rotation
- fullscreen handling
- app launching from the shell

### Tiling and layout
- Dwindle-style tiling
- Scroll-style workspace flow
- layout switching on the fly
- split flips and swapping
- efficient desktop workflow

### Launcher, bar, and desktop chrome
- built-in app launcher
- status bar with workspace indicators and app shortcuts
- power menu
- quick action surfaces
- live help/overlay system

### Theme and wallpaper system
- built-in color themes
- wallpaper loading and chooser support
- background styling
- configurable desktop aesthetics

### Output and rendering
- DRM/KMS output support
- terminal mode output support
- render quality modes
- GPU-aware rendering paths
- direct framebuffer support in bare TTY scenarios

### Login and session flow
- graphical TTY login support
- PAM-style authentication flow
- session selection and session startup
- booting into a proper desktop environment

### Integration
- Wayland support
- XWayland support
- clipboard handling
- environment integration for GUI apps
- desktop environment shaping instead of pure window manager behavior

### Configurability
- hot-reloadable config
- Lua config file support
- per-user desktop customization
- environment tuning without rebuilding the whole system

---

## Roadmap: 

To become a true ecosystem, from the bootloader all the way to the clipboard on your screen.
Bootloader, custom kernel, arch-based operating system, login manager, and semi-desktop environment. 

- Own every stack
- Be open minded
- Accept suggestions & feedback in mind
- "The people" oriented 
- Acceptable

The goal is to be more void oriented. TheHolyVeil would be TheBigVoid, velogin would be singulard, veil-host (veil) would be Infinity(DE), the limine fork would be Ascend, and then AbyssOS would be the actual operating system. 
This aligns with our names and presents veil as a more aligned and ecosystem proven product instead of a loose object in the chaos. 

## Project structure

This repository is a Rust workspace with a modular layout:

```text
veilTDC/
├── Cargo.toml
├── config.lua
├── install.sh
├── pkg.sh
├── render_real_font.py
├── test-braille.sh
├── README.md
├── .cargo/
├── .github/
├── dist/
├── veil-config/
├── veil-host/
├── veil-login/
├── veil-render/
└── ...
