// Luma encoder — for each terminal cell, box/area-average every source texel
// in the cell's coverage rectangle (not a single nearest-neighbour point),
// compute Rec.601 luma, write to output buffer.
//
// Point sampling aliases badly on anti-aliased glyph edges. Averaging every
// source texel under each cell is the standard downsampling fix for that
// aliasing class.
//
// Text-cell detection: when the intra-cell contrast (max−min luma) exceeds
// TEXT_CONTRAST_T, the cell likely contains an ink glyph on a background.
// The ink-side luma is written instead of the box average. The average smears
// anti-aliased glyphs to mid-gray (~0.5), which maps to mid-density LUMA_MAP
// characters regardless of glyph shape; the ink minimum (dark-on-light) or
// maximum (light-on-dark) pushes glyph cells to clearly dark or clearly bright
// values. Adjacent glyph cells also become similarly extreme, collapsing
// intra-region cell-to-cell contrast and reducing false |/- edge detection
// inside text areas. Mirrors compute_luma in veil-render/src/lib.rs — both
// paths must produce the same output for the same input frame.
//
// Output layout: 1 × u32 per cell, row-major.  Only the low 8 bits are used.

struct Params {
    src_w : u32,
    src_h : u32,
    cols  : u32,
    rows  : u32,
}

@group(0) @binding(0) var src : texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> out : array<u32>;
@group(0) @binding(2) var<uniform> p : Params;

// Sample every texel in the half-open box [x0,x1) × [y0,y1), computing the
// box-average luma plus per-cell min and max for text-detection. Clamped to at
// least one texel wide/tall so upsampling (eff dims > src dims) degrades to a
// point sample instead of a div-by-0.
// Returns vec3(avg_luma, min_luma, max_luma).
fn box_sample(x0: u32, x1: u32, y0: u32, y1: u32) -> vec3<f32> {
    let xe = max(x0 + 1u, x1);
    let ye = max(y0 + 1u, y1);
    var sum   = 0.0;
    var mn    = 1.0;
    var mx    = 0.0;
    var count = 0u;
    for (var y = y0; y < ye; y = y + 1u) {
        for (var x = x0; x < xe; x = x + 1u) {
            let rgb = textureLoad(src, vec2<i32>(i32(x), i32(y)), 0).rgb;
            let l   = rgb.r * 0.299 + rgb.g * 0.587 + rgb.b * 0.114;
            sum    += l;
            mn      = min(mn, l);
            mx      = max(mx, l);
            count  += 1u;
        }
    }
    let avg = sum / f32(count);
    return vec3<f32>(avg, mn, mx);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let col = id.x;
    let row = id.y;
    if col >= p.cols || row >= p.rows { return; }

    let x0 = col * p.src_w / p.cols;
    let x1 = (col + 1u) * p.src_w / p.cols;
    let y0 = row * p.src_h / p.rows;
    let y1 = (row + 1u) * p.src_h / p.rows;

    let s        = box_sample(x0, x1, y0, y1);
    let avg      = s.x;
    let mn       = s.y;
    let mx       = s.z;
    let contrast = mx - mn;

    // Text-cell detection (60/255 ≈ 0.2353; mirrors compute_luma).
    // High intra-cell contrast → ink glyph on background.
    // select(false_val, true_val, condition) — WGSL argument order.
    //   Light background (avg > 0.5) → ink is the dark minimum.
    //   Dark  background (avg ≤ 0.5) → ink is the bright maximum.
    let ink_luma = select(mx, mn, avg > 0.5);
    let luma     = select(avg, ink_luma, contrast >= 0.2353);

    out[row * p.cols + col] = u32(luma * 255.0);
}
