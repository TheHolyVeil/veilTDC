# veil

Multi-call binary for the Veil window manager and desktop ecosystem.

## What it is
`veil` is the single executable installed to `/usr/local/bin/veil` that handles:
1. **Compositor**: Running `veil` or `veil run <app>` or `veil start` launches the master compositor.
2. **Supervised Components**: Running `veil shell <name>` (e.g. `veil shell bar`) launches an isolated child shell component.
3. **IPC Control**: Running `veil ctl <command>` communicates directly with the compositor's IPC socket to query state, switch workspaces, or trigger reloads.

## Subcommands
- `veil run <cmd>`: Run an application inside Veil.
- `veil start`: Start an empty compositor session.
- `veil shell bar`: Run the desktop status bar as a supervised child process.
- `veil ctl ping`: Check IPC responsiveness.
- `veil ctl reload`: Hot-reload configuration.
- `veil ctl workspace <n>`: Switch active workspace.

## How to test
```bash
cargo build -p veil
cargo test --workspace
```
