//! Disk usage of one mount point, sampled once a minute.

use std::ffi::CString;
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use veil_ui::{Metrics, Theme, Widget};

use crate::poll::{Poller, Seen};
use crate::util::{label, level_color};

pub struct DiskWidget {
    poll: Poller<Option<(f32, f32)>>,
    seen: Seen,
}

impl DiskWidget {
    pub fn new(mount: &str) -> Self {
        let mount = mount.to_string();
        // `statvfs` on a mount that went away (a dead NFS share) can block
        // for a long time, so it never runs on the render thread.
        let poll = Poller::every("disk", Duration::from_secs(60), move || statvfs_gb(&mount));
        Self { poll, seen: Seen::default() }
    }
}

impl Widget for DiskWidget {
    fn name(&self) -> &str {
        "disk"
    }

    fn update(&mut self, _now: Instant) -> bool {
        self.seen.changed(&self.poll)
    }

    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &mut TextSystem,
        theme: &Theme,
        m: &Metrics,
        x: f32,
        y: f32,
        h: f32,
    ) -> f32 {
        let Some(Some((used, total))) = self.poll.latest() else {
            return 0.0;
        };
        let color = level_color(theme, usage_pct(used, total), 75.0, 90.0);
        label(pixmap, text, m, x, y, h, &format_usage(used, total), color)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

pub fn usage_pct(used: f32, total: f32) -> f32 {
    used / total.max(0.001) * 100.0
}

pub fn format_usage(used: f32, total: f32) -> String {
    format!("󰋊 {used:.0}/{total:.0}G")
}

/// (used GB, total GB) for the filesystem at `path`.
fn statvfs_gb(path: &str) -> Option<(f32, f32)> {
    let cpath = CString::new(path).ok()?;
    // SAFETY: `st` is plain data the syscall fills in, `cpath` is NUL-terminated.
    let st = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(cpath.as_ptr(), &mut st) != 0 {
            return None;
        }
        st
    };
    let block = st.f_frsize as f64;
    let total = st.f_blocks as f64 * block / 1e9;
    let avail = st.f_bavail as f64 * block / 1e9;
    Some(((total - avail) as f32, total as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_has_a_sane_size() {
        let (used, total) = statvfs_gb("/").expect("statvfs /");
        assert!(total > 0.0 && used >= 0.0 && used <= total);
    }

    #[test]
    fn a_missing_mount_gives_none() {
        assert_eq!(statvfs_gb("/definitely/not/a/mount"), None);
    }

    #[test]
    fn formatting_and_percentage() {
        assert_eq!(format_usage(120.4, 500.0), "󰋊 120/500G");
        assert_eq!(usage_pct(250.0, 500.0), 50.0);
        assert!(usage_pct(1.0, 0.0).is_finite(), "an empty total does not divide by zero");
    }
}
