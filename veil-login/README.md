# Abyss (Velogin)

**Graphical TTY Login Manager for the Void Ecosystem**

Abyss is a modern, minimal login manager built with Slint UI that runs on bare TTY with DRM/KMS support. Handle PAM authentication, select sessions, and boot into your environment—all without X11 or Wayland overhead.

## Features

- **Graphical UI on TTY** — Runs on bare metal (DRM/KMS), no parent compositor needed
- **PAM Authentication** — Full two-stage PAM flow, keyring support, account lockout respect
- **Session Selection** — Choose between VoidWM (Veil), shell, or custom sessions
- **Avatar Support** — Load user avatars from system (fallback to placeholder)
- **Wallpaper Background** — Custom background image at `/etc/abyss/background.png`
- **Login Cooldown** — Progressive backoff after failed attempts (security)
- **Virtual Keyboard** — Onscreen keyboard support for touch/accessibility
- **GPU Rendering** — Slint with GPU via EGL/GBM, fallback to software
- **Lightweight** — ~50MB memory idle, minimal dependencies

## Install

See main Void/Veil README for installation instructions.

## Configuration

### Background Image

Place your background image at `/etc/velogin/background.png` (or `/etc/abyss/background.png` legacy path):

```bash
sudo mkdir -p /etc/velogin
sudo cp your-background.png /etc/velogin/background.png
```

Supported formats: PNG, JPEG. Recommended: 1920×1080 or higher. Note: Existing wallpapers in `/etc/abyss/background.png` are automatically detected via fallback—no file renaming is required.

### Custom Sessions

Sessions are detected from:
- `/usr/share/xsessions/` — Desktop files (standard XDG format)
- Hardcoded defaults: VoidWM (Veil), Bash shell

To add a custom session, create `/usr/share/xsessions/mysession.desktop`:

```ini
[Desktop Entry]
Name=My Custom Session
Exec=/usr/bin/my-session-starter
Type=Application
```

Abyss will detect and list it on login.

### PAM Configuration

Abyss uses the system `login` service via PAM. Configuration is standard:

```bash
cat /etc/pam.d/login
```

Common settings:
- Password policies (length, complexity) via `pam_cracklib` or `pam_pwquality`
- Account lockout via `pam_faillock`
- Keyring integration via `pam_gnome_keyring` or `pam_kwallet`

Abyss respects all PAM decisions (locked accounts, expired passwords, etc.).

## Usage

### Boot to Abyss

Configure your system to launch Abyss on TTY1:

```bash
# Disable getty on tty1
sudo systemctl disable getty@tty1.service

# Enable seatd (session management)
sudo systemctl enable --now seatd.service

# Install Abyss service
sudo systemctl enable abyss.service
sudo systemctl start abyss.service
```

On next boot, you'll see Abyss instead of a text login prompt.

### Manual Launch

```bash
abyss
```

Launches Abyss on the current TTY.

### Login Flow

1. **Username prompt** — Type your username
2. **Password prompt** — Type password (hidden input)
3. **Session selection** — Arrow keys to choose, Enter to select
4. **Launch** — Environment loads, session starts
5. **Logout** — Exit your session, loop back to login

## Architecture

```
Abyss (velogin binary)
  ├─ Slint UI framework (graphical rendering)
  ├─ DRM/KMS backend (direct framebuffer on TTY)
  ├─ libseat session manager (VT switching, seat access)
  ├─ pam-client2 (PAM authentication)
  ├─ fontdue (font rendering, glyph atlas)
  └─ uzers (user info lookup, home dirs)

Flow:
  TTY Input → Slint UI → PAM auth → Session spawn → exec
```

### Why Slint?

- **No dependencies** — Minimal, self-contained
- **GPU + software rendering** — Flexible hardware support
- **Fast** — Compiled, not interpreted
- **Responsive** — Real UI, not bash scripts

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| Tab | Next field |
| Shift+Tab | Previous field |
| Up/Down | Navigate session list |
| Enter | Confirm (username → password → launch) |
| Esc | Clear current field, go back |
| Home | Emergency exit: quits the greeter without starting a session (systemd then restarts it) |

## Troubleshooting

### Black screen after login

**Cause:** Session binary not found or crashed.

**Fix:**
1. Ctrl+C to return to Abyss
2. Check if session path exists: `which veil-host`
3. Verify permissions: `ls -la /usr/bin/veil-host`
4. Try Bash shell session instead

### "Authentication failed" loop

**Cause:** Wrong password, or PAM misconfiguration.

**Fix:**
1. Verify username exists: `getent passwd yourusername`
2. Reset password: `sudo passwd yourusername`
3. Check PAM config: `sudo cat /etc/pam.d/login`
4. If using LDAP/NIS, verify those services are running

### No background image showing

**Cause:** `/etc/abyss/background.png` missing or invalid.

**Fix:**
1. Verify file exists: `ls -la /etc/abyss/background.png`
2. Check permissions: `sudo chmod 644 /etc/abyss/background.png`
3. Verify format: `file /etc/abyss/background.png`
4. If missing, Abyss will use solid background (fallback works fine)

### GPU rendering not working, falling back to software

**Cause:** EGL/GBM libraries missing or GPU driver not loaded.

**Fix:**
1. Verify GPU driver: `lspci | grep -i vga`
2. Install DRM libraries: `sudo pacman -S libdrm mesa`
3. Check `/var/log/syslog` for GPU errors
4. Software rendering works fine (slower, but stable)

### Virtual keyboard not appearing

**Cause:** Touch input not detected, or Slint compiled without keyboard feature.

**Fix:**
1. If no touchscreen, keyboard is hidden (normal)
2. If touchscreen present, check: `cat /proc/bus/input/devices`
3. Ensure libinput is installed: `pacman -S libinput`

## Building from Source

Requires Rust 1.70+:

```bash
git clone https://github.com/viewerofall/veil.git
cd veil/veil-login

cargo build --release

sudo install -m755 target/release/velogin /usr/bin/abyss
sudo install -m755 dist/velogin.service /etc/systemd/system/abyss.service
sudo install -m755 dist/pam.d/velogin /etc/pam.d/abyss
```

## Dependencies

- **libpam** — PAM authentication
- **libseat** — Session/seat management
- **libdrm** — DRM framebuffer access
- **mesa / libgbm** — GPU rendering (optional)
- **fontdue** — Font rendering
- **Slint** — UI framework (vendored, no extra dep)

## Security Considerations

- **Password validation** — Delegated to PAM (respects system policies)
- **Account lockout** — PAM `pam_faillock` handles backoff
- **Keyring access** — Two-stage auth avoids keyring daemon fork (security best practice)
- **Session isolation** — Each user session runs with correct UID/GID
- **No password echoing** — Input is hidden, never logged

## License

Same as Veil/Void ecosystem (check main repo).

## Contributing

Issues & PRs welcome: https://github.com/viewerofall/veil/issues

---

**Abyss** — The gateway to Void. 🌌
