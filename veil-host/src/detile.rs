//! GPU dmabuf importer — the detile path for tiled/compressed client buffers.
//!
//! veil's fast path CPU-mmaps *linear* dmabufs (see `server::import_dmabuf`).
//! GPU compositors (niri, Hyprland, sway) and most GL/Vulkan apps hand us
//! *tiled* buffers (AMD DCC / implicit modifiers) whose memory layout the CPU
//! can't read directly — mmapping one and reading it pixel-by-pixel stalls the
//! compositor thread for seconds (the freeze veil used to hit on GPU apps).
//!
//! This imports such a buffer as an EGLImage: the DRM driver, which knows the
//! tiling, samples it into a normal texture, and we read that back as linear
//! RGBA straight into the existing blit path. Any modifier the render node
//! supports now works. If EGL/GLES can't be brought up (headless i686, no GPU),
//! [`GpuImporter::new`] returns `None` and veil stays CPU-only + linear-only —
//! no regression to the minimum-hardware mode.

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::allocator::{Buffer, Format, Fourcc};
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ExportMem, ImportDma, Renderer};
use smithay::utils::{DeviceFd, Rectangle};
use std::os::fd::OwnedFd;

/// An EGL/GLES context on a DRM render node, used purely to import and read
/// back client dmabufs the CPU can't map (tiled / non-linear modifiers).
pub struct GpuImporter {
    renderer: GlesRenderer,
}

impl GpuImporter {
    /// Bring up an EGL/GLES context on the first usable DRM render node.
    /// Returns `None` on any failure so the caller falls back to the CPU path.
    pub fn new() -> Option<Self> {
        let fd = open_render_node()?;
        let gbm = GbmDevice::new(DrmDeviceFd::new(DeviceFd::from(fd)))
        .map_err(|e| eprintln!("[veil-host] gpu: gbm open failed: {e}"))
        .ok()?;
        // SAFETY: `gbm` outlives the display for the program's lifetime (moved in).
        let display = unsafe { EGLDisplay::new(gbm) }
        .map_err(|e| eprintln!("[veil-host] gpu: EGL display failed: {e}"))
        .ok()?;
        let context = EGLContext::new(&display)
        .map_err(|e| eprintln!("[veil-host] gpu: EGL context failed: {e}"))
        .ok()?;
        // SAFETY: context is current-able on this (the compositor) thread and
        // never moved off it — GpuImporter lives in State, which is thread-local.
        let renderer = unsafe { GlesRenderer::new(context) }
        .map_err(|e| eprintln!("[veil-host] gpu: GLES renderer failed: {e}"))
        .ok()?;
        eprintln!("[veil-host] gpu: dmabuf importer ready (tiled buffers enabled)");
        Some(Self { renderer })
    }

    /// Formats (including tiled modifiers) the render node can import. Advertised
    /// to clients so they allocate GPU-native buffers we can now handle.
    pub fn formats(&self) -> Vec<Format> {
        self.renderer.dmabuf_formats().iter().copied().collect()
    }

    /// Validate that this dmabuf can actually be imported as an EGLImage,
    /// without reading it back. Called at buffer-creation time so buffers we
    /// *can't* detile are rejected — the client then falls back to shm instead
    /// of committing something we'd render blank. The trial texture is dropped
    /// immediately (releases the EGLImage).
    pub fn can_import(&mut self, dmabuf: &Dmabuf) -> bool {
        self.renderer.import_dmabuf(dmabuf, None).is_ok()
    }

    /// Import a dmabuf of any supported modifier and read it back as linear RGBA
    /// (R,G,B,A byte order — matches `SurfaceBuf`). `None` on failure.
    pub fn import(&mut self, dmabuf: &Dmabuf) -> Option<(Vec<u8>, u32, u32)> {
        let w = dmabuf.width();
        let h = dmabuf.height();
        let tex = self
        .renderer
        .import_dmabuf(dmabuf, None)
        .map_err(|e| eprintln!("[veil-host] gpu: import_dmabuf failed: {e}"))
        .ok()?;
        let region = Rectangle::from_size((w as i32, h as i32).into());

        // Scope the mapping so its full-screen PBO drops (queues for deletion)
        // BEFORE we run cleanup below — otherwise this frame's PBO isn't freed
        // until the next frame.
        let out = {
            // Abgr8888 little-endian = bytes R,G,B,A in memory — exactly SurfaceBuf.
            let mapping = self
            .renderer
            .copy_texture(&tex, region, Fourcc::Abgr8888)
            .map_err(|e| eprintln!("[veil-host] gpu: copy_texture failed: {e}"))
            .ok();
            match mapping {
                Some(m) => self
                .renderer
                .map_texture(&m)
                .map_err(|e| eprintln!("[veil-host] gpu: map_texture failed: {e}"))
                .ok()
                .map(|bytes| bytes.to_vec()),
                None => None,
            }
        };

        // CRITICAL: drain smithay's deferred-destruction queue and prune the
        // dmabuf cache. GlesRenderer defers glDeleteBuffers/Textures + EGLImage
        // destruction into a channel that is ONLY drained by cleanup(). We never
        // render through unbind() (the usual cleanup trigger), so without this
        // every copy_texture leaks a full-screen PBO per frame → runaway
        // RAM/VRAM (observed: 30 GB + swap hosting niri). `cleanup_texture_cache`
        // is the public trait entry point — it makes the context current, then
        // runs the private cleanup() that frees the queued GL/EGL resources.
        drop(tex);
        let _ = self.renderer.cleanup_texture_cache();

        out.map(|v| (v, w, h))
    }
}

/// Open the first accessible DRM render node (renderD128..135) read/write.
///
/// `pub(crate)`: also used by `server.rs` to open a second, independent fd on
/// the same node for the `linux-drm-syncobj-v1` import device — kept separate
/// from `GpuImporter`'s own EGL/GBM device so explicit-sync wiring never risks
/// the working detile path.
pub(crate) fn open_render_node() -> Option<OwnedFd> {
    use std::fs::OpenOptions;
    for n in 128..=135u32 {
        let path = format!("/dev/dri/renderD{n}");
        if let Ok(f) = OpenOptions::new().read(true).write(true).open(&path) {
            return Some(OwnedFd::from(f));
        }
    }
    eprintln!("[veil-host] gpu: no accessible DRM render node");
    None
}
