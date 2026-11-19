# veil-lock

The Veil lock screen. An `ext-session-lock-v1` client run by the compositor as `veil shell lock`.

## How locking works

- **The compositor locks, this client unlocks.** While the session is locked the compositor draws only the dimmed wallpaper and the lock surfaces, runs no keybinds, and sends the keyboard only to a lock surface (see `veil-host/src/session_lock.rs`).
- **The lock outlives this client.** If `veil-lock` crashes or is killed the session stays locked and the compositor starts it again. Only an `unlock_and_destroy` from the client that holds the lock ends the lock.
- **Triggers:** `veil ctl session.lock`, the power menu, the idle timeout, and the lid action (`[idle]` in `config.toml`). With `lid_action = "lock+suspend"` the machine suspends only after the lock is in place.

## Authentication

PAM, authentication only (no session, no account-management step). Services tried in order: `veil-lock`, `login`, `system-auth`.

```sh
sudo install -Dm644 crates/veil-lock/dist/pam.d/veil-lock /etc/pam.d/veil-lock
```

`system-auth` is the stack your login uses, so faillock and any `pam_u2f` / fingerprint setup apply here too; their prompts are shown on the lock screen. Typed passwords are wiped from memory when cleared or replaced.

## Profile picture

The picture is the one AccountsService has for your account (`IconFile`, the store the desktop settings, DMS, SDDM and GDM use), so changing it there changes it here. Fallback: `~/.face`, then `~/.face.icon`. PNG or JPEG, any size (centre-cropped to a square). With none, or a broken file, the lock screen shows your initial. The lookup runs in the background, so the picture appears a moment after the screen does.

## Keys

`Enter` unlock, `Backspace` delete, `Esc` or `Ctrl+U` clear the field. Pointer and touch are ignored while locked.

`Tab` opens the power menu (Suspend, Reboot, Power off): `←` `→` / `h` `l` or `1`-`3` pick, `Enter` runs it, `Esc` or `Tab` closes. Reboot and Power off ask for a second `Enter`; any other key cancels. The menu never touches the password field. Actions go to the compositor (`session.suspend`, `session.reboot`, `session.poweroff`), or to `systemctl` if there is no compositor connection.

## Status

Top right: battery level, with time left (discharging) or time to full (charging), read from `/sys/class/power_supply/BAT*` and smoothed. Nothing is drawn without a battery. A "Caps Lock is on" line shows under the field when Caps Lock is active.

## If the lock screen cannot start

The compositor shows a notice on a dimmed screen and retries (every 2 s, then every 30 s after a crash loop). Switch to a TTY (`Ctrl+Alt+F3`), log in and run:

```sh
veil ctl restart lock
```

VT switching is handled below the compositor's input layer and works while locked.

## Testing

```sh
cargo test -p veil-lock          # key handling, rendering, PAM edge cases
cargo test -p veil-host session_lock   # lock client respawn and suspend policy
```

The protocol path (lock, crash recovery, takeover, unlock) needs a real session; see the checklist in the Veil docs.
