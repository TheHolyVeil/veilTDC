//! Component supervisor and process lifecycle management.
//!
//! Supervises shell components (bar, notify, osd, launch, power, settings, lock)
//! as isolated child processes using `current_exe() shell <name>`.
//! Implements backoff, crash-loop detection, and watchdog health checks.

use std::collections::{HashMap, VecDeque};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    /// Started on session start, restarted on crash (bar, notify, osd).
    Persistent,
    /// Started on demand (launcher, power menu, settings, lock).
    OnDemand,
    /// Runs once and exits (screenshot).
    OneShot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorState {
    Disabled,
    Starting { spawned_at: Instant },
    Running { started_at: Instant, last_pong: Instant },
    Backoff { attempt: u32, until: Instant },
    Failed { reason: String },
    Stopped,
}

#[derive(Debug)]
pub struct ComponentRecord {
    pub name: String,
    pub kind: ComponentKind,
    pub state: SupervisorState,
    pub child: Option<Child>,
    pub pid: Option<u32>,
    /// Timestamps of crashes within the last 60 seconds.
    pub crash_history: VecDeque<Instant>,
}

impl ComponentRecord {
    pub fn new(name: impl Into<String>, kind: ComponentKind, enabled: bool) -> Self {
        Self {
            name: name.into(),
            kind,
            state: if enabled {
                SupervisorState::Stopped
            } else {
                SupervisorState::Disabled
            },
            child: None,
            pid: None,
            crash_history: VecDeque::new(),
        }
    }
}

pub struct Supervisor {
    components: HashMap<String, ComponentRecord>,
    exe_path: Option<PathBuf>,
    wayland_display: Option<String>,
    ipc_socket: Option<PathBuf>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub const START_TIMEOUT: Duration = Duration::from_secs(5);
    pub const PING_INTERVAL: Duration = Duration::from_secs(5);
    pub const PONG_TIMEOUT: Duration = Duration::from_secs(10);
    pub const CRASH_WINDOW: Duration = Duration::from_secs(60);
    pub const MAX_CRASHES: usize = 5;
    pub const STABILITY_RESET: Duration = Duration::from_secs(300); // 5 min
    pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

    pub fn new() -> Self {
        let mut sup = Self {
            components: HashMap::new(),
            exe_path: std::env::current_exe().ok(),
            wayland_display: None,
            ipc_socket: None,
        };
        sup.register_defaults();
        sup
    }

    pub fn set_wayland_display(&mut self, display: impl Into<String>) {
        self.wayland_display = Some(display.into());
    }

    pub fn set_ipc_socket(&mut self, path: PathBuf) {
        self.ipc_socket = Some(path);
    }

    fn register_defaults(&mut self) {
        self.register("bar", ComponentKind::Persistent, true);
        self.register("notify", ComponentKind::Persistent, true);
        self.register("osd", ComponentKind::Persistent, true);
        self.register("launch", ComponentKind::OnDemand, true);
        self.register("power", ComponentKind::OnDemand, true);
        self.register("settings", ComponentKind::OnDemand, true);
        self.register("lock", ComponentKind::OnDemand, true);
        self.register("screenshot", ComponentKind::OneShot, true);
    }

    pub fn register(&mut self, name: &str, kind: ComponentKind, enabled: bool) {
        self.components.insert(
            name.to_string(),
            ComponentRecord::new(name, kind, enabled),
        );
    }

    pub fn get(&self, name: &str) -> Option<&ComponentRecord> {
        self.components.get(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut ComponentRecord> {
        self.components.get_mut(name)
    }

    /// Compute backoff duration: 0.5s * 2^attempt capped at 30s.
    pub fn backoff_duration(attempt: u32) -> Duration {
        let millis = (500u64).saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX));
        let dur = Duration::from_millis(millis);
        if dur > Self::MAX_BACKOFF {
            Self::MAX_BACKOFF
        } else {
            dur
        }
    }

    /// Record a crash and determine if circuit breaker should trip.
    pub fn handle_crash(record: &mut ComponentRecord, now: Instant) {
        // Prune older than 60s
        while let Some(&first) = record.crash_history.front() {
            if now.saturating_duration_since(first) > Self::CRASH_WINDOW {
                record.crash_history.pop_front();
            } else {
                break;
            }
        }
        record.crash_history.push_back(now);

        if record.crash_history.len() >= Self::MAX_CRASHES {
            record.state = SupervisorState::Failed {
                reason: format!("Crash loop: {} crashes in under 60 seconds", record.crash_history.len()),
            };
        } else {
            let attempt = (record.crash_history.len() as u32).saturating_sub(1);
            let until = now + Self::backoff_duration(attempt);
            record.state = SupervisorState::Backoff { attempt, until };
        }
    }

    /// Component reported hello over IPC.
    pub fn on_hello(&mut self, name: &str, now: Instant) {
        if let Some(record) = self.components.get_mut(name) {
            if let SupervisorState::Starting { .. } = record.state {
                record.state = SupervisorState::Running {
                    started_at: now,
                    last_pong: now,
                };
            }
        }
    }

    /// Component replied with pong or IPC activity.
    pub fn on_pong(&mut self, name: &str, now: Instant) {
        if let Some(record) = self.components.get_mut(name) {
            if let SupervisorState::Running { ref mut last_pong, .. } = record.state {
                *last_pong = now;
            }
        }
    }

    /// Request to launch an on-demand component, or restart a persistent one.
    pub fn start_component(&mut self, name: &str, now: Instant) -> Result<(), String> {
        let exe = self.exe_path.clone().ok_or("No executable path known for veil")?;
        let wayland_display = self.wayland_display.clone();
        let ipc_socket = self.ipc_socket.clone();

        let record = self.components.get_mut(name).ok_or_else(|| format!("Unknown component: {name}"))?;
        if record.state == SupervisorState::Disabled {
            return Err(format!("Component {name} is disabled in configuration"));
        }

        // Spawn child
        let mut cmd = Command::new(&exe);
        cmd.arg("shell").arg(name);

        if let Some(display) = wayland_display {
            cmd.env("WAYLAND_DISPLAY", display);
        }
        if let Some(sock) = ipc_socket {
            cmd.env("VEIL_IPC_SOCKET", sock);
        }

        // Safety: PR_SET_PDEATHSIG ensures child dies when compositor dies
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }

        match cmd.spawn() {
            Ok(child) => {
                record.pid = Some(child.id());
                record.child = Some(child);
                record.state = SupervisorState::Starting { spawned_at: now };
                Ok(())
            }
            Err(e) => {
                let err_str = format!("Failed to spawn {name}: {e}");
                Self::handle_crash(record, now);
                Err(err_str)
            }
        }
    }

    /// Periodic tick: checks child exit status, startup timeouts, and ping watchdog.
    pub fn tick(&mut self, now: Instant) -> Vec<String> {
        let mut to_start = Vec::new();

        for (name, record) in self.components.iter_mut() {
            // Check if child process has exited
            if let Some(mut child) = record.child.take() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        record.pid = None;
                        if status.success() && record.kind != ComponentKind::Persistent {
                            // Clean exit of on-demand or one-shot
                            record.state = SupervisorState::Stopped;
                        } else {
                            // Abnormal exit or persistent component died
                            Self::handle_crash(record, now);
                        }
                    }
                    Ok(None) => {
                        // Still running
                        record.child = Some(child);
                    }
                    Err(_) => {
                        Self::handle_crash(record, now);
                    }
                }
            }

            // State-based watchdog transitions
            match record.state {
                SupervisorState::Starting { spawned_at } => {
                    if now.saturating_duration_since(spawned_at) > Self::START_TIMEOUT {
                        // Timed out waiting for hello
                        if let Some(mut child) = record.child.take() {
                            let _ = child.kill();
                        }
                        record.pid = None;
                        Self::handle_crash(record, now);
                    }
                }
                SupervisorState::Running { started_at, last_pong } => {
                    // Reset crash history after 5 minutes of stability
                    if now.saturating_duration_since(started_at) > Self::STABILITY_RESET && !record.crash_history.is_empty() {
                        record.crash_history.clear();
                    }

                    // Watchdog: check pong for persistent components
                    if record.kind == ComponentKind::Persistent && now.saturating_duration_since(last_pong) > Self::PONG_TIMEOUT {
                        if let Some(mut child) = record.child.take() {
                            let _ = child.kill();
                        }
                        record.pid = None;
                        Self::handle_crash(record, now);
                    }
                }
                SupervisorState::Backoff { until, .. } if now >= until => {
                    if record.kind == ComponentKind::Persistent {
                        to_start.push(name.clone());
                    } else {
                        record.state = SupervisorState::Stopped;
                    }
                }
                _ => {}
            }
        }

        to_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_exponential_growth_and_cap() {
        assert_eq!(Supervisor::backoff_duration(0), Duration::from_millis(500));
        assert_eq!(Supervisor::backoff_duration(1), Duration::from_millis(1000));
        assert_eq!(Supervisor::backoff_duration(2), Duration::from_millis(2000));
        assert_eq!(Supervisor::backoff_duration(3), Duration::from_millis(4000));
        assert_eq!(Supervisor::backoff_duration(6), Duration::from_millis(30000)); // Capped at 30s
    }

    #[test]
    fn circuit_breaker_trips_after_five_crashes() {
        let mut record = ComponentRecord::new("bar", ComponentKind::Persistent, true);
        let t0 = Instant::now();

        for i in 0..4 {
            let t = t0 + Duration::from_secs(i * 2);
            Supervisor::handle_crash(&mut record, t);
            assert!(matches!(record.state, SupervisorState::Backoff { .. }));
        }

        // 5th crash trips the circuit breaker
        let t5 = t0 + Duration::from_secs(10);
        Supervisor::handle_crash(&mut record, t5);
        assert!(matches!(record.state, SupervisorState::Failed { .. }));
    }

    #[test]
    fn stability_resets_crash_history() {
        let mut sup = Supervisor::new();
        let t0 = Instant::now();

        let rec = sup.get_mut("bar").unwrap();
        rec.crash_history.push_back(t0);
        rec.state = SupervisorState::Running {
            started_at: t0,
            last_pong: t0,
        };

        // Advance 4 minutes with regular pongs (not yet stable)
        let t_4m = t0 + Duration::from_secs(240);
        sup.on_pong("bar", t_4m);
        sup.tick(t_4m);
        assert_eq!(sup.get("bar").unwrap().crash_history.len(), 1);

        // Advance past 5 minutes with regular pongs (stable)
        let t_6m = t0 + Duration::from_secs(360);
        sup.on_pong("bar", t_6m);
        sup.tick(t_6m);
        assert!(sup.get("bar").unwrap().crash_history.is_empty());
    }

    #[test]
    fn startup_and_hello_transition() {
        let mut sup = Supervisor::new();
        let t0 = Instant::now();
        let rec = sup.get_mut("bar").unwrap();
        rec.state = SupervisorState::Starting { spawned_at: t0 };

        sup.on_hello("bar", t0 + Duration::from_secs(1));
        assert!(matches!(sup.get("bar").unwrap().state, SupervisorState::Running { .. }));
    }

    #[test]
    fn startup_timeout_kills_and_backs_off() {
        let mut sup = Supervisor::new();
        let t0 = Instant::now();
        let rec = sup.get_mut("bar").unwrap();
        rec.state = SupervisorState::Starting { spawned_at: t0 };

        // 6 seconds later (> 5s limit)
        let t_timeout = t0 + Duration::from_secs(6);
        sup.tick(t_timeout);
        assert!(matches!(sup.get("bar").unwrap().state, SupervisorState::Backoff { .. }));
    }
}
