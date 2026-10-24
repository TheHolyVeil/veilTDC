# veil-bar

Supervised desktop bar for the Veil window manager.

## What it is
`veil-bar` is an isolated child process supervised by the Veil master compositor. It draws through `wlr-layer-shell`, anchors to the top of the monitor, claims an exclusive zone of 36 logical pixels, and subscribes to `veil-ipc` events for workspaces, active window titles, volume, and palette updates.

## Widgets
- **Left**: `workspaces` (numbered pills with active/occupied states), `window_title` (ellipsized focused window title).
- **Center**: `clock` (localized date and time).
- **Right**: `cpu` (load percentage), `memory` (used RAM percentage), `battery` (percentage and charging state), `audio` (volume / mute).

## Public Contract
- **Wayland Protocol**: `wlr-layer-shell-v1` (Layer: `Top`, Anchor: `Top | Left | Right`, Exclusive Zone: 36px).
- **IPC Topics**: Subscribes to `workspaces`, `focus`, `volume`, `palette`.
- **Supervision**: Started by `veil shell bar`. Sends `ClientMessage::Hello` on launch and answers periodic ping messages.

## Invariants
1. **Crash isolation**: A crash in `veil-bar` never affects the compositor or user windows. The supervisor catches the crash and restarts it with exponential backoff.
2. **Process boundary**: Operates solely over Wayland SHM buffers and the Veil IPC Unix socket.

## How to test
```bash
cargo test -p veil-bar
```
