use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use wgpu::util::DeviceExt;

const HB_SHADER: &str = include_str!("shaders/halfblock.wgsl");
const LUMA_SHADER: &str = include_str!("shaders/luma.wgsl");

/// Longest we block the frame loop waiting for one frame's GPU work (G1). A
/// healthy encode of this size takes a few ms; if it takes this long the GPU
/// is hung or reset, and waiting forever would freeze the whole compositor.
const READBACK_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct GpuEncoder {
    device: wgpu::Device,
    queue: wgpu::Queue,
    hb_pipeline: wgpu::ComputePipeline,
    luma_pipeline: wgpu::ComputePipeline,
    hb_res: Option<FrameRes>,
    luma_res: Option<FrameRes>,
    /// Set once the device can't be trusted (uncaptured wgpu error, device
    /// lost, readback timeout). Every `encode_*` then returns `None` and the
    /// caller drops this encoder and uses the CPU path for good.
    dead: Arc<AtomicBool>,
    /// Device limits, cached: largest source texture side, largest output
    /// buffer. Anything bigger is refused up front (`None`) instead of
    /// tripping wgpu's validation, which panics under `panic = "abort"` (G2).
    max_dim: u32,
    max_buf: u64,
    warned_limits: bool,
    /// Set from `VEIL_GPU_TRACE=1` at construction. When on, `encode_*`
    /// prints upload/dispatch time vs readback/poll time separately every
    /// 60 calls, so a regression can be pinned to a phase instead of guessed
    /// at. Zero cost when unset (one bool check per call, no timing done).
    trace: bool,
    trace_n: u32,
}

/// Cached GPU-side resources for one (src_w, src_h, cols, rows) geometry.
struct FrameRes {
    src_w: u32,
    src_h: u32,
    cols: u16,
    rows: u16,
    texture: wgpu::Texture,
    out_buf: wgpu::Buffer,
    staging: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl GpuEncoder {
    /// Initialise the GPU encoder on the highest-performance Vulkan device.
    /// Returns `None` if no Vulkan adapter is available, *or* if the only
    /// adapter Vulkan can offer is a software rasterizer (llvmpipe/lavapipe
    /// — `DeviceType::Cpu`). A software adapter means there's no real GPU to
    /// offload to: every dispatch still runs on the same CPU cores as
    /// everything else, just behind extra driver/sync overhead the plain
    /// CPU path (`veil_render::rgba_to_halfblocks` / `compute_luma`) doesn't
    /// pay. Returning `None` here means the caller falls back to that CPU
    /// path automatically, which is strictly faster in that situation.
    pub fn new() -> Option<Self> {
        pollster::block_on(Self::init())
    }

    async fn init() -> Option<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });

        // G4: two tiny compute shaders don't justify waking a discrete GPU on a
        // hybrid-graphics laptop. LowPower is the default; VEIL_GPU_POWER=high
        // restores the old behaviour.
        let power_preference = match std::env::var("VEIL_GPU_POWER").as_deref() {
            Ok("high") => wgpu::PowerPreference::HighPerformance,
            _ => wgpu::PowerPreference::LowPower,
        };
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference,
                compatible_surface: None,
                force_fallback_adapter: false,
                // New in wgpu 30: adjusts adapter limits/features to match a
                // predefined bucket. We're not rendering to a surface or
                // targeting WebGPU compliance, just running two fixed compute
                // shaders with sizes we control — no reason to let bucketing
                // touch our limits, so keep it off (also its documented default).
                apply_limit_buckets: false,
            })
            .await
            .ok()?;

        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu {
            eprintln!(
                "[veil-gpu] {} is a software rasterizer, not a real GPU — skipping, CPU render path is faster here",
                info.name
            );
            return None;
        }
        eprintln!("[veil-gpu] {}", info.name);

        // G2: ask for what the adapter really supports. The old
        // `Limits::default()` capped textures at 8192 px per side, so a wide
        // virtual desktop made `create_texture` fail.
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: None,
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .ok()?;

        // wgpu's default uncaptured-error handler panics, which aborts the
        // whole compositor under `panic = "abort"`. Replace it: log, mark the
        // encoder dead, and let the caller fall back to the CPU path.
        let dead = Arc::new(AtomicBool::new(false));
        {
            let dead = dead.clone();
            device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
                eprintln!("[veil-gpu] wgpu error, disabling GPU encoder: {e}");
                dead.store(true, Ordering::Relaxed);
            }));
        }
        {
            let dead = dead.clone();
            device.set_device_lost_callback(move |reason, msg| {
                // `Destroyed` also fires on a normal drop; only a real loss matters.
                if matches!(reason, wgpu::DeviceLostReason::Unknown) {
                    eprintln!("[veil-gpu] device lost ({msg}), disabling GPU encoder");
                    dead.store(true, Ordering::Relaxed);
                }
            });
        }
        let limits = device.limits();
        let max_dim = limits.max_texture_dimension_2d;
        let max_buf = limits
            .max_buffer_size
            .min(limits.max_storage_buffer_binding_size);

        let hb_pipeline = make_pipeline(&device, HB_SHADER, "halfblock");
        let luma_pipeline = make_pipeline(&device, LUMA_SHADER, "luma");
        if dead.load(Ordering::Relaxed) {
            // Pipeline/shader creation already failed: not worth keeping.
            return None;
        }
        let trace = std::env::var("VEIL_GPU_TRACE").is_ok();
        if trace {
            eprintln!("[veil-gpu] VEIL_GPU_TRACE on — timing every 60th encode call");
        }

        Some(Self {
            device,
            queue,
            hb_pipeline,
            luma_pipeline,
            hb_res: None,
            luma_res: None,
            dead,
            max_dim,
            max_buf,
            warned_limits: false,
            trace,
            trace_n: 0,
        })
    }

    /// True once the device is unusable (see the `dead` field). The caller
    /// should drop this encoder and stay on the CPU path.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    /// Cheap pre-flight for one frame. `false` means "use the CPU path for
    /// this frame": dead device, a degenerate geometry (a terminal shrunk to
    /// 1 row gives zero usable rows → zero-size buffers, G3), a short pixel
    /// buffer, or a size beyond the device limits (G2).
    fn can_encode(
        &mut self,
        rgba_len: usize,
        src_w: u32,
        src_h: u32,
        cols: u16,
        rows: u16,
        out_bytes: u64,
    ) -> bool {
        if self.is_dead() || cols == 0 || rows == 0 || src_w == 0 || src_h == 0 {
            return false;
        }
        if (rgba_len as u64) < src_w as u64 * src_h as u64 * 4 {
            return false;
        }
        if src_w > self.max_dim || src_h > self.max_dim || out_bytes > self.max_buf {
            if !self.warned_limits {
                self.warned_limits = true;
                eprintln!(
                    "[veil-gpu] {src_w}x{src_h} -> {cols}x{rows} exceeds device limits \
                     (max texture {}, max buffer {} B) — using the CPU encoder for such frames",
                    self.max_dim, self.max_buf
                );
            }
            return false;
        }
        true
    }

    /// Pass a readback result through, poisoning the encoder on failure: a
    /// timed-out or failed map means the GPU can't be trusted any more.
    fn finish<T>(&self, out: Option<T>) -> Option<T> {
        if out.is_none() {
            self.dead.store(true, Ordering::Relaxed);
        }
        if self.is_dead() {
            None
        } else {
            out
        }
    }

    /// GPU halfblock encode: upload RGBA → compute shader samples top/bot
    /// pixel pairs per cell → returns ColorCell vec ready for emit_halfblocks.
    ///
    /// `&mut self`: geometry-matched GPU resources are cached on `self` and
    /// only rebuilt when `(src_w, src_h, cols, rows)` changes (i.e. on
    /// terminal resize) — the steady-state per-frame path just writes new
    /// pixels into the existing texture and re-dispatches.
    ///
    /// Returns `None` when this frame can't or shouldn't go through the GPU
    /// (zero-size grid, over device limits, short buffer, GPU hang/loss). The
    /// caller then encodes on the CPU, and drops the encoder if
    /// [`Self::is_dead`].
    pub fn encode_halfblock(
        &mut self,
        rgba: &[u8],
        src_w: u32,
        src_h: u32,
        cols: u16,
        rows: u16,
    ) -> Option<Vec<crate::ColorCell>> {
        let bytes = cols as u64 * rows as u64 * 8; // 2 × u32 per cell
        if !self.can_encode(rgba.len(), src_w, src_h, cols, rows, bytes) {
            return None;
        }
        let t0 = self.trace.then(std::time::Instant::now);
        let (res, rebuilt) = ensure_res(
            &self.device,
            &self.hb_pipeline,
            &mut self.hb_res,
            src_w,
            src_h,
            cols,
            rows,
            bytes,
            "hb",
        );

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &res.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * src_w),
                rows_per_image: Some(src_h),
            },
            wgpu::Extent3d {
                width: src_w,
                height: src_h,
                depth_or_array_layers: 1,
            },
        );

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("hb") });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("hb"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.hb_pipeline);
            pass.set_bind_group(0, &res.bind_group, &[]);
            pass.dispatch_workgroups((cols as u32).div_ceil(8), (rows as u32).div_ceil(8), 1);
        }
        enc.copy_buffer_to_buffer(&res.out_buf, 0, &res.staging, 0, bytes);
        self.queue.submit([enc.finish()]);
        let t1 = self.trace.then(std::time::Instant::now);

        let cells = readback(&self.device, &res.staging, |raw| {
            // G5: decode straight from the mapped range — no intermediate
            // `to_vec()` and no alignment-sensitive `cast_slice`.
            raw.as_chunks::<8>()
                .0
                .iter()
                .map(|c| {
                    let w0 = u32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
                    let w1 = u32::from_ne_bytes([c[4], c[5], c[6], c[7]]);
                    crate::ColorCell {
                        fg: [byte(w0, 0), byte(w0, 1), byte(w0, 2)],
                        bg: [byte(w1, 0), byte(w1, 1), byte(w1, 2)],
                    }
                })
                .collect::<Vec<_>>()
        });
        let t2 = self.trace.then(std::time::Instant::now);
        self.trace_tick("hb", rebuilt, t0, t1, t2);
        self.finish(cells)
    }

    /// GPU luma encode: upload RGBA → compute shader computes Rec.601 luma
    /// per cell → returns luma vec ready for luma_to_chars / apply_hysteresis.
    /// Same cache-and-reuse strategy as [`Self::encode_halfblock`].
    pub fn encode_luma(
        &mut self,
        rgba: &[u8],
        src_w: u32,
        src_h: u32,
        cols: u16,
        rows: u16,
    ) -> Vec<u8> {
        let bytes = cols as u64 * rows as u64 * 4; // 1 × u32 per cell
        let t0 = self.trace.then(std::time::Instant::now);
        let (res, rebuilt) = ensure_res(
            &self.device,
            &self.luma_pipeline,
            &mut self.luma_res,
            src_w,
            src_h,
            cols,
            rows,
            bytes,
            "luma",
        );

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &res.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * src_w),
                rows_per_image: Some(src_h),
            },
            wgpu::Extent3d {
                width: src_w,
                height: src_h,
                depth_or_array_layers: 1,
            },
        );

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("luma"),
            });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("luma"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.luma_pipeline);
            pass.set_bind_group(0, &res.bind_group, &[]);
            pass.dispatch_workgroups((cols as u32).div_ceil(8), (rows as u32).div_ceil(8), 1);
        }
        enc.copy_buffer_to_buffer(&res.out_buf, 0, &res.staging, 0, bytes);
        self.queue.submit([enc.finish()]);
        let t1 = self.trace.then(std::time::Instant::now);

        let raw = readback(&self.device, &res.staging, bytes);
        let t2 = self.trace.then(std::time::Instant::now);
        self.trace_tick("luma", rebuilt, t0, t1, t2);

        let u32s: &[u32] = bytemuck::cast_slice(&raw);
        u32s.iter().map(|&v| (v & 0xFF) as u8).collect()
    }

    /// Print upload/dispatch time vs readback/poll time every 60th call,
    /// plus whether this call rebuilt cached resources (a `true` on every
    /// call — instead of only on real resize — means geometry is jittering
    /// frame to frame and the cache is thrashing back to per-frame alloc).
    /// No-op unless `VEIL_GPU_TRACE` was set at construction.
    fn trace_tick(
        &mut self,
        label: &str,
        rebuilt: bool,
        t0: Option<std::time::Instant>,
        t1: Option<std::time::Instant>,
        t2: Option<std::time::Instant>,
    ) {
        if !self.trace {
            return;
        }
        self.trace_n = self.trace_n.wrapping_add(1);
        if rebuilt {
            eprintln!("[veil-gpu:{label}] resource cache REBUILT this call (geometry changed)");
        }
        if self.trace_n.is_multiple_of(60) {
            if let (Some(t0), Some(t1), Some(t2)) = (t0, t1, t2) {
                eprintln!(
                    "[veil-gpu:{label}] upload+dispatch={:.2}ms  readback={:.2}ms  total={:.2}ms",
                    (t1 - t0).as_secs_f64() * 1000.0,
                    (t2 - t1).as_secs_f64() * 1000.0,
                    (t2 - t0).as_secs_f64() * 1000.0,
                );
            }
        }
    }
}

/// Return the cached `FrameRes` for this pipeline if the geometry still
/// matches, otherwise build a fresh one (new texture sized for `src_w ×
/// src_h`, new output/staging buffers sized for `cols × rows`, new bind
/// group) and cache that instead. This is the only place new GPU resources
/// get allocated — everything else in the hot path just writes into what's
/// already there.
#[allow(clippy::too_many_arguments)]
fn ensure_res<'a>(
    device: &wgpu::Device,
    pipeline: &wgpu::ComputePipeline,
    slot: &'a mut Option<FrameRes>,
    src_w: u32,
    src_h: u32,
    cols: u16,
    rows: u16,
    out_bytes: u64,
    label: &'static str,
) -> (&'a FrameRes, bool) {
    let stale = match slot {
        Some(r) => r.src_w != src_w || r.src_h != src_h || r.cols != cols || r.rows != rows,
        None => true,
    };
    if stale {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: src_w,
                height: src_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let params = params_buf(device, src_w, src_h, cols as u32, rows as u32);
        let out_buf = storage_buf(device, out_bytes, label);
        let staging = staging_buf(device, out_bytes, label);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&view)),
                entry(1, out_buf.as_entire_binding()),
                entry(2, params.as_entire_binding()),
            ],
        });
        *slot = Some(FrameRes {
            src_w,
            src_h,
            cols,
            rows,
            texture,
            out_buf,
            staging,
            bind_group,
        });
    }
    (slot.as_ref().unwrap(), stale)
}

// ─── helpers ─────────────────────────────────────────────────────────────────
// (per-frame `upload_texture` is gone — `ensure_res` builds the texture once
// and the hot path calls `queue.write_texture` on the cached one instead)

fn params_buf(device: &wgpu::Device, src_w: u32, src_h: u32, cols: u32, rows: u32) -> wgpu::Buffer {
    let data: [u32; 4] = [src_w, src_h, cols, rows];
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("params"),
        contents: bytemuck::cast_slice(&data),
        usage: wgpu::BufferUsages::UNIFORM,
    })
}

fn storage_buf(device: &wgpu::Device, size: u64, label: &'static str) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    })
}

fn staging_buf(device: &wgpu::Device, size: u64, label: &'static str) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn readback(device: &wgpu::Device, staging: &wgpu::Buffer, size: u64) -> Vec<u8> {
    let slice = staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    let _ = device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    let mapped = rx.recv().ok().and_then(|r| r.ok()).is_some();
    if !mapped {
        // map_async was still issued (and, per wgpu, must be balanced by an
        // unmap even on failure) — `staging` outlives this call now that
        // it's cached, so skipping this would leave it stuck "mapped" and
        // poison every future frame's map_async on the same buffer.
        staging.unmap();
        return vec![0u8; size as usize];
    }
    let out = match slice.get_mapped_range() {
        Ok(data) => {
            let out = data.to_vec();
            drop(data);
            out
        }
        Err(e) => {
            eprintln!("[veil-gpu] get_mapped_range failed: {e}");
            vec![0u8; size as usize]
        }
    };
    staging.unmap();
    out
}

fn entry(binding: u32, resource: wgpu::BindingResource<'_>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource }
}

fn byte(word: u32, shift: u32) -> u8 {
    ((word >> (shift * 8)) & 0xFF) as u8
}

fn make_pipeline(device: &wgpu::Device, wgsl: &str, label: &str) -> wgpu::ComputePipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(wgsl)),
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: None, // auto-layout from shader reflection
        module: &module,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    })
}
