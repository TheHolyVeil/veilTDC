//! Drawing helpers shared by the bar widgets, so every widget sizes and
//! pads itself from the same [`Metrics`].

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tiny_skia::{Color, Pixmap};
use veil_text::{Role, TextSystem};
use veil_ui::{fill_rounded_rect, Metrics, Theme};

/// Colour for a usage percentage: normal, warning, error.
pub fn level_color(theme: &Theme, pct: f32, warn_at: f32, err_at: f32) -> Color {
    if pct > err_at {
        theme.err
    } else if pct > warn_at {
        theme.warn
    } else {
        theme.text_dim
    }
}

/// A one-line "icon + value" widget body in the secondary text size.
/// Returns the width the widget takes, padding included.
#[allow(clippy::too_many_arguments)]
pub fn label(
    pixmap: &mut Pixmap,
    text: &mut TextSystem,
    m: &Metrics,
    x: f32,
    y: f32,
    h: f32,
    s: &str,
    color: Color,
) -> f32 {
    let ty = y + (h - m.small) / 2.0;
    let drawn = text.draw(pixmap, s, x + m.pad * 0.75, ty, Role::UI, m.small, color);
    drawn + m.pad * 1.5
}

/// A clickable glyph button: a rounded pill behind one glyph and an optional
/// short label. Returns its width.
#[allow(clippy::too_many_arguments)]
pub fn button(
    pixmap: &mut Pixmap,
    text: &mut TextSystem,
    theme: &Theme,
    m: &Metrics,
    x: f32,
    y: f32,
    h: f32,
    s: &str,
    fg: Color,
    pill: bool,
) -> f32 {
    let (tw, _) = text.measure(s, Role::UI, m.font);
    let w = tw + m.pad * 2.0;
    if pill {
        let v = m.px(4.0);
        fill_rounded_rect(pixmap, x + m.px(2.0), y + v, w - m.px(4.0), h - v * 2.0, m.radius, theme.surface_hi);
    }
    let ty = y + (h - m.font) / 2.0;
    text.draw(pixmap, s, x + m.pad, ty, Role::UI, m.font, fg);
    w
}

/// Output cap for [`run_capture`]; a runaway command cannot fill memory.
const MAX_CAPTURE: u64 = 64 * 1024;

/// Run a program and capture its stdout, giving up after `timeout`.
///
/// `None` means it could not start, exited non-zero, or was killed for
/// running too long. The child gets its own process group, so on timeout
/// everything it started dies with it. Meant for sampler threads, never the
/// render loop.
pub fn run_capture(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(MAX_CAPTURE).read_to_end(&mut buf);
        let _ = tx.send(buf);
        // Keep reading and throw the rest away: stopping would close the
        // pipe and the command would die of SIGPIPE, turning "too chatty"
        // into "failed".
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(15)),
            _ => break None,
        }
    };
    let Some(status) = status else {
        let pid = child.id() as i32;
        // SAFETY: plain syscall; a negative pid addresses the child's group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    if !status.success() {
        return None;
    }
    let bytes = rx.recv_timeout(Duration::from_millis(500)).unwrap_or_default();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `sh -c <command>`, see [`run_capture`].
pub fn run_shell(command: &str, timeout: Duration) -> Option<String> {
    run_capture("sh", &["-c", command], timeout)
}

/// Shorten to at most `max` characters with an ellipsis.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_short_strings_and_marks_long_ones() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello world", 6), "hello…");
        assert_eq!(truncate_chars("ünïcödé-ünïcödé", 5).chars().count(), 5);
        assert_eq!(truncate_chars("x", 0), "…");
    }

    #[test]
    fn level_colors_pick_by_threshold() {
        let t = Theme::default();
        assert_eq!(level_color(&t, 10.0, 50.0, 80.0), t.text_dim);
        assert_eq!(level_color(&t, 60.0, 50.0, 80.0), t.warn);
        assert_eq!(level_color(&t, 90.0, 50.0, 80.0), t.err);
    }

    #[test]
    fn run_shell_captures_stdout() {
        assert_eq!(run_shell("echo hi", Duration::from_secs(2)).as_deref(), Some("hi\n"));
    }

    #[test]
    fn failing_and_missing_commands_give_none() {
        assert_eq!(run_shell("exit 3", Duration::from_secs(2)), None);
        assert_eq!(run_capture("definitely-not-a-program-xyz", &[], Duration::from_secs(1)), None);
    }

    #[test]
    fn a_hung_command_is_killed_at_the_timeout() {
        let start = Instant::now();
        assert_eq!(run_shell("sleep 30", Duration::from_millis(300)), None);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn output_is_capped() {
        let out = run_shell("head -c 200000 /dev/zero | tr '\\0' x", Duration::from_secs(5)).unwrap();
        assert_eq!(out.len() as u64, MAX_CAPTURE);
    }
}
