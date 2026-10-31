//! Bar layout: config-driven sections, scaling, rendering and hit-testing.

use std::collections::HashMap;
use std::time::Instant;
use tiny_skia::Pixmap;
use veil_ipc::BarSettings;
use veil_text::{FontConfig, TextSystem};
use veil_ui::{clear, fill_rect, Action, Metrics, Theme, Widget};

use crate::registry;
use crate::widgets::{
    AudioWidget, NotificationsWidget, WindowTitleWidget, WorkspacesWidget,
};

const LEFT: usize = 0;
const CENTER: usize = 1;
const RIGHT: usize = 2;

struct Entry {
    name: String,
    /// What the widget was built from; see [`registry::options_key`].
    opts: String,
    widget: Box<dyn Widget>,
}

/// Where a widget was drawn on one output, for clicks.
struct Hit {
    section: usize,
    idx: usize,
    x0: f32,
    x1: f32,
}

pub struct BarLayout {
    sections: [Vec<Entry>; 3],
    pub theme: Theme,
    pub text: TextSystem,
    settings: BarSettings,
    metrics: Metrics,
    /// Workspace state per monitor (index == compositor monitor index): each
    /// monitor has its own workspaces, so each bar draws its own.
    ws: Vec<(usize, Vec<usize>)>,
    title: String,
    volume: (u8, bool),
    notifications: (u32, bool),
    hits: HashMap<String, Vec<Hit>>,
}

impl Default for BarLayout {
    fn default() -> Self {
        Self::new(36)
    }
}

impl BarLayout {
    /// A bar with the default sections at `height` px.
    pub fn new(height: u32) -> Self {
        Self::with_settings(BarSettings { height, ..Default::default() }.sanitized(), &FontConfig::default())
    }

    pub fn with_settings(settings: BarSettings, font: &FontConfig) -> Self {
        let mut bar = Self {
            sections: [Vec::new(), Vec::new(), Vec::new()],
            theme: Theme::default(),
            text: TextSystem::new(font),
            settings: settings.clone(),
            metrics: Metrics::from_font_size(font.size),
            ws: Vec::new(),
            title: String::new(),
            volume: (50, false),
            notifications: (0, false),
            hits: HashMap::new(),
        };
        bar.apply_settings(settings);
        bar
    }

    // ─── configuration ──────────────────────────────────────────────────────

    /// Rebuild the sections from new settings. A widget that is still in the
    /// bar with unchanged options is kept as it is, so a reload does not
    /// restart samplers or drop D-Bus connections.
    pub fn apply_settings(&mut self, settings: BarSettings) {
        let settings = settings.sanitized();
        let mut old: HashMap<String, Vec<Entry>> = HashMap::new();
        for e in self.sections.iter_mut().flat_map(|s| s.drain(..)) {
            old.entry(e.name.clone()).or_default().push(e);
        }

        let wanted = [&settings.left, &settings.center, &settings.right];
        for (si, names) in wanted.into_iter().enumerate() {
            for name in names {
                let opts = registry::options_key(name, &settings);
                let reuse = old.get_mut(name).and_then(|v| {
                    v.iter().position(|e| e.opts == opts).map(|i| v.remove(i))
                });
                let entry = match reuse {
                    Some(e) => e,
                    None => match registry::build(name, &settings) {
                        Ok(widget) => Entry { name: name.clone(), opts, widget },
                        Err(why) => {
                            tracing::warn!("bar: skipping widget '{name}': {why}");
                            continue;
                        }
                    },
                };
                self.sections[si].push(entry);
            }
        }
        self.settings = settings;
        self.inject_state();
    }

    /// Reload fonts. Returns true if anything changed (the bar must redraw,
    /// and its height may have changed with the size).
    pub fn set_font(&mut self, fc: &FontConfig) -> bool {
        let reloaded = self.text.reload(fc);
        let metrics = Metrics::from_font_size(fc.size);
        let resized = metrics != self.metrics;
        self.metrics = metrics;
        reloaded || resized
    }

    /// Bar height in px: the configured height scaled by `font.size`.
    pub fn height(&self) -> u32 {
        self.metrics.bar_height(self.settings.height)
    }

    pub fn at_top(&self) -> bool {
        self.settings.at_top()
    }

    pub fn settings(&self) -> &BarSettings {
        &self.settings
    }

    /// Widget names per section, for tests and logging.
    pub fn layout_names(&self) -> [Vec<String>; 3] {
        [0, 1, 2].map(|i| self.sections[i].iter().map(|e| e.name.clone()).collect())
    }

    // ─── state from IPC ─────────────────────────────────────────────────────

    pub fn update(&mut self, now: Instant) -> bool {
        let mut redraw = false;
        for e in self.sections.iter_mut().flatten() {
            redraw |= e.widget.update(now);
        }
        redraw
    }

    pub fn on_workspaces(&mut self, monitors: Vec<(usize, Vec<usize>)>) {
        self.ws = monitors;
    }

    pub fn on_focus(&mut self, title: String) {
        self.title = title;
        self.inject_state();
    }

    pub fn on_volume(&mut self, volume: u8, muted: bool) {
        self.volume = (volume, muted);
        self.inject_state();
    }

    pub fn on_notifications(&mut self, count: u32, dnd: bool) {
        self.notifications = (count, dnd);
        self.inject_state();
    }

    /// Push the event-driven state into whichever widgets show it, including
    /// ones that were just rebuilt.
    fn inject_state(&mut self) {
        for e in self.sections.iter_mut().flatten() {
            let any = e.widget.as_any_mut();
            if let Some(w) = any.downcast_mut::<WindowTitleWidget>() {
                w.title = self.title.clone();
            } else if let Some(w) = any.downcast_mut::<AudioWidget>() {
                (w.volume, w.muted) = self.volume;
            } else if let Some(w) = any.downcast_mut::<NotificationsWidget>() {
                (w.count, w.dnd) = self.notifications;
            }
        }
    }

    /// Monitor index behind a wl_output name. Veil names them `veil-host-<n>`;
    /// anything else (or no name) is monitor 0.
    fn monitor_of(output_name: &str) -> usize {
        output_name
            .strip_prefix("veil-host-")
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    }

    // ─── rendering ──────────────────────────────────────────────────────────

    /// Render the bar for the monitor behind `output_name`, using that
    /// monitor's own workspace state.
    pub fn render_for(&mut self, output_name: &str, width: u32, height: u32) -> Vec<u8> {
        if let Some((active, occupied)) = self.ws.get(Self::monitor_of(output_name)).cloned() {
            for e in self.sections.iter_mut().flatten() {
                if let Some(ws) = e.widget.as_any_mut().downcast_mut::<WorkspacesWidget>() {
                    ws.active = active;
                    ws.occupied = occupied.clone();
                }
            }
        }
        let (pixels, hits) = self.render_with_hits(width, height);
        self.hits.insert(output_name.to_string(), hits);
        pixels
    }

    /// Render entire bar into an ARGB8888 byte vector suitable for Wayland SHM commit.
    pub fn render(&mut self, width: u32, height: u32) -> Vec<u8> {
        self.render_with_hits(width, height).0
    }

    fn render_with_hits(&mut self, width: u32, height: u32) -> (Vec<u8>, Vec<Hit>) {
        let Some(mut pixmap) = Pixmap::new(width, height) else {
            return (Vec::new(), Vec::new());
        };
        let m = self.metrics;
        let h = height as f32;
        let w = width as f32;

        clear(&mut pixmap, self.theme.bg);
        // A border on the edge that faces the screen.
        let border_y = if self.at_top() { h - 1.0 } else { 0.0 };
        fill_rect(&mut pixmap, 0.0, border_y, w, 1.0, self.theme.border);

        // Measure with a scratch pixmap; widgets must not depend on its size.
        let mut scratch = Pixmap::new(1, 1).expect("1x1 pixmap");
        let mut widths: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for si in 0..3 {
            for e in &mut self.sections[si] {
                widths[si].push(e.widget.draw(&mut scratch, &mut self.text, &self.theme, &m, 0.0, 0.0, h));
            }
        }
        let total = |ws: &Vec<f32>| -> f32 {
            let shown: Vec<_> = ws.iter().filter(|a| **a > 0.0).collect();
            shown.iter().copied().sum::<f32>() + m.gap * shown.len().saturating_sub(1) as f32
        };

        let left_start = m.pad;
        let left_end = left_start + total(&widths[LEFT]);
        let center_start = ((w - total(&widths[CENTER])) / 2.0).max(left_end + m.gap);
        let center_end = center_start + total(&widths[CENTER]);
        let right_start = (w - total(&widths[RIGHT]) - m.pad).max(center_end + m.gap);

        let mut hits = Vec::new();
        for (si, start) in [(LEFT, left_start), (CENTER, center_start), (RIGHT, right_start)] {
            let mut x = start;
            for (idx, e) in self.sections[si].iter_mut().enumerate() {
                if widths[si][idx] <= 0.0 {
                    continue; // hidden right now (no player, nothing to show)
                }
                let adv = e.widget.draw(&mut pixmap, &mut self.text, &self.theme, &m, x, 0.0, h);
                hits.push(Hit { section: si, idx, x0: x, x1: x + adv });
                x += adv + m.gap;
            }
        }
        (pixmap.take(), hits)
    }

    // ─── clicks ─────────────────────────────────────────────────────────────

    fn hit_at(&self, output: &str, x: f32) -> Option<&Hit> {
        self.hits.get(output)?.iter().find(|h| x >= h.x0 && x < h.x1)
    }

    /// A left click at `x` on `output`'s bar.
    pub fn on_press(&mut self, output: &str, x: f32) -> Option<Action> {
        let (section, idx, x0) = self.hit_at(output, x).map(|h| (h.section, h.idx, h.x0))?;
        self.sections[section][idx].widget.on_click(x - x0)
    }

    /// A scroll at `x`; negative `dy` is up.
    pub fn on_scroll(&mut self, output: &str, x: f32, dy: f32) -> Option<Action> {
        let (section, idx) = self.hit_at(output, x).map(|h| (h.section, h.idx))?;
        self.sections[section][idx].widget.on_scroll(dy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veil_ipc::CustomWidget as CustomCfg;

    #[test]
    fn bar_layout_render_produces_non_empty_buffer() {
        let mut bar = BarLayout::new(36);
        let pixels = bar.render(1920, 36);
        assert_eq!(pixels.len(), 1920 * 36 * 4);
        assert!(pixels.iter().any(|&b| b > 0));
    }

    #[test]
    fn sections_come_from_the_settings() {
        let s = BarSettings {
            left: vec!["activities".into(), "workspaces".into()],
            center: vec![],
            right: vec!["clock".into(), "bogus".into(), "disk".into()],
            ..Default::default()
        };
        let bar = BarLayout::with_settings(s, &FontConfig::default());
        let [l, c, r] = bar.layout_names();
        assert_eq!(l, ["activities", "workspaces"]);
        assert!(c.is_empty());
        assert_eq!(r, ["clock", "disk"], "an unknown widget is skipped, not fatal");
    }

    #[test]
    fn height_scales_with_font_size_and_comes_from_config() {
        let s = BarSettings { height: 40, ..Default::default() };
        let mut bar = BarLayout::with_settings(s, &FontConfig::default());
        assert_eq!(bar.height(), 40);
        let big = FontConfig { size: 28.0, ..FontConfig::default() };
        assert!(bar.set_font(&big), "a new size counts as a change");
        assert_eq!(bar.height(), 80);
        assert!(!bar.set_font(&big), "the same font again changes nothing");
    }

    #[test]
    fn reload_keeps_widgets_whose_options_did_not_change() {
        let mut bar = BarLayout::new(36);
        let before = bar.sections[CENTER][0].widget.as_ref() as *const dyn Widget as *const u8;
        let mut s = bar.settings().clone();
        s.height = 44; // not a widget option
        bar.apply_settings(s);
        let same = bar.sections[CENTER][0].widget.as_ref() as *const dyn Widget as *const u8;
        assert_eq!(before, same, "the clock instance survived the reload");

        let mut s = bar.settings().clone();
        s.clock_format = "%H:%M".into(); // the clock's own option
        bar.apply_settings(s);
        let rebuilt = bar.sections[CENTER][0].widget.as_ref() as *const dyn Widget as *const u8;
        assert_ne!(before, rebuilt, "a changed option rebuilds that widget");
    }

    #[test]
    fn event_state_reaches_rebuilt_widgets() {
        let mut bar = BarLayout::new(36);
        bar.on_focus("my window".into());
        bar.on_volume(80, true);
        let mut s = bar.settings().clone();
        s.left = vec!["window_title".into()];
        s.right = vec!["audio".into(), "notifications".into()];
        bar.on_notifications(4, false);
        bar.apply_settings(s);
        let find = |bar: &mut BarLayout, name: &str| -> usize {
            bar.sections.iter().flatten().position(|e| e.name == name).unwrap()
        };
        let i = find(&mut bar, "window_title");
        let w = bar.sections.iter_mut().flatten().nth(i).unwrap();
        assert_eq!(w.widget.as_any_mut().downcast_mut::<WindowTitleWidget>().unwrap().title, "my window");
        let i = find(&mut bar, "audio");
        let w = bar.sections.iter_mut().flatten().nth(i).unwrap();
        let a = w.widget.as_any_mut().downcast_mut::<AudioWidget>().unwrap();
        assert_eq!((a.volume, a.muted), (80, true));
        let i = find(&mut bar, "notifications");
        let w = bar.sections.iter_mut().flatten().nth(i).unwrap();
        assert_eq!(w.widget.as_any_mut().downcast_mut::<NotificationsWidget>().unwrap().count, 4);
    }

    #[test]
    fn clicks_reach_the_widget_under_the_pointer() {
        let s = BarSettings {
            left: vec!["workspaces".into()],
            center: vec![],
            right: vec!["audio".into()],
            ..Default::default()
        };
        let mut bar = BarLayout::with_settings(s, &FontConfig::default());
        bar.render_for("veil-host-0", 800, 36);
        // The workspaces pill row starts at the left padding.
        let act = bar.on_press("veil-host-0", 8.0 + 35.0).expect("workspace click");
        assert_eq!(act, Action::Ipc { cmd: "workspace.switch".into(), args: serde_json::json!({ "n": 2 }) });
        // Far right, inside the audio widget: mute toggle.
        let act = bar.on_press("veil-host-0", 790.0).expect("audio click");
        assert!(matches!(act, Action::Ipc { ref cmd, .. } if cmd == "volume.step"));
        // Empty space between the sections hits nothing.
        assert!(bar.on_press("veil-host-0", 400.0).is_none());
        // An output that never rendered has no hit map.
        assert!(bar.on_press("veil-host-9", 10.0).is_none());
    }

    #[test]
    fn scrolling_over_audio_changes_the_volume() {
        let s = BarSettings { left: vec![], center: vec![], right: vec!["audio".into()], ..Default::default() };
        let mut bar = BarLayout::with_settings(s, &FontConfig::default());
        bar.render_for("veil-host-0", 800, 36);
        let up = bar.on_scroll("veil-host-0", 790.0, -1.0).unwrap();
        assert_eq!(up, Action::Ipc { cmd: "volume.step".into(), args: serde_json::json!({ "delta": 5 }) });
        let down = bar.on_scroll("veil-host-0", 790.0, 1.0).unwrap();
        assert_eq!(down, Action::Ipc { cmd: "volume.step".into(), args: serde_json::json!({ "delta": -5 }) });
    }

    #[test]
    fn custom_widgets_resolve_through_the_settings() {
        let s = BarSettings {
            center: vec!["custom:hi".into(), "custom:missing".into()],
            custom: vec![CustomCfg { name: "hi".into(), command: "echo hi".into(), interval: 30 }],
            ..Default::default()
        };
        let bar = BarLayout::with_settings(s, &FontConfig::default());
        assert_eq!(bar.layout_names()[CENTER], ["custom:hi"]);
    }

    #[test]
    fn a_bottom_bar_puts_its_border_on_top() {
        let top = BarSettings { left: vec![], center: vec![], right: vec![], ..Default::default() };
        let bottom = BarSettings { position: "bottom".into(), ..top.clone() };
        let mut a = BarLayout::with_settings(top, &FontConfig::default());
        let mut b = BarLayout::with_settings(bottom, &FontConfig::default());
        let (pa, pb) = (a.render(40, 36), b.render(40, 36));
        let row = |p: &Vec<u8>, y: usize| p[y * 40 * 4..(y + 1) * 40 * 4].to_vec();
        assert_eq!(row(&pa, 35), row(&pb, 0), "the border row moves to the screen-facing edge");
        assert_ne!(row(&pa, 0), row(&pa, 35));
    }
}

#[cfg(test)]
mod monitor_tests {
    use super::BarLayout;

    #[test]
    fn monitor_index_comes_from_the_veil_output_name() {
        assert_eq!(BarLayout::monitor_of("veil-host-0"), 0);
        assert_eq!(BarLayout::monitor_of("veil-host-1"), 1);
        // Not ours (another compositor, or no name): monitor 0.
        assert_eq!(BarLayout::monitor_of("DP-2"), 0);
        assert_eq!(BarLayout::monitor_of(""), 0);
    }

    #[test]
    fn each_monitor_draws_its_own_workspace_state() {
        let mut bar = BarLayout::new(36);
        bar.on_workspaces(vec![(1, vec![1]), (3, vec![3, 4])]);
        let a = bar.render_for("veil-host-0", 400, 36);
        let b = bar.render_for("veil-host-1", 400, 36);
        assert!(!a.is_empty() && !b.is_empty());
        assert_ne!(a, b, "monitor 1 must not render monitor 0's workspaces");
    }
}
