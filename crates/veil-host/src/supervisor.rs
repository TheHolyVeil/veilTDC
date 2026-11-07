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
    /// `Failed` was already reported to the user (one OSD notice per failure).
    pub failed_announced: bool,
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
            failed_announced: false,
        }
    }
}

/// Kill a child and everything in its process group, then reap it.
fn kill_group(child: &mut Child) {
    // The child leads its own group (`process_group(0)` at spawn), so a
    // negative pid reaches anything it forked too.
    let pid = child.id() as i32;
    if pid > 0 {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
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

    /// Run components from this binary instead of `current_exe()` (tests).
    pub fn set_exe(&mut self, path: PathBuf) {
        self.exe_path = Some(path);
    }

    /// Is `pid` a live child this supervisor spawned? This is what the
    /// layer-shell trust rule asks; a namespace string proves nothing.
    pub fn is_supervised_pid(&self, pid: u32) -> bool {
        self.components
            .values()
            .any(|r| r.child.is_some() && r.pid == Some(pid))
    }

    /// Names that entered `Failed` since the last call, once each, so the
    /// compositor can show one OSD notice per failure.
    pub fn take_failed(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, r) in self.components.iter_mut() {
            if matches!(r.state, SupervisorState::Failed { .. }) && !r.failed_announced {
                r.failed_announced = true;
                out.push(name.clone());
            }
        }
        out.sort();
        out
    }

    /// (name, state, pid) for every component, sorted by name.
    pub fn status(&self) -> Vec<(String, &'static str, Option<u32>)> {
        let mut v: Vec<_> = self
            .components
            .values()
            .map(|r| {
                let st = match r.state {
                    SupervisorState::Disabled => "disabled",
                    SupervisorState::Starting { .. } => "starting",
                    SupervisorState::Running { .. } => "running",
                    SupervisorState::Backoff { .. } => "backoff",
                    SupervisorState::Failed { .. } => "failed",
                    SupervisorState::Stopped => "stopped",
                };
                (r.name.clone(), st, r.pid)
            })
            .collect();
        v.sort();
        v
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
        // Own process group: a restart or compositor exit kills the whole
        // tree, not just the direct child.
        cmd.process_group(0);

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

    /// Stop a running component (e.g. on-demand dismissal).
    pub fn stop_component(&mut self, name: &str) -> Result<(), String> {
        let record = self
            .components
            .get_mut(name)
            .ok_or_else(|| format!("Unknown component: {name}"))?;
        if let Some(mut child) = record.child.take() {
            kill_group(&mut child);
        }
        record.pid = None;
        if record.state != SupervisorState::Disabled {
            record.state = SupervisorState::Stopped;
        }
        Ok(())
    }

    /// Toggle an on-demand component: stop if running, start if stopped.
    /// Returns `Ok(true)` if started, `Ok(false)` if stopped.
    pub fn toggle_component(&mut self, name: &str, now: Instant) -> Result<bool, String> {
        let is_running = self.components.get(name).is_some_and(|r| {
            matches!(r.state, SupervisorState::Running { .. } | SupervisorState::Starting { .. })
        });
        if is_running {
            self.stop_component(name)?;
            Ok(false)
        } else {
            self.start_component(name, now)?;
            Ok(true)
        }
    }

    /// `veil ctl restart <n>`: kill whatever is running, forget past crashes
    /// (this is also the way out of `Failed`), and start it again.
    pub fn restart_component(&mut self, name: &str, now: Instant) -> Result<(), String> {
        let record = self
            .components
            .get_mut(name)
            .ok_or_else(|| format!("Unknown component: {name}"))?;
        if let Some(mut child) = record.child.take() {
            kill_group(&mut child);
        }
        record.pid = None;
        record.crash_history.clear();
        record.failed_announced = false;
        if record.state != SupervisorState::Disabled {
            record.state = SupervisorState::Stopped;
        }
        self.start_component(name, now)
    }

    /// Periodic tick: checks child exit status, startup timeouts, and ping watchdog.
    pub fn tick(&mut self, now: Instant) -> Vec<String> {
        let mut to_start = Vec::new();

        for (name, record) in self.components.iter_mut() {
            // Check if child process has exited
            if let Some(mut child) = record.child.take() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        // The leader is gone; sweep whatever it left in its
                        // group so a crashed component leaks no orphans.
                        unsafe {
                            libc::kill(-(child.id() as i32), libc::SIGKILL);
                        }
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
                            kill_group(&mut child);
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
                            kill_group(&mut child);
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

impl Drop for Supervisor {
    /// The compositor is going away: take every component's whole group with
    /// it (PDEATHSIG only covers the direct child).
    fn drop(&mut self) {
        for record in self.components.values_mut() {
            if let Some(mut child) = record.child.take() {
                kill_group(&mut child);
            }
        }
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

    /// A fake `veil` that ignores its args, forks a grandchild and sleeps.
    /// Returns (script path, file the grandchild's pid is written to).
    fn fake_exe(tag: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix(&format!("veil-sup-{tag}-"))
            .tempdir()
            .unwrap()
            .keep();
        let grandchild = dir.join("grandchild.pid");
        let script = dir.join("fake-veil");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nsleep 300 &\necho $! > {}\nwait\n", grandchild.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, grandchild)
    }

    fn alive(pid: i32) -> bool {
        // Signal 0 only checks existence. A zombie still "exists", so also
        // look at /proc for the Z state.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| !s.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
            .unwrap_or(false)
    }

    #[test]
    fn kill_9_of_a_child_is_restarted_after_backoff() {
        let (exe, _) = fake_exe("kill9");
        let mut sup = Supervisor::new();
        sup.set_exe(exe);
        let t0 = Instant::now();
        sup.start_component("bar", t0).unwrap();
        let pid = sup.get("bar").unwrap().pid.unwrap();
        assert!(sup.is_supervised_pid(pid));
        sup.on_hello("bar", t0);
        assert!(matches!(sup.get("bar").unwrap().state, SupervisorState::Running { .. }));

        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        // Let the kernel deliver it, then tick: exit is noticed, backoff starts.
        std::thread::sleep(Duration::from_millis(150));
        assert!(sup.tick(t0).is_empty());
        assert!(matches!(sup.get("bar").unwrap().state, SupervisorState::Backoff { .. }));
        assert!(!sup.is_supervised_pid(pid), "a dead child is no longer trusted");

        // Before the 0.5 s backoff ends: nothing. After: the supervisor asks for a start.
        assert!(sup.tick(t0 + Duration::from_millis(400)).is_empty());
        let to_start = sup.tick(t0 + Duration::from_millis(600));
        assert_eq!(to_start, vec!["bar".to_string()]);
        sup.start_component("bar", t0 + Duration::from_millis(600)).unwrap();
        let new_pid = sup.get("bar").unwrap().pid.unwrap();
        assert_ne!(pid, new_pid);
    }

    #[test]
    fn restart_kills_the_whole_process_group() {
        let (exe, gpid_file) = fake_exe("group");
        let mut sup = Supervisor::new();
        sup.set_exe(exe);
        let t0 = Instant::now();
        sup.start_component("bar", t0).unwrap();
        // Wait for the script to record its grandchild.
        let mut grandchild = 0i32;
        for _ in 0..50 {
            if let Ok(s) = std::fs::read_to_string(&gpid_file) {
                if let Ok(p) = s.trim().parse() {
                    grandchild = p;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(grandchild > 0 && alive(grandchild));

        sup.restart_component("bar", t0).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!alive(grandchild), "grandchild must die with its group");
        drop(sup);
    }

    #[test]
    fn dropping_the_supervisor_kills_its_children() {
        let (exe, _) = fake_exe("drop");
        let mut sup = Supervisor::new();
        sup.set_exe(exe);
        sup.start_component("bar", Instant::now()).unwrap();
        let pid = sup.get("bar").unwrap().pid.unwrap() as i32;
        assert!(alive(pid));
        drop(sup);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!alive(pid));
    }

    #[test]
    fn failure_is_announced_once() {
        let mut sup = Supervisor::new();
        let t0 = Instant::now();
        let rec = sup.get_mut("bar").unwrap();
        for i in 0..5 {
            Supervisor::handle_crash(rec, t0 + Duration::from_secs(i));
        }
        assert_eq!(sup.take_failed(), vec!["bar".to_string()]);
        assert!(sup.take_failed().is_empty());
    }

    #[test]
    fn test_toggle_component() {
        let (exe, _) = fake_exe("toggle");
        let mut sup = Supervisor::new();
        sup.set_exe(exe);
        let t0 = Instant::now();
        assert_eq!(sup.get("power").unwrap().state, SupervisorState::Stopped);

        // Toggle on
        let started = sup.toggle_component("power", t0).unwrap();
        assert!(started);
        assert!(matches!(sup.get("power").unwrap().state, SupervisorState::Starting { .. }));
        let pid = sup.get("power").unwrap().pid.unwrap() as i32;
        assert!(alive(pid));

        // Toggle off
        let started2 = sup.toggle_component("power", t0).unwrap();
        assert!(!started2);
        assert_eq!(sup.get("power").unwrap().state, SupervisorState::Stopped);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!alive(pid));
    }
}
