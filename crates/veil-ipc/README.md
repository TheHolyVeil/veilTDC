# veil-ipc

Protocol types, newline-delimited JSON codec, and client/server message schemas for the Veil desktop shell architecture.

## What it is
`veil-ipc` defines the communication protocol between the Veil master compositor process and supervised child components (`bar`, `notify`, `osd`, `launch`, `settings`, etc.), as well as external tools like `veil ctl`.

## Public Contract
- **Transport**: Unix domain stream socket located at `$XDG_RUNTIME_DIR/veil/ipc.sock` (or `/tmp/veil-$UID/ipc.sock`).
- **Framing**: UTF-8 newline-delimited (`\n`) JSON.
- **Message Types**:
  - `ClientMessage::Hello { version, name }`
  - `ClientMessage::Subscribe { topics }`
  - `ClientMessage::Cmd { id, cmd, args }`
  - `ClientMessage::Ping`
  - `ServerMessage::Hello { version, palette, config }`
  - `ServerMessage::Event { topic, data }`
  - `ServerMessage::Reply { id, ok, error, data }`
  - `ServerMessage::Pong`
- **Constants**:
  - `PROTOCOL_VERSION = 1`
  - `MAX_MESSAGE_SIZE = 65536` (64 KiB)
  - `MAX_SUBSCRIBER_QUEUE = 1048576` (1 MiB)

## Notification contract (phase 4, additive within v1)
- Event `notifications` `{count, dnd}`: the bell. Published by `notify` via the
  command `notifications.set`, rebroadcast by the master, and **replayed to any
  client that subscribes later** so a restarted bar is never stale.
- Event `notify.control` `{dnd: "on"|"off"|"toggle"}` or `{panel: "open"|"close"|"toggle"}`:
  master to `notify` only. Emitted after the commands `notify.dnd` / `notify.panel`
  (no arguments means toggle, so `veil ctl notify.dnd` works as a keybind).
  They fail with "notify is not running" when nothing is subscribed.
- Typed payloads live in `veil_ipc::notify`; the wire names are pinned by a test.
- `component.toggle notify` is refused on purpose: notify owns the D-Bus name.

## Invariants
1. **Zero dependencies on compositor**: `veil-ipc` depends only on `serde`, `serde_json`, `thiserror`, and `libc`. Never on `veil-host` or Wayland crates.
2. **Hard size limit**: Messages exceeding 64 KiB return `IpcError::MessageTooLarge` and are rejected before transmission or processing.
3. **Additive versioning**: Changes within `PROTOCOL_VERSION = 1` must be strictly backward-compatible.

## How to test
```bash
cargo test -p veil-ipc
```
