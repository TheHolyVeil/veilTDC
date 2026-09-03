# veil (v3)

**The Lightweight Terminal & Bare-TTY Wayland Desktop Environment.**

> *"We provide the essentials, you get the packages you need."*

`veil` v3 is a full-featured, lightweight Wayland Desktop Environment (DE) built in Rust. Powered by Smithay and Vulkan compute shaders, `veil` can run nested inside your terminal emulator (with pixel-perfect Kitty graphics, truecolor half-blocks, or ASCII) or standalone directly on a bare TTY framebuffer via DRM/KMS.

---

## 🌟 What's New in v3?

`veil` has evolved beyond a simple nested window compositor—it is practically a complete **Desktop Environment (DE)** with an array of onboard tools, modal interfaces, and ecosystem utilities.

### 🧰 Onboard DE Tools & Capabilities

* 🖥️ **Dual Tiling Engine**:
  * **Dwindle Tiling**: Recursive binary splitting layout (1–4 window grid with stacking fallback).
  * **Scroll Layout**: PaperWM / Niri-style horizontal scrolling strip with fixed-width columns for infinite open windows.
  * **Layout Hot-Swapping**: Switch layout modes on the fly (`Super+W`), rotate split axes (`Super+R`), swap window positions (`Super+S`), or toggle fullscreen (`Super+F`).
* 🗂️ **Multi-Workspace System**:
  * 9 independent workspaces (`Super+1..9` to switch, `Super+Shift+1..9` to move windows and follow).
  * Workspaces are independently scoped per-monitor in multi-display setups.
* 🚀 **Built-in App Launcher**:
  * Integrated modal application launcher (`Super+D`).
  * Scans system `.desktop` files (XDG compliant) with real-time fuzzy filtering.
  * Features a raw command execution fallback (e.g., launch `firefox`, `thunar`, or custom scripts directly).
  * Boots directly into an empty desktop ready to launch apps (`veil-host start`).
* 📊 **Desktop Status Bar**:
  * Built-in customizable desktop status bar (`top` or `bottom` position).
  * Displays active workspace indicators, live clock, and dynamic app launch tiles auto-generated from your keybindings.
* ⚡ **Modal Power Menu**:
  * Integrated system power menu (`Super+P`) with instant **Power Off**, **Reboot**, and **Logout** actions.
* 🎨 **12-Theme Color Engine**:
  * Built-in color schemes for compositor chrome, launcher, bar, and overlays: `default`, `nord`, `dracula`, `catppuccin` (mocha), `gruvbox`, `everforest`, `tokyonight`, `solarized`, `rosepine`, `monokai`, `onedark`, and `deepsage` (hand-tuned dark sage).
  * Hex background void-fill customizer (`background = "#RRGGBB"`).
* 💡 **Live Help & OSD Overlay**:
  * Instant keyboard shortcut reference overlay (`Super+/`).
  * On-Screen Display (OSD) notification engine with titles, progress bars, and auto-expirations.
* 🎮 **GPU Acceleration & Detiling**:
  * **Vulkan Compute Pipeline (`veil-gpu`)**: Offloads half-block and luma encoding to GPU compute shaders via `wgpu`. Automatically bypasses CPU-bound software rasterizers (like `llvmpipe`).
  * **EGL dmabuf Detiling (`detile`)**: Native EGLImage importer for tiled/compressed GPU buffers (AMD DCC / implicit modifiers) from heavy GPU apps/compositors.
  * **mimalloc Allocator**: Zero-fragmentation global memory allocation preventing glibc `mmap` RSS bloat.
* 🪟 **XWayland & Native Wayland Integration**:
  * Embedded XWayland server for full X11 application support alongside Wayland.
  * Automatic Wayland environment variable propagation (`ELECTRON_OZONE_PLATFORM_HINT`, `MOZ_ENABLE_WAYLAND`, `QT_QPA_PLATFORM`, `GDK_BACKEND`, `SDL_VIDEODRIVER`, `XDG_CURRENT_DESKTOP=veil`).
* 📋 **Bidirectional Clipboard**:
  * Continuous, seamless clipboard synchronization between hosted Wayland/X11 apps and the host environment.
* 🖥️ **Bare-TTY & DRM Output**:
  * Runs standalone on bare console TTYs (`/dev/tty1`–`6`) with native DRM/KMS and `libseat` session management.
  * Native VT switching (`Ctrl+Alt+F1`–`F12`) with GPU state suspend/resume without swallowing keys.

---

## 🏗️ Architecture & Ecosystem

```
veil/
├── veil-host/      Smithay-based DE daemon & compositor (server, layout, launcher, bar, powermenu)
├── veil-config/    Lua config engine, 12 color themes, workspace rules, keybind definitions
├── veil-gpu/       WGPU Vulkan compute shader pipeline for fast frame encoding
├── veil-render/    Terminal rendering engines (Kitty graphics, Halfblock, ASCII, ASCII-Edge)
└── veil-login/     Abyss / Velogin: TTY-native graphical login manager (PAM auth, Slint UI)
```

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

## 📦 Installation

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

After installation, enable seat management and `velogin`:

```bash
sudo systemctl enable --now seatd.service
sudo usermod -aG seat $USER
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

## 🚀 Usage & Commands

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

## ⌨️ Keybindings Reference

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

## ⚙️ Configuration (`config.lua`)

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

  -- Direct Application Launchers (<mod_key> + key)
  -- Apps defined here are automatically added to the status bar shortcuts!
  apps = {
    b = "firefox",
    t = "kitty",
    f = "thunar",
  }
}
```

---

## 🖥️ Render Modes & Output Backends

### Render Modes

| Mode | Identifier | Requirements | Notes |
|---|---|---|---|
| **Kitty** | `kitty` | Kitty Graphics Protocol | Native pixel resolution inside supported terminals (Kitty, WezTerm). |
| **Halfblock** | `pixel` / `halfblock` | 24-bit Truecolor | Uses `▀` Unicode characters for 2× vertical pixel density. |
| **ASCII Edge** | `ascii_edge` | Any Terminal | Rec.601 luma calculation with edge-detection overlay. |
| **ASCII** | `ascii` | Any Terminal | Pure ASCII luma character mapping for universal TTY compatibility. |

### Output Backends

1. **DRM/KMS Backend**: Direct hardware framebuffer output on bare TTYs (`/dev/dri/card*`), managed via `libseat`.
2. **Terminal Backend**: Draws inline inside an existing terminal emulator window.

---

## 🔑 Browser & Keyring Note

When running browsers like Firefox or Chromium inside a nested session or custom TTY compositor, safe storage keys managed by PAM (`gnome-keyring` / `kwallet`) may not be automatically passed if the login session wasn't initialized through a standard display manager.

Your profile data remains completely intact on disk. To log into keyrings seamlessly on bare metal, use **Abyss (`velogin`)** as your system login manager.

---

## 📜 License & Philosophy

`veil` is licensed under the **MIT License**.

**Design Principles:**
* **Lightweight & Fast**: Zero bloat, low footprint, high performance.
* **Full-Featured DE**: Built-in essentials so you don't need a dozen third-party daemons to get a functional desktop.
* **Run Anywhere**: TTY-native bare metal or nested in any terminal emulator.
* **Modular**: Clean Rust workspace crates (`veil-host`, `veil-config`, `veil-gpu`, `veil-render`, `veil-login`).

---

**veil v3** — *Your apps, your terminal, your desktop.*
