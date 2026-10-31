//! Bar widget modules.

pub mod activities;
pub mod audio;
pub mod battery;
pub mod clock;
pub mod control_center;
pub mod cpu;
pub mod custom;
pub mod disk;
pub mod media;
pub mod memory;
pub mod network;
pub mod notifications;
pub mod systray;
pub mod temp;
pub mod weather;
pub mod window_title;
pub mod workspaces;

pub use activities::ActivitiesWidget;
pub use audio::AudioWidget;
pub use battery::BatteryWidget;
pub use clock::ClockWidget;
pub use control_center::ControlCenterWidget;
pub use cpu::CpuWidget;
pub use custom::CustomWidget;
pub use disk::DiskWidget;
pub use media::MediaWidget;
pub use memory::MemoryWidget;
pub use network::NetworkWidget;
pub use notifications::NotificationsWidget;
pub use systray::SystrayWidget;
pub use temp::TempWidget;
pub use weather::WeatherWidget;
pub use window_title::WindowTitleWidget;
pub use workspaces::WorkspacesWidget;
