// Halfblock encoder — for each terminal cell, box/area-average every source
// texel in the top and bottom coverage rectangles (not a single nearest-
// neighbour point), pack fg/bg RGB into the output buffer.
//
// Point sampling aliases badly on anti-aliased glyph edges — text over a
// background produces the "gibberish symbols" artifact. Averaging every
// source texel under each cell is the standard downsampling fix for that
// aliasing class (still just non-aliased, not typographically crisp — the
// AT-SPI text-overlay path in veil-render/src/lib.rs handles crispness for
// ascii_luma/ascii_edge tiers).
//
// Output layout: 2 × u32 per cell, row-major.
//   word0 = fg.r | (fg.g << 8) | (fg.b << 16)
//   word1 = bg.r | (bg.g << 8) | (bg.b << 16)

struct Params {
    src_w : u32,
    src_h : u32,
    cols  : u32,
    rows  : u32,
}

@group(0) @binding(0) var src : texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> out : array<u32>;
@group(0) @binding(2) var<uniform> p : Params;

// Average every texel in the half-open box [x0,x1) × [y0,y1). Clamped to at
// least one texel wide/tall so upsampling (eff dims > src dims, box would
// otherwise be empty) degrades to the old point sample instead of a div-by-0.
fn box_avg(x0: u32, x1: u32, y0: u32, y1: u32) -> vec3<f32> {
    let xe = max(x0 + 1u, x1);
    let ye = max(y0 + 1u, y1);
    var sum = vec3<f32>(0.0);
    var count = 0u;
    for (var y = y0; y < ye; y = y + 1u) {
        for (var x = x0; x < xe; x = x + 1u) {
            sum += textureLoad(src, vec2<i32>(i32(x), i32(y)), 0).rgb;
            count = count + 1u;
        }
    }
    return sum / f32(count);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let col = id.x;
    let row = id.y;
    if col >= p.cols || row >= p.rows { return; }

    // Each terminal row maps to two source pixel rows via ▀ (top=fg, bot=bg).
    let eff_w = p.cols;
    let eff_h = p.rows * 2u;

    let x0 = col * p.src_w / eff_w;
    let x1 = (col + 1u) * p.src_w / eff_w;
    let top_y0 = (row * 2u)      * p.src_h / eff_h;
    let top_y1 = (row * 2u + 1u) * p.src_h / eff_h;
    let bot_y0 = (row * 2u + 1u) * p.src_h / eff_h;
    let bot_y1 = (row * 2u + 2u) * p.src_h / eff_h;

    let fg = box_avg(x0, x1, top_y0, top_y1);
    let bg = box_avg(x0, x1, bot_y0, bot_y1);

    let fr = u32(fg.r * 255.0);
    let fg_ = u32(fg.g * 255.0);
    let fb = u32(fg.b * 255.0);
    let br = u32(bg.r * 255.0);
    let bg_ = u32(bg.g * 255.0);
    let bb = u32(bg.b * 255.0);

    let idx = (row * p.cols + col) * 2u;
    out[idx]      = fr | (fg_ << 8u) | (fb << 16u);
    out[idx + 1u] = br | (bg_ << 8u) | (bb << 16u);
}
