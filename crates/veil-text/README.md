# veil-text

Vector text rasterization, measurement, and glyph caching for Veil using `fontdue` 0.9 with a guaranteed 5x7 bitmap fallback.

## What it is
`veil-text` provides a clean, Wayland-free text rendering API used across all Veil UI components and built-in compositor overlays. It resolves fonts at runtime, rasterizes glyphs with grayscale anti-aliasing into `tiny-skia` buffers, routes Nerd Font code points automatically to icon fonts, and falls back to a 5x7 pixel font when no fonts are installed.

## Public Contract
- `Role`: `UI`, `Mono`, `Icons`.
- `TextSystem::new(&FontConfig)`: Initializes discovery and font loading.
- `measure(&str, Role, size: f32) -> (f32, f32)`: Returns `(width, height)`.
- `draw(&mut Pixmap, &str, x, y, Role, size, Color) -> f32`: Draws text and returns advance width.
- `ellipsize(&str, Role, size, max_width) -> String`: Truncates with `…` to fit bounded widths.

## Invariants
1. **Never fails**: Drawing or measuring unknown characters or running on systems without fonts will never panic or fail. It renders through the 5x7 bitmap fallback.
2. **Zero Wayland coupling**: Operates strictly on `tiny-skia::Pixmap` and strings.

## How to test
```bash
cargo test -p veil-text
```
