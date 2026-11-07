# veil-power

Supervised fullscreen power menu overlay for Veil, lifted and adapted from `woven-power`.

## What it is

`veil-power` provides the desktop power menu overlay for Veil. It runs as an on-demand child process supervised by the compositor via `veil shell power`.

When invoked (typically via `<mod>+P` or `veil ctl toggle power`), it maps a fullscreen overlay surface with `KeyboardInteractivity::Exclusive` and dims the background. If the compositor advertises `ext-background-effect-v1`, it requests background blur behind the overlay.

## Features

- **6 Power Actions**: Lock, Logout, Suspend, Hibernate, Reboot, Shutdown.
- **Safety Confirmations**: Destructive actions (Reboot, Shutdown, Hibernate) require confirmation.
- **Keyboard & Mouse Control**:
  - `Escape`: Cancel dialog / close menu.
  - `Enter`: Select action or confirm.
  - `←` / `→` or `h` / `l`: Move between actions.
  - `↑` / `↓` or `k` / `j`: Move across rows.
  - `1` - `6`: Quick selection.
  - Click & hover: Pointer interaction with highlighted click zones.
- **Dynamic Theming**: Adapts to the compositor wallpaper palette (`veil-palette`) and system fonts (`veil-text`).
- **Compositor IPC**: Dispatches actions (`session.lock`, `session.logout`, `session.suspend`, `session.reboot`, `session.poweroff`) over the Veil IPC socket.

## Testing

Run unit tests:
```sh
cargo test -p veil-power
```
