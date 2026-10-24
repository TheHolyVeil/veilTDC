//! Bar widget modules.

pub mod audio;
pub mod battery;
pub mod clock;
pub mod cpu;
pub mod memory;
pub mod window_title;
pub mod workspaces;

pub use audio::AudioWidget;
pub use battery::BatteryWidget;
pub use clock::ClockWidget;
pub use cpu::CpuWidget;
pub use memory::MemoryWidget;
pub use window_title::WindowTitleWidget;
pub use workspaces::WorkspacesWidget;
