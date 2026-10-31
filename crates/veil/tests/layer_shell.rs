//! Phase 1 integration tests: the real compositor (in-process, no output
//! backend), a real wlr-layer-shell client, and the real IPC socket.
//!
//! Needs `$XDG_RUNTIME_DIR` (the compositor binds a Wayland socket there);
//! without it the tests skip instead of failing. Nothing here touches the
//! process environment, so tests can run in parallel: each one gets its own
//! Wayland socket and IPC socket.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use veil_host::{Host, HostConfig, InputCmd, ShellOptions};
use veil_ipc::{ClientMessage, FrameBuffer, ServerMessage};
use veil_ui::{Anchor, KeyboardInteractivity, Layer, LayerOptions, LayerShellClient, UiInputEvent};

const W: i32 = 1280;
const H: i32 = 720;
const BTN_LEFT: u32 = 0x110;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ─── harness ─────────────────────────────────────────────────────────────────

struct Env {
    host: Host,
    wayland_socket: PathBuf,
    ipc_path: PathBuf,
}

fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

/// Start a compositor, or `None` (skip) when there is no runtime dir.
fn start(shell_bar: bool, trusted: &[&str]) -> Option<Env> {
    let Some(rt) = runtime_dir() else {
        eprintln!("skipping: XDG_RUNTIME_DIR not set");
        return None;
    };
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tag = format!("veil-it-{}-{n}", std::process::id());
    let ipc_dir = rt.join(&tag);
    std::fs::create_dir_all(&ipc_dir).unwrap();
    let ipc_path = ipc_dir.join("ipc.sock");

    let cfg = HostConfig {
        socket_name: format!("wayland-{tag}"),
        width: W as u32,
        height: H as u32,
        fps: 10,
        bar: veil_config::BarConfig {
            shell: shell_bar,
            ..Default::default()
        },
        shell: ShellOptions {
            ipc_socket: Some(ipc_path.clone()),
            shell_exe: Some(PathBuf::from(env!("CARGO_BIN_EXE_veil"))),
            trusted_namespaces: trusted.iter().map(|s| s.to_string()).collect(),
        },
        ..HostConfig::default()
    };
    let wayland_socket = rt.join(&cfg.socket_name);
    let host = Host::spawn(cfg).expect("spawn compositor");
    let env = Env {
        host,
        wayland_socket,
        ipc_path,
    };

    // The real monitor layout (also what releases the supervised shell).
    env.host
        .send_input(InputCmd::SetMonitors {
            sizes: vec![(W as u32, H as u32)],
            refresh_mhz: 60_000,
            refresh_per_monitor: vec![60_000],
        })
        .unwrap();
    wait_for("compositor sockets", Duration::from_secs(5), || {
        env.wayland_socket.exists() && env.ipc_path.exists()
    });
    // With no anchor command the compositor opens its launcher, a modal that
    // swallows every click. Close it (Escape, evdev 1) so input reaches clients.
    for pressed in [true, false] {
        env.host
            .send_input(InputCmd::Key { keycode: 1, mods: 0, pressed })
            .unwrap();
    }
    Some(env)
}

impl Drop for Env {
    fn drop(&mut self) {
        self.host.stop();
        // Give the loop a moment to unwind (and the supervisor to reap).
        std::thread::sleep(Duration::from_millis(300));
        let _ = std::fs::remove_dir_all(self.ipc_path.parent().unwrap());
        // The compositor leaves its Wayland socket and lock file behind.
        let _ = std::fs::remove_file(&self.wayland_socket);
        let mut lock = self.wayland_socket.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

impl Env {
    /// Keep the frame channel from piling up while a test runs.
    fn drain(&self) {
        while self.host.frames().try_recv().is_ok() {}
    }

    fn ipc(&self) -> Ipc {
        Ipc::connect(&self.ipc_path)
    }

    fn layers(&self) -> Value {
        self.drain();
        self.ipc().cmd("layers.list", json!({})).unwrap_or(Value::Null)
    }

    /// The (single) output's layer entry for `namespace`, if any.
    fn layer(&self, namespace: &str) -> Option<Value> {
        self.layers()["outputs"][0]["layers"]
            .as_array()?
            .iter()
            .find(|l| l["namespace"] == namespace)
            .cloned()
    }

    fn work_area(&self) -> Vec<i64> {
        self.layers()["outputs"][0]["work_area"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default()
    }

    fn keyboard_focus(&self) -> Value {
        self.layers()["keyboard_focus"].clone()
    }

    fn pointer_to(&self, x: i32, y: i32) {
        self.host
            .send_input(InputCmd::PointerMotionAbs {
                x,
                y,
                width: W as u32,
                height: H as u32,
            })
            .unwrap();
    }

    fn click(&self) {
        for pressed in [true, false] {
            self.host
                .send_input(InputCmd::PointerButton {
                    button: BTN_LEFT,
                    pressed,
                })
                .unwrap();
        }
    }

    fn client(&self, opts: LayerOptions) -> TestClient {
        TestClient::spawn(&self.wayland_socket, opts)
    }
}

fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    panic!("timed out after {timeout:?} waiting for: {what}");
}

// ─── IPC client ──────────────────────────────────────────────────────────────

struct Ipc {
    stream: UnixStream,
    frames: FrameBuffer,
    next_id: u64,
}

impl Ipc {
    fn connect(path: &Path) -> Self {
        let start = Instant::now();
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(s) => break s,
                Err(e) if start.elapsed() > Duration::from_secs(5) => panic!("ipc connect: {e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(40)),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut c = Self {
            stream,
            frames: FrameBuffer::new(),
            next_id: 1,
        };
        c.send(&ClientMessage::Hello {
            version: veil_ipc::PROTOCOL_VERSION,
            name: "it".into(),
        });
        match c.recv() {
            ServerMessage::Hello { .. } => {}
            other => panic!("expected hello, got {other:?}"),
        }
        c
    }

    fn send(&mut self, msg: &ClientMessage) {
        self.stream
            .write_all(&veil_ipc::encode(msg).unwrap())
            .unwrap();
    }

    fn recv(&mut self) -> ServerMessage {
        let mut buf = [0u8; 8192];
        loop {
            if let Some(m) = self.frames.next_frame::<ServerMessage>().unwrap() {
                return m;
            }
            let n = self.stream.read(&mut buf).expect("ipc read");
            assert!(n > 0, "ipc closed");
            self.frames.push(&buf[..n]).unwrap();
        }
    }

    fn cmd(&mut self, cmd: &str, args: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&ClientMessage::Cmd {
            id,
            cmd: cmd.into(),
            args,
        });
        loop {
            if let ServerMessage::Reply {
                id: rid,
                ok,
                error,
                data,
            } = self.recv()
            {
                if rid == id {
                    return if ok {
                        Ok(data.unwrap_or(Value::Null))
                    } else {
                        Err(error.unwrap_or_default())
                    };
                }
            }
        }
    }
}

// ─── layer-shell test client ─────────────────────────────────────────────────

/// A real layer-shell client on its own thread. `stall()` makes it stop
/// servicing its socket entirely, like a hung process.
struct TestClient {
    stall: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    input: Receiver<UiInputEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestClient {
    fn spawn(socket: &Path, opts: LayerOptions) -> Self {
        let stream = UnixStream::connect(socket).expect("connect to compositor");
        let stall = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (stall_t, stop_t) = (stall.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut client =
                LayerShellClient::with_options(opts, Some(stream)).expect("layer client");
            let mut drawn = false;
            while !stop_t.load(Ordering::Relaxed) {
                if stall_t.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                if client.dispatch().is_err() {
                    return;
                }
                if client.take_reconfigured() || !drawn {
                    client.present_for_each(|_, w, h| vec![0x80; (w * h * 4) as usize]);
                    drawn = true;
                }
                while let Ok(ev) = client.input_rx.try_recv() {
                    let _ = tx.send(ev);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Self {
            stall,
            stop,
            input: rx,
            thread: Some(thread),
        }
    }

    fn stall(&self) {
        self.stall.store(true, Ordering::Relaxed);
    }

    fn saw(&self, within: Duration, want: fn(&UiInputEvent) -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < within {
            if let Ok(ev) = self.input.recv_timeout(Duration::from_millis(50)) {
                if want(&ev) {
                    return true;
                }
            }
        }
        false
    }
}

impl Drop for TestClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn opts(
    ns: &str,
    layer: Layer,
    anchor: Anchor,
    size: (u32, u32),
    zone: i32,
    kbd: KeyboardInteractivity,
) -> LayerOptions {
    LayerOptions {
        namespace: ns.into(),
        layer,
        anchor,
        size,
        exclusive_zone: zone,
        keyboard: kbd,
    }
}

fn top_bar() -> Anchor {
    Anchor::TOP | Anchor::LEFT | Anchor::RIGHT
}

fn mapped(env: &Env, ns: &str) -> bool {
    env.layer(ns).is_some_and(|l| l["mapped"] == true)
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[test]
fn client_shows_reserves_space_and_gets_input() {
    let Some(env) = start(false, &[]) else { return };
    assert_eq!(env.work_area(), vec![0, 0, W as i64, H as i64]);

    let client = env.client(opts(
        "test-bar",
        Layer::Top,
        top_bar(),
        (0, 36),
        36,
        KeyboardInteractivity::None,
    ));
    wait_for("surface mapped", Duration::from_secs(5), || {
        mapped(&env, "test-bar")
    });

    let l = env.layer("test-bar").unwrap();
    assert_eq!(l["layer"], "top");
    assert_eq!(l["geometry"], json!([0, 0, W, 36]));
    assert_eq!(
        env.work_area(),
        vec![0, 36, W as i64, (H - 36) as i64],
        "exclusive zone shrinks the work area"
    );

    // Pointer over the surface reaches the client.
    let start = Instant::now();
    let mut got_motion = false;
    while start.elapsed() < Duration::from_secs(4) && !got_motion {
        env.pointer_to(100, 10);
        env.drain();
        got_motion = client.saw(Duration::from_millis(200), |e| {
            matches!(e, UiInputEvent::Motion { .. })
        });
    }
    assert!(
        got_motion,
        "client never received pointer motion (compositor pointer focus: {})",
        env.layers()["pointer_focus"]
    );
    env.click();
    assert!(
        client.saw(Duration::from_secs(2), |e| matches!(
            e,
            UiInputEvent::Press { .. }
        )),
        "client never received the press"
    );

    // Closing the client gives the space back.
    drop(client);
    wait_for("work area restored", Duration::from_secs(5), || {
        env.work_area() == vec![0, 0, W as i64, H as i64]
    });
}

/// Tiny deterministic generator so a failure reproduces.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

#[test]
fn exclusive_zone_arithmetic_matches_the_model() {
    let Some(env) = start(false, &[]) else { return };
    let mut rng = Lcg(0x5eed);

    for round in 0..6 {
        // One to three single-edge surfaces, each with its own zone.
        let count = 1 + rng.next(3) as usize;
        let (mut top, mut bottom, mut left, mut right) = (0i64, 0i64, 0i64, 0i64);
        let mut clients = Vec::new();
        for i in 0..count {
            let zone = 8 + rng.next(60) as i64;
            let (anchor, size) = match rng.next(4) {
                0 => {
                    top += zone;
                    (top_bar(), (0, zone as u32))
                }
                1 => {
                    bottom += zone;
                    (Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT, (0, zone as u32))
                }
                2 => {
                    left += zone;
                    (Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM, (zone as u32, 0))
                }
                _ => {
                    right += zone;
                    (Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM, (zone as u32, 0))
                }
            };
            let layer = if rng.next(2) == 0 {
                Layer::Top
            } else {
                Layer::Bottom
            };
            clients.push(env.client(opts(
                &format!("zone-{round}-{i}"),
                layer,
                anchor,
                size,
                zone as i32,
                KeyboardInteractivity::None,
            )));
        }
        let expect = vec![left, top, W as i64 - left - right, H as i64 - top - bottom];
        wait_for(
            &format!("round {round}: work area {expect:?}"),
            Duration::from_secs(6),
            || env.work_area() == expect,
        );

        drop(clients);
        wait_for("work area restored", Duration::from_secs(6), || {
            env.work_area() == vec![0, 0, W as i64, H as i64]
        });
    }
}

#[test]
fn trusted_exclusive_holder_keeps_the_keyboard_while_healthy_and_loses_it_when_hung() {
    let Some(env) = start(false, &["test-launch"]) else {
        return;
    };

    let launcher = env.client(opts(
        "test-launch",
        Layer::Overlay,
        Anchor::empty(),
        (400, 200),
        0,
        KeyboardInteractivity::Exclusive,
    ));
    wait_for("launcher holds the keyboard", Duration::from_secs(5), || {
        env.keyboard_focus() == "test-launch"
    });
    let l = env.layer("test-launch").unwrap();
    assert_eq!(l["layer"], "overlay", "trusted overlay is honoured");
    assert_eq!(l["keyboard"], "exclusive");

    // Healthy and idle for well over the watchdog budget: probes are
    // answered, so it must keep the grab.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(4) {
        assert_eq!(
            env.keyboard_focus(),
            "test-launch",
            "a healthy holder lost the grab"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // Now it hangs.
    launcher.stall();
    let hung_at = Instant::now();
    wait_for("grab revoked", Duration::from_secs(4), || {
        env.layer("test-launch")
            .is_some_and(|l| l["revoked"] == true)
    });
    let took = hung_at.elapsed();
    assert!(
        took <= Duration::from_millis(3500),
        "revoked after {took:?}, budget is about 2 s"
    );
    assert_ne!(env.keyboard_focus(), "test-launch");
    assert_eq!(env.layer("test-launch").unwrap()["keyboard"], "on_demand");
    assert_eq!(env.layers()["grab"], Value::Null);
}

#[test]
fn exclusive_grab_ends_when_the_holder_goes_away() {
    let Some(env) = start(false, &["test-launch"]) else {
        return;
    };
    let launcher = env.client(opts(
        "test-launch",
        Layer::Overlay,
        Anchor::empty(),
        (400, 200),
        0,
        KeyboardInteractivity::Exclusive,
    ));
    wait_for("grab", Duration::from_secs(5), || {
        env.keyboard_focus() == "test-launch"
    });
    drop(launcher);
    wait_for("grab released", Duration::from_secs(5), || {
        env.keyboard_focus() != "test-launch"
    });
    assert_eq!(env.layers()["grab"], Value::Null);
}

#[test]
fn an_unsupervised_client_cannot_claim_overlay_or_exclusive_by_name() {
    // "veil-launch" is the name a real component would use; the compositor
    // must not take the client's word for it.
    let Some(env) = start(false, &[]) else { return };
    let _evil = env.client(opts(
        "veil-launch",
        Layer::Overlay,
        Anchor::empty(),
        (400, 200),
        0,
        KeyboardInteractivity::Exclusive,
    ));
    wait_for("mapped", Duration::from_secs(5), || {
        mapped(&env, "veil-launch")
    });
    let l = env.layer("veil-launch").unwrap();
    assert_eq!(l["layer"], "top", "overlay downgraded");
    assert_eq!(l["keyboard"], "on_demand", "exclusive downgraded");
    assert_ne!(env.keyboard_focus(), "veil-launch");
    assert_eq!(env.layers()["grab"], Value::Null);
}

#[test]
fn on_demand_takes_the_keyboard_on_click_and_none_never_does() {
    let Some(env) = start(false, &[]) else { return };
    // Stacked at the top: the first takes 0..40, the second 40..80.
    let _none = env.client(opts(
        "kbd-none",
        Layer::Top,
        top_bar(),
        (0, 40),
        40,
        KeyboardInteractivity::None,
    ));
    wait_for("none mapped", Duration::from_secs(5), || {
        mapped(&env, "kbd-none")
    });
    let _demand = env.client(opts(
        "kbd-demand",
        Layer::Top,
        top_bar(),
        (0, 40),
        40,
        KeyboardInteractivity::OnDemand,
    ));
    wait_for("demand mapped", Duration::from_secs(5), || {
        mapped(&env, "kbd-demand")
    });

    let geom = |ns: &str| -> Vec<i64> {
        env.layer(ns).unwrap()["geometry"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_i64)
            .collect()
    };
    let (gn, gd) = (geom("kbd-none"), geom("kbd-demand"));
    assert_ne!(gn[1], gd[1], "stacked, not overlapping");

    let click_at = |g: &[i64]| {
        let (x, y) = ((g[0] + g[2] / 2) as i32, (g[1] + g[3] / 2) as i32);
        for _ in 0..5 {
            env.pointer_to(x, y);
            env.drain();
            std::thread::sleep(Duration::from_millis(60));
        }
        env.click();
    };

    click_at(&gn);
    std::thread::sleep(Duration::from_millis(400));
    assert_ne!(
        env.keyboard_focus(),
        "kbd-none",
        "keyboard mode none must never take focus"
    );

    click_at(&gd);
    wait_for("on-demand focus", Duration::from_secs(3), || {
        env.keyboard_focus() == "kbd-demand"
    });

    // A click elsewhere gives it back.
    env.pointer_to(W / 2, H / 2);
    std::thread::sleep(Duration::from_millis(100));
    env.click();
    wait_for("focus released", Duration::from_secs(3), || {
        env.keyboard_focus() != "kbd-demand"
    });
}

#[test]
fn supervised_bar_comes_back_after_kill_9() {
    let Some(env) = start(true, &[]) else { return };

    let status = |env: &Env| -> Option<(String, u32)> {
        let v = env.ipc().cmd("component.status", json!({})).ok()?;
        let bar = v.as_array()?.iter().find(|c| c["name"] == "bar")?.clone();
        Some((bar["state"].as_str()?.to_string(), bar["pid"].as_u64()? as u32))
    };

    wait_for("bar running and mapped", Duration::from_secs(15), || {
        env.drain();
        status(&env).is_some_and(|(s, _)| s == "running") && mapped(&env, "veil-bar")
    });
    let (_, pid) = status(&env).unwrap();
    // The bar is a supervised child, so it is trusted and owns its zone.
    assert_eq!(env.work_area(), vec![0, 36, W as i64, (H - 36) as i64]);

    sigkill(pid as i32);

    // The supervisor notices, backs off, and brings a new process up.
    wait_for(
        "bar restarted with a new pid",
        Duration::from_secs(20),
        || {
            env.drain();
            status(&env).is_some_and(|(s, p)| s == "running" && p != pid)
                && mapped(&env, "veil-bar")
        },
    );
    assert_eq!(env.work_area(), vec![0, 36, W as i64, (H - 36) as i64]);
}

fn sigkill(pid: i32) {
    // Spelled out so the test crate needs no libc dependency.
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: plain syscall wrapper with integer arguments.
    unsafe {
        kill(pid, 9);
    }
}
