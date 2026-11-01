# veil-notify

The Veil notification service (phase 4 of the Veil Shell Architecture).
Scope and decisions live in `Veil_Shell.md`, section 11, Phase 4.

## State: step 2 of 7 (pure logic: data, timing, geometry)

| Step | What | State |
| --- | --- | --- |
| 1 | `text`, `markup`, `model`, `store`, `rate` | done |
| 2 | `timers` (expiry state machine), `layout` (toast stack geometry) | done, 58 tests total |
| 3 | `dbus` (zbus 6 service, signals, name queueing) | next |
| 4 | IPC wiring, host relay, bell fix in `veil-bar` | |
| 5 | toasts | |
| 6 | history panel | |
| 7 | supervisor and `veil shell notify` registration | |

No D-Bus, no Wayland, no threads yet: everything here is deterministic and
the clock is always an argument.

## Invariants (do not break)

- **Body markup** is canonical: only `<b> <i> <u>`, always balanced, text
  escaped. `markup::sanitize` is the only way a body gets in, and
  `parse(render(x)) == x`. Text is decoded once and never re-parsed, so
  `&lt;b&gt;` can never become bold.
- **Summaries are plain text**, one line. Markup in a summary shows as typed.
- **Banned characters** (controls, bidi overrides) never reach a stored string.
- **Action keys are never truncated**, because they are echoed back to the app
  in `ActionInvoked`. A key that is too long is dropped with its action.
- **Ids** are never 0 and never reused across a restart: the counter is in the
  history file and is lifted above the highest stored id on load.
- **History file**: `$XDG_RUNTIME_DIR/veil/notify-history.json`, 0600, directory
  0700, written by temp file plus rename. Loading never fails: a missing,
  foreign-owned, oversized, corrupt or wrong-version file is an empty store.
  Images are not persisted. Restored entries are history only, never re-toasted.
- **Rate limiter table is bounded** (256 apps, then one shared bucket).
- **Timers**: only the newest `max_visible` toasts count down; hidden ones are
  paused, never expired unseen. Hover pauses all of them. Critical and timeout 0
  never expire. A late `poll` replays expiries in order and charges a revealed
  toast only for the time it was actually visible. Mutators queue expiries, and
  `poll` is the only place they come out.
- **Layout**: the toast surface is sized to the stack and its input region is
  the toast rectangles only (`Stack::input_rects`); gaps and everything else
  click through. The first toast is always shown, clamped to the screen.
- **Untrusted image headers** are validated before any index is computed; all
  size arithmetic is checked.

## Defaults (proposals, become `notify.*` config keys in step 4)

summary 256 B, body 4096 B, image long edge 128 px, 4 buttons, history 100,
rate limit 20 per 10 s per app, default timeout 5 s (clamped 0.5 s to 1 h),
5 visible toasts, toast width 360 px, gap 8 px.

## Known limits

- `x<y and z>w` reads as a tag and loses `<y and z>`. Ambiguous input; the
  alternative is rendering real tags as text, which is worse.
- `<br>` is accepted as a newline though the spec does not list it; apps send it.
- Image scaling is nearest-neighbour. Good enough for a 128 px toast icon.
- On a very short output the layout can hide a toast the timers still count as
  visible, so it may expire unseen. Normal screens never hit this.

## Test

    cargo test -p veil-notify
    cargo clippy -p veil-notify --all-targets -- -D warnings
