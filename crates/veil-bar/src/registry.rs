//! Widget names from the config to widget instances.

use veil_ipc::BarSettings;
use veil_ui::Widget;

use crate::widgets::*;

/// Every built-in widget name (`custom:<name>` is the open-ended one).
pub const BUILTIN: [&str; 16] = [
    "workspaces",
    "window_title",
    "clock",
    "battery",
    "cpu",
    "memory",
    "disk",
    "temp",
    "network",
    "audio",
    "media",
    "weather",
    "systray",
    "notifications",
    "control_center",
    "activities",
];

/// The settings a widget was built from. When a config reload leaves this
/// unchanged the running instance is kept, so a reload does not restart its
/// sampler threads or re-register on D-Bus.
pub fn options_key(name: &str, s: &BarSettings) -> String {
    match name {
        "clock" => s.clock_format.clone(),
        "disk" => s.disk_mount.clone(),
        "weather" => format!("{:?}", s.weather),
        n => match n.strip_prefix("custom:") {
            Some(c) => s
                .custom
                .iter()
                .find(|w| w.name == c)
                .map(|w| format!("{}\u{1f}{}", w.command, w.interval))
                .unwrap_or_default(),
            None => String::new(),
        },
    }
}

/// Build the widget called `name`. `Err` carries why it was left out; the
/// bar logs it and carries on without that widget.
pub fn build(name: &str, s: &BarSettings) -> Result<Box<dyn Widget>, String> {
    Ok(match name {
        "workspaces" => Box::new(WorkspacesWidget::new()),
        "window_title" => Box::new(WindowTitleWidget::new()),
        "clock" => Box::new(ClockWidget::with_format(&s.clock_format)),
        "battery" => Box::new(BatteryWidget::new()),
        "cpu" => Box::new(CpuWidget::new()),
        "memory" => Box::new(MemoryWidget::new()),
        "disk" => Box::new(DiskWidget::new(&s.disk_mount)),
        "temp" => Box::new(TempWidget::new()),
        "network" => Box::new(NetworkWidget::new()),
        "audio" => Box::new(AudioWidget::new()),
        "media" => Box::new(MediaWidget::new()),
        "weather" => match &s.weather {
            Some(w) => Box::new(WeatherWidget::new(w)),
            None => return Err("weather needs bar.widgets.weather = { lat = ..., lon = ... }".into()),
        },
        "systray" => Box::new(SystrayWidget::new()),
        "notifications" => Box::new(NotificationsWidget::new()),
        "control_center" => Box::new(ControlCenterWidget::new()),
        "activities" => Box::new(ActivitiesWidget::new()),
        other => match other.strip_prefix("custom:") {
            Some(c) => match s.custom.iter().find(|w| w.name == c) {
                Some(w) => Box::new(CustomWidget::new(&w.name, &w.command, w.interval)),
                None => return Err(format!("no bar.widgets.custom entry named '{c}'")),
            },
            None => return Err(format!("unknown widget '{other}'")),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use veil_ipc::{CustomWidget as CustomCfg, WeatherSettings};

    fn settings() -> BarSettings {
        BarSettings {
            weather: Some(WeatherSettings { lat: 1.0, lon: 2.0, units: "metric".into() }),
            custom: vec![CustomCfg { name: "up".into(), command: "true".into(), interval: 30 }],
            ..Default::default()
        }
    }

    #[test]
    fn every_builtin_name_builds_and_reports_its_own_name() {
        let s = settings();
        // media and systray talk to the session bus (the tray would even
        // claim the StatusNotifierWatcher name on the real desktop), so unit
        // tests leave them out; their pure parts are tested in their modules.
        for name in BUILTIN.into_iter().filter(|n| !matches!(*n, "media" | "systray")) {
            let w = build(name, &s).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(w.name(), name, "registry name and widget name must agree");
        }
        assert_eq!(build("custom:up", &s).unwrap().name(), "custom:up");
    }

    #[test]
    fn bad_names_are_errors_not_panics() {
        let s = BarSettings::default();
        assert!(build("nonsense", &s).is_err());
        assert!(build("custom:missing", &s).is_err());
        assert!(build("weather", &s).err().expect("an error").contains("lat"), "weather without coordinates explains itself");
    }

    #[test]
    fn options_key_changes_only_with_the_widgets_own_settings() {
        let mut s = settings();
        let before = (options_key("clock", &s), options_key("disk", &s), options_key("custom:up", &s), options_key("cpu", &s));
        s.clock_format = "%H:%M".into();
        assert_ne!(options_key("clock", &s), before.0);
        assert_eq!(options_key("disk", &s), before.1);
        assert_eq!(options_key("custom:up", &s), before.2);
        s.custom[0].interval = 5;
        assert_ne!(options_key("custom:up", &s), before.2);
        assert_eq!(options_key("cpu", &s), before.3);
    }
}
