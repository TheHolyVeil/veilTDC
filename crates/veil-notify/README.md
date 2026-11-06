# veil-notify

The Veil notification service (phase 4 of the Veil Shell Architecture).
Scope and decisions live in `Veil_Shell.md`, section 11, Phase 4.

## State: step 5 of 7 (toasts work end to end)

| Step | What | State |
| --- | --- | --- |
| 1 | `text`, `markup`, `model`, `store`, `rate` | done |
| 2 | `timers` (expiry state machine), `layout` (toast stack geometry) | done |
| 3 | `dbus` (zbus 6 service, signals, name queueing), `ids` | done |
| 4 | `ipc` client, host relay (`veil-host`), bell fix (`veil-bar`), shared payloads (`veil-ipc`) | done, 79 tests here |
| 5 | toasts: `engine`, `toast`, `wrap`, `layout`, `waker`, `app`; `veil shell notify` entry | done, 118 tests here + 3 end-to-end |
| 6 | history panel | next |
| 7 | supervisor registration (`shell.notify` config key), `notify.*` config keys | |

No Wayland yet. Everything except `dbus` is deterministic and takes the clock as
an argument. `dbus` runs one worker thread (`veil-notify-dbus`) and talks to the
main loop only through channels. Try it without any UI:

    cargo run -p veil-notify --example headless
    notify-send -A yes=Yes --wait "hi"        # prints: yes

## Invariants (do not break)

- **Body markup** is canonical: only `<b> <i> <u>`, always balanced, text
  escaped. `markup::sanitize` is the only way a body gets in, and
  `parse(render(x)) == x`. Text is decoded once and never re-parsed, so
  `&lt;b&gt;` can never become bold.
- **Summaries are plain text**, one line. Markup in a summary shows as typed.
- **Banned characters** (controls, bidi overrides) never reach a stored string.
- **Action keys are never truncated**, because they are echoed back to the app
  in `ActionInvoked`. A key that is too long is dropped with its action.
- **Ids** are never 0 and never reused across a restart. The counter is an
  `ids::IdGen` (atomic, shared with the D-Bus thread, which must return an id
  synchronously); the main loop saves `IdGen::peek()` with the history and
  `store::Loaded::next_id` is lifted above the highest stored id on load.
- **History file**: `$XDG_RUNTIME_DIR/veil/notify-history.json`, 0600, directory
  0700, written by temp file plus rename. Loading never fails: a missing,
  foreign-owned, oversized, corrupt or wrong-version file is an empty store.
  Images are not persisted. Restored entries are history only, never re-toasted.
- **D-Bus name**: requested with *no* flags (queue, never steal, not replaceable).
  zbus's `Builder::name` and `request_name` use `REPLACE_EXISTING | DO_NOT_QUEUE`,
  the opposite, so the name is requested with `request_name_with_flags` after
  `serve_at`. A queued daemon polls for ownership every 2 s; an owner never wakes
  on its own.
- **`replaces_id` / `CloseNotification` work for any caller** if the id was
  handed out. Do not add per-sender ownership: every `notify-send -r` is a new
  bus connection, so it breaks the standard volume-script pattern, and any
  session client can already post under any app name. `replaces_id` returns the
  same id (spec); an id never handed out is not honoured.
- **D-Bus handlers stay cheap**: parse, sanitise, allocate an id, `try_send` into
  a bounded channel (256). A full queue drops the event, it never blocks D-Bus
  or grows memory.
- **IPC** (`ipc.rs`): publishes the bell as `notifications.set`, receives
  `notify.control` (dnd, panel), `palette` and `config`. The reader thread blocks
  on the socket; the only periodic wakeup is the ping. The ping is sent when due
  even during a burst of traffic (the supervisor restarts a component silent for
  10 s). Only changed states are sent. On reconnect the latest state is re-sent.
  The master relays, since clients cannot publish events themselves.
- **Rate limiter table is bounded** (256 apps, then one shared bucket).
- **Timers**: only the newest `max_visible` toasts count down; hidden ones are
  paused, never expired unseen. Hover pauses all of them. Critical and timeout 0
  never expire. A late `poll` replays expiries in order and charges a revealed
  toast only for the time it was actually visible. Mutators queue expiries, and
  `poll` is the only place they come out.
- **One layer surface per toast**, each its own Wayland connection, anchored
  top-right with its stack position as the top margin. Gaps are real gaps, so
  clicks fall through with no input-region tricks (the host's handling of input
  regions is an open question, see the ignored test in `crates/veil/tests/layer_shell.rs`).
  `margin_top` is only the visual gap: the host already places a zero-zone
  overlay below the bar's exclusive zone (measured, same file). The first toast
  is always shown; later ones only while the stack stays under `max_height`.
- **The engine is I/O-free** (`engine.rs`): every rule about DND, replace, rate
  limits, dismiss, actions, expiry and the bell is tested there with a fake
  clock. `app.rs` only moves events in and effects out. Keep it that way.
- **The loop sleeps in `poll(2)`** on the waker plus every toast's Wayland fd,
  with a timeout from `Core::next_wake`; idle is zero wakeups. A pass that
  changed anything runs again before sleeping: queued Wayland requests are not
  sent until flushed and a pointer event may already be buffered.
- **Untrusted image headers** are validated before any index is computed; all
  size arithmetic is checked.

## Defaults (proposals, become `notify.*` config keys in step 4)

summary 256 B, body 4096 B, image long edge 128 px, 4 buttons, history 100,
rate limit 20 per 10 s per app, default timeout 5 s (clamped 0.5 s to 1 h),
5 visible toasts, toast width 360 px, gap 8 px.

## Known limits

- We do not emit the optional `ActivationToken` signal (spec 1.3).
- Two zbus builds exist in the workspace: 6.x here and in `veil-bar`, 5.19 pulled
  in by Slint (velogin). Harmless, costs compile time only.
- `x<y and z>w` reads as a tag and loses `<y and z>`. Ambiguous input; the
  alternative is rendering real tags as text, which is worse.
- `<br>` is accepted as a newline though the spec does not list it; apps send it.
- Image scaling is nearest-neighbour. Good enough for a 128 px toast icon.
- If the stack exceeds `max_height` (900 px) a toast the timers count as visible
  is not shown and may expire unseen. Needs five very long toasts at once.
- No "+n more" indicator for toasts beyond the visible five; they wait, and the
  bell counts them. A history panel (step 6) is where they are listed.
- `<i>` shows upright, and `image-path` / `app_icon` files are not drawn
  (`image-data` is). Faux bold is the glyphs drawn twice 0.6 px apart.
- Toasts show on the first output (`OutputFilter::First`). Following focus needs
  the host to publish the focused output; it knows (`active_monitor_idx`), it
  just is not in the `focus` event yet.
- `notify.panel` currently only flips the engine's panel flag and marks
  everything read, because there is no panel until step 6.
- Settings are code defaults; the `notify.*` config keys come with step 7.

## Test

    cargo test -p veil-notify
    cargo clippy -p veil-notify --all-targets -- -D warnings
