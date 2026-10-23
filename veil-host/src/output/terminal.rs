//! Terminal output backend: Kitty graphics protocol, halfblock, or ASCII.
//!
//! Wraps the existing veil-render functions (rgba_to_halfblocks, compute_luma, etc.)
//! and renders to stdout via escape codes.

use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use std::io::{self, Write};
use veil_render::gpu::GpuEncoder;
use veil_render::{
    apply_hysteresis, compute_luma, luma_to_chars, render_kitty_frame, rgba_to_halfblocks,
    ColorDepth, HalfblockEmitter,
};

use super::{OutputBackend, TerminalOpts};

#[derive(Copy, Clone, Debug)]
pub enum TerminalMode {
    Kitty,
    Halfblock(ColorDepth),
    Ascii,
    AsciiEdge,
}

impl TerminalMode {
    /// Map a (resolved) config quality onto a render mode. `Auto`/`Sixel`
    /// shouldn't arrive here (`resolved_quality` removes them) but fall back
    /// to the detector rather than guessing.
    fn from_quality(q: &veil_config::Quality) -> Self {
        use veil_config::Quality as Q;
        match q {
            Q::Kitty => Self::Kitty,
            Q::Pixel => Self::Halfblock(ColorDepth::True),
            Q::Pixel256 => Self::Halfblock(ColorDepth::Ansi256),
            Q::Pixel16 => Self::Halfblock(ColorDepth::Ansi16),
            Q::AsciiLuma => Self::Ascii,
            Q::AsciiEdge => Self::AsciiEdge,
            Q::Auto | Q::Sixel => Self::from_quality(&veil_config::detect_quality()),
        }
    }
}

pub struct TerminalOutput {
    stdout: std::io::Stdout,
    mode: TerminalMode,
    width: u32,
    height: u32,
    cols: u16,
    rows: u16,
    gpu: Option<GpuEncoder>,
    stable_luma: Vec<u8>,
    /// Remembers the last halfblock frame so only changed cells are sent.
    halfblock: HalfblockEmitter,
    /// Pre-allocated render scratch buffer reused across frames to avoid
    /// per-frame heap allocation in the hot render path.
    render_buf: String,
}

impl TerminalOutput {
    /// `opts.gpu`: from config.lua. `None` (`gpu_render = false`) skips GPU
    /// init outright, same effect as if no Vulkan adapter existed, and the CPU
    /// path in veil-render is used instead. `Some(power)` is `gpu_power`.
    /// `opts.quality`: the resolved config.lua `quality`.
    pub fn new(opts: TerminalOpts) -> io::Result<Self> {
        let (cols, rows) = super::terminal_cells();
        let stdout = std::io::stdout();

        // Enable raw mode and alternate screen
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout_ref = std::io::stdout();
        execute!(
            stdout_ref,
            terminal::EnterAlternateScreen,
            terminal::Clear(ClearType::All),
            cursor::Hide,
            cursor::MoveTo(0, 0),
        )?;

        // Enable mouse: only any-event + SGR modes (avoid URXVT dup events)
        stdout_ref.write_all(b"\x1b[?1003h\x1b[?1006h")?;
        stdout_ref.flush()?;

        // Render mode comes from config.lua's `quality` (already resolved).
        let mode = TerminalMode::from_quality(&opts.quality);

        // Try to init GPU encoder (only useful for halfblock/ascii modes) —
        // gated on config.lua's gpu_render, and skipped for Kitty mode
        // (which never used the GPU path to begin with).
        let gpu = match opts.gpu {
            Some(power) if !matches!(mode, TerminalMode::Kitty) => {
                GpuEncoder::new(power == veil_config::GpuPower::Low)
            }
            _ => None,
        };

        let (pw, ph) = Self::term_pixel_size().unwrap_or((cols as u32 * 8, rows as u32 * 16));

        // Pre-allocate render buffer: halfblock emits ~40 bytes/cell on average
        // (ANSI color codes + ▀ UTF-8); reserve generously to avoid reallocs.
        let render_cap = cols as usize * rows as usize * 48;

        Ok(Self {
            stdout,
            mode,
            width: pw,
            height: ph,
            cols,
            rows,
            gpu,
            stable_luma: Vec::new(),
            halfblock: HalfblockEmitter::new(),
            render_buf: String::with_capacity(render_cap),
        })
    }

    /// Get terminal pixel dimensions from ioctl (not all terminals support this).
    fn term_pixel_size() -> Option<(u32, u32)> {
        let mut winsz: libc::winsize = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut winsz) };
        if ret == 0 && winsz.ws_xpixel > 0 && winsz.ws_ypixel > 0 {
            Some((winsz.ws_xpixel as u32, winsz.ws_ypixel as u32))
        } else {
            None
        }
    }

    /// Run `f` on the GPU encoder if there is one. `None` means "encode this
    /// frame on the CPU" (no GPU, or it declined the frame). If the encoder
    /// reports itself dead (hang, device loss, wgpu error) it is dropped here,
    /// so every later frame goes straight to the CPU path. Takes the field,
    /// not `&mut self`, so callers can keep `self.render_buf` borrowed.
    fn gpu_or_none<T>(
        gpu: &mut Option<GpuEncoder>,
        f: impl FnOnce(&mut GpuEncoder) -> Option<T>,
    ) -> Option<T> {
        let g = gpu.as_mut()?;
        let r = f(g);
        if g.is_dead() {
            eprintln!("[veil-host] GPU encoder lost, CPU encoder from now on");
            *gpu = None;
        }
        r
    }

    /// Renders directly into `self.render_buf`; the caller reads it afterward.
    /// Returns `false` when there is nothing to write (an unchanged halfblock
    /// frame), so the caller can skip the write entirely.
    fn render_output(&mut self, rgba: &[u8]) -> bool {
        let cols = self.cols;
        let rows = self.rows;
        let usable_rows = rows.saturating_sub(1);

        // Reuse the pre-allocated scratch buffer — clear without freeing.
        self.render_buf.clear();
        let out = &mut self.render_buf;

        match self.mode {
            TerminalMode::Kitty => {
                out.push_str("\x1b[H");
                out.push_str(&render_kitty_frame(
                    rgba,
                    self.width,
                    self.height,
                    cols,
                    usable_rows,
                ));
            }
            TerminalMode::Halfblock(depth) => {
                let (w, h) = (self.width, self.height);
                let cells = Self::gpu_or_none(&mut self.gpu, |g| {
                    g.encode_halfblock(rgba, w, h, cols, usable_rows)
                })
                .unwrap_or_else(|| rgba_to_halfblocks(rgba, w, h, cols, usable_rows));
                // Cell-diffed: positions itself with absolute cursor moves,
                // so no leading home sequence.
                return self.halfblock.emit(out, &cells, cols, usable_rows, depth);
            }
            TerminalMode::Ascii => {
                out.push_str("\x1b[H");
                let (w, h) = (self.width, self.height);
                let luma = Self::gpu_or_none(&mut self.gpu, |g| {
                    g.encode_luma(rgba, w, h, cols, usable_rows)
                })
                .unwrap_or_else(|| compute_luma(rgba, w, h, cols, usable_rows));
                let chars = luma_to_chars(&luma, cols, usable_rows);
                Self::emit_chars_vec(out, &chars, cols, usable_rows);
            }
            TerminalMode::AsciiEdge => {
                out.push_str("\x1b[H");
                let (w, h) = (self.width, self.height);
                let luma = Self::gpu_or_none(&mut self.gpu, |g| {
                    g.encode_luma(rgba, w, h, cols, usable_rows)
                })
                .unwrap_or_else(|| compute_luma(rgba, w, h, cols, usable_rows));
                if self.stable_luma.len() != luma.len() {
                    self.stable_luma = luma.clone();
                }
                apply_hysteresis(&mut self.stable_luma, &luma, 10);
                let chars = luma_to_chars(&self.stable_luma, cols, usable_rows);
                Self::emit_chars_vec(out, &chars, cols, usable_rows);
            }
        }
        true
    }

    fn emit_chars_vec(out: &mut String, chars: &[char], cols: u16, rows: u16) {
        // Encode chars into UTF-8 in batch — avoids thousands of individual
        // format! calls and write! dispatches, each of which carries overhead
        // for the fmt machinery. One encode_utf8 per char, one push_str per row.
        let mut row_buf = String::with_capacity(cols as usize * 4);
        for row in 0..rows as usize {
            if row > 0 {
                out.push_str("\r\n");
            }
            row_buf.clear();
            for col in 0..cols as usize {
                let idx = row * cols as usize + col;
                if idx < chars.len() {
                    row_buf.push(chars[idx]);
                }
            }
            out.push_str(&row_buf);
        }
    }
}

impl OutputBackend for TerminalOutput {
    fn render_frame(
        &mut self,
        _monitor: usize,
        rgba: &[u8],
        width: u32,
        height: u32,
        _damage: crate::layout::Rect,
    ) -> io::Result<()> {
        // _monitor unused: a terminal is inherently one viewport, so the
        // default `monitor_count() == 1` applies and this is always called
        // with `0`.
        //
        // _damage unused: the encoders walk the full cell grid every call.
        // Halfblock output is cell-diffed against the previous frame
        // (HalfblockEmitter), so unchanged cells cost nothing on the wire;
        // ASCII/Kitty still redraw fully.
        self.width = width;
        self.height = height;

        // render_output fills self.render_buf in place — no allocation here.
        // `false` = unchanged halfblock frame: nothing to write.
        if self.render_output(rgba) {
            // Single write_all for the entire frame — avoids many small write syscalls.
            self.stdout.write_all(self.render_buf.as_bytes())?;
            self.stdout.flush()?;
        }

        Ok(())
    }

    fn get_size(&self, _monitor: usize) -> (u32, u32) {
        (self.width, self.height)
    }

    fn on_vt_switch(&mut self, _switch_in: bool) -> io::Result<()> {
        // No-op for terminal output
        Ok(())
    }

    fn on_resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        // The terminal reflowed/cleared: the remembered frame is wrong.
        self.halfblock.invalidate();
        // Re-size the render scratch buffer to match the new terminal size.
        let needed = cols as usize * rows as usize * 48;
        if self.render_buf.capacity() < needed {
            self.render_buf.reserve(needed - self.render_buf.capacity());
        }
    }
}

impl Drop for TerminalOutput {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = execute!(self.stdout, cursor::Show, terminal::LeaveAlternateScreen,);
    }
}
