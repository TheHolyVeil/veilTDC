# veil-osd

The on-screen display (`veil shell osd`): a small pill at the bottom of one
output for volume and brightness levels, config errors and component-failed
notices. Persistent, supervised, click-through, no keyboard.

## Contract

- Subscribes to `osd`, `palette` and `config` on the Veil IPC. Nothing else.
- `osd` payload is `veil_ipc::osd::OsdEvent` (`kind`, `title`, `body`,
  `progress`, `timeout_ms`, `output`). Additive-only within protocol v1.
- It never reads PipeWire or sysfs. The compositor's media worker does, and its
  result arrives as the event.
- The layer surface exists only while something is showing; idle cost is the
  3 s heartbeat.

## Invariants

- While no `osd` subscriber is connected (starting, restarting, crashed,
  `osd.shell = false`) the compositor draws its built-in OSD, so a key press is
  never silent. `State::show_osd` in veil-host is the only place that decides.
- Received text is cleaned (control characters, bidi overrides, length) before
  it is drawn.
- A new event while the pill is up replaces its content and deadline without
  fading in again.

## Layout

| File | What |
| --- | --- |
| `model.rs` | what shows and when; fade maths; takes `now`, no clock |
| `render.rs` | the pill, pure (pixmap in, bytes out) |
| `client.rs` | non-blocking IPC client and heartbeat |
| `lib.rs` | poll loop, surface lifecycle, palette and font updates |

## Testing

`cargo test -p veil-osd`. By hand: run a compositor, then
`veil ctl volume.step '{"delta":5}'`; `kill -9` the `veil shell osd` process and
press a volume key (the built-in OSD must show), then again after the restart.
