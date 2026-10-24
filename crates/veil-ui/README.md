# veil-ui

Drawing primitives, theme token synchronization, widget traits, and layer-shell client runtime for Veil components.

## What it is
`veil-ui` provides the shared UI toolkit for all supervised Veil child components (`veil-bar`, `veil-notify`, `veil-osd`, `veil-launch`, `veil-power`, `veil-settings`). It wraps `tiny-skia` for 2D software drawing, integrates `veil-text` for typography, provides dynamic palette theming, and encapsulates `smithay-client-toolkit` for zero-boilerplate `wlr-layer-shell` integration.

## Public Contract
- **Drawing**: `fill_rect`, `fill_rounded_rect`, `stroke_rounded_rect`, `fill_circle`, `hex_color`.
- **Theme**: `Theme` and `ThemeTokens` (`bg`, `surface`, `surface_hi`, `border`, `text`, `accent`, etc.).
- **LayerShellClient**: Initializes connection, handles auto-discovery of outputs, requests layer/anchor/exclusive zone, and renders via `present_for_each`.
- **Widget**: Trait for self-contained UI widgets (`update`, `draw`, `on_click`).

## Invariants
1. **Never depends on veil-host**: Components link `veil-ui`, `veil-text`, `veil-ipc`, and `veil-config`. Never the compositor core.
2. **Crash isolation**: Surface rendering errors inside `veil-ui` are caught cleanly without risking the compositor process.

## How to test
```bash
cargo test -p veil-ui
```
