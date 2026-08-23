//! Terminal output backend: Kitty graphics protocol, halfblock, or ASCII.
//!
//! Wraps the existing veil-render functions (rgba_to_halfblocks, compute_luma, etc.)
//! and renders to stdout via escape codes.

use std::io::{self, Write};
use std::fmt::Write as _;
use crossterm::{cursor, execute, terminal::{self, ClearType}};
use veil_render::{rgba_to_halfblocks, compute_luma, luma_to_chars, apply_hysteresis, render_kitty_frame};
use veil_gpu::GpuEncoder;

use super::OutputBackend;

#[derive(Copy, Clone, Debug)]
pub enum TerminalMode {
    Kitty,
    Halfblock,
    Ascii,
    AsciiEdge,
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
    /// Pre-allocated render scratch buffer reused across frames to avoid
    /// per-frame heap allocation in the hot render path.
    render_buf: String,
}

impl TerminalOutput {
    pub fn new() -> io::Result<Self> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
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

        // Detect terminal capabilities for render mode
        let mode = Self::detect_mode();

        // Try to init GPU encoder (only useful for halfblock/ascii modes)
        let gpu = if matches!(mode, TerminalMode::Kitty) {
            None
        } else {
            GpuEncoder::new()
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

    /// Detect best render mode based on terminal capabilities.
    fn detect_mode() -> TerminalMode {
        let term = std::env::var("TERM").unwrap_or_default();
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();

        if term == "xterm-kitty" {
            return TerminalMode::Kitty;
        }
        if let Ok(prog) = std::env::var("TERM_PROGRAM") {
            if prog == "WezTerm" || prog.to_lowercase().contains("wezterm") {
                return TerminalMode::Kitty;
            }
        }

        if colorterm == "truecolor" || colorterm == "24bit" {
            return TerminalMode::Halfblock;
        }

        TerminalMode::Ascii
    }

    /// Renders directly into `self.render_buf`. No return value — the caller
    /// reads `self.render_buf` afterward. This avoids the `.to_vec()` clone
    /// that previously threw away the whole point of the reusable buffer.
    fn render_output(&mut self, rgba: &[u8]) {
        let cols = self.cols;
        let rows = self.rows;
        let usable_rows = rows.saturating_sub(1);

        // Reuse the pre-allocated scratch buffer — clear without freeing.
        self.render_buf.clear();
        let out = &mut self.render_buf;
        let _ = write!(out, "\x1b[H");

        match self.mode {
            TerminalMode::Kitty => {
                out.push_str(&render_kitty_frame(rgba, self.width, self.height, cols, usable_rows));
            }
            TerminalMode::Halfblock => {
                let cells = if let Some(ref g) = self.gpu {
                    g.encode_halfblock(rgba, self.width, self.height, cols, usable_rows)
                } else {
                    rgba_to_halfblocks(rgba, self.width, self.height, cols, usable_rows)
                };
                Self::emit_halfblocks(out, &cells, cols, usable_rows);
            }
            TerminalMode::Ascii => {
                let luma = if let Some(ref g) = self.gpu {
                    g.encode_luma(rgba, self.width, self.height, cols, usable_rows)
                } else {
                    compute_luma(rgba, self.width, self.height, cols, usable_rows)
                };
                let chars = luma_to_chars(&luma, cols, usable_rows);
                Self::emit_chars_vec(out, &chars, cols, usable_rows);
            }
            TerminalMode::AsciiEdge => {
                let luma = if let Some(ref g) = self.gpu {
                    g.encode_luma(rgba, self.width, self.height, cols, usable_rows)
                } else {
                    compute_luma(rgba, self.width, self.height, cols, usable_rows)
                };
                if self.stable_luma.len() != luma.len() {
                    self.stable_luma = luma.clone();
                }
                apply_hysteresis(&mut self.stable_luma, &luma, 10);
                let chars = luma_to_chars(&self.stable_luma, cols, usable_rows);
                Self::emit_chars_vec(out, &chars, cols, usable_rows);
            }
        }
    }

    fn emit_halfblocks(out: &mut String, cells: &[veil_render::ColorCell], cols: u16, rows: u16) {
        for row in 0..rows as usize {
            if row > 0 {
                let _ = write!(out, "\r\n");
            }
            for col in 0..cols as usize {
                let idx = row * cols as usize + col;
                if idx < cells.len() {
                    let fg = cells[idx].fg;
                    let bg = cells[idx].bg;
                    let _ = write!(
                        out,
                        "\x1b[38;2;{};{};{};48;2;{};{};{}m▀",
                        fg[0], fg[1], fg[2],
                        bg[0], bg[1], bg[2]
                    );
                }
            }
        }
        let _ = write!(out, "\x1b[0m");
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
    fn render_frame(&mut self, rgba: &[u8], width: u32, height: u32, _damage: crate::layout::Rect) -> io::Result<()> {
        // _damage unused: halfblock/ascii/kitty encoding walks the full cell
        // grid every call today (see render_output below), no partial-region
        // path yet. That's real, separate follow-up work — not silently
        // dropped, just not part of this pass. Full redraw every tick, same
        // behavior as before this parameter existed.
        self.width = width;
        self.height = height;

        // render_output fills self.render_buf in place — no allocation here.
        self.render_output(rgba);
        // Single write_all for the entire frame — avoids many small write syscalls.
        self.stdout.write_all(self.render_buf.as_bytes())?;
        self.stdout.flush()?;

        Ok(())
    }

    fn get_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn on_vt_switch(&mut self, _switch_in: bool) -> io::Result<()> {
        // No-op for terminal output
        Ok(())
    }

    fn on_resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
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
        let _ = execute!(
            self.stdout,
            cursor::Show,
            terminal::LeaveAlternateScreen,
        );
    }
}
