//! Velogin — graphical TTY login manager (veil-login crate, temporarily renamed from Abyss).
//!
//! Slint greeter (DRM/KMS bare-TTY, or winit window when developing nested)
//! → PAM auth → fork/setuid/exec into the chosen session. One login per
//! process life: after the session ends we exit and systemd respawns us,
//! which sidesteps event-loop-restart and DRM-master-reacquire entirely.
//!
//! Credentials are verified on a worker thread while the greeter shows
//! "authenticating…"; on success the event loop quits, the UI (and its DRM
//! master) is dropped, and the real PAM session + exec happens in spawn.rs.

mod session;
mod spawn;
mod state;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::sync::Mutex;

slint::include_modules!();

/// Handoff from the auth thread to the main thread across the event-loop quit.
static PENDING: Mutex<Option<(String, String, usize)>> = Mutex::new(None);

fn detect_current_tty() -> i32 {
    if let Ok(active) = std::fs::read_to_string("/sys/class/tty/tty0/active") {
        let active = active.trim();
        if let Some(num_str) = active.strip_prefix("tty") {
            if let Ok(num) = num_str.parse::<i32>() {
                return num;
            }
        }
    }
    1
}

fn check_prerequisites(dry_run: bool) {
    if dry_run {
        return;
    }
    if !std::path::Path::new("/etc/pam.d/velogin").is_file() {
        eprintln!("[velogin] missing /etc/pam.d/velogin — auth will always fail");
        eprintln!("[velogin] install veil-login/dist/pam.d/velogin there, then retry");
    }
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("[velogin] not running as root — auth will fail for other users");
        eprintln!("[velogin] (set VELOGIN_DRY_RUN=1 to test the UI without logging in)");
    }
    if !std::path::Path::new("/run/seatd.sock").exists() {
        eprintln!("[velogin] /run/seatd.sock missing — linuxkms needs seatd on a bare TTY");
        eprintln!("[velogin] systemctl enable --now seatd.service");
    }
}

fn main() {
    // Second stage: re-exec'd clean session helper (see reexec_into_session).
    // A fresh process image — no Slint/GL/libseat threads, no possibly-wedged
    // malloc arena — so gkr-pam's session hook (which forks gnome-keyring-daemon
    // and does control-socket I/O) runs the way it does under sddm-helper.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|s| s == "--session").unwrap_or(false) {
        run_session_helper(&argv);
        return;
    }

    let dry_run = std::env::var("VELOGIN_DRY_RUN").is_ok();
    check_prerequisites(dry_run);

    let sessions = session::detect();
    eprintln!(
        "[velogin] sessions: {:?}",
        sessions.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );

    let ui = match LoginWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("[velogin] failed to create UI: {e}");
            std::process::exit(1);
        }
    };

    // ── static setup ────────────────────────────────────────────────────────
    let names: Vec<slint::SharedString> =
        sessions.iter().map(|s| s.name.as_str().into()).collect();
    ui.set_sessions(std::rc::Rc::new(slint::VecModel::from(names)).into());

    // Session memory: preselect whatever was logged into last (Niri stays Niri).
    if let Some(last) = state::last_session() {
        if let Some(idx) = sessions.iter().position(|s| s.name == last) {
            ui.set_session_index(idx as i32);
        }
    }

    if let Ok(hn) = std::fs::read_to_string("/etc/hostname") {
        ui.set_hostname(hn.trim().into());
    }
    if let Some(img) = state::wallpaper_path().and_then(|p| load_image(&p)) {
        ui.set_wallpaper(img);
        ui.set_has_wallpaper(true);
    }
    // User enumeration & preselection
    let detected_users = state::detect_users();
    let user_names: Vec<slint::SharedString> =
        detected_users.iter().map(|u| u.as_str().into()).collect();
    ui.set_users(std::rc::Rc::new(slint::VecModel::from(user_names)).into());

    if let Some(user) = state::last_user() {
        ui.set_username(user.as_str().into());
        ui.set_user_known(true);
        load_avatar(&ui, &user);
    } else if let Some(first_user) = detected_users.first() {
        ui.set_username(first_user.as_str().into());
        load_avatar(&ui, first_user);
    }

    let current_tty = detect_current_tty();
    ui.set_current_tty(current_tty);

    // Clock for the stage-0 screen; ticks fast enough that seconds never skip.
    let clock_timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        clock_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(250),
            move || {
                if let Some(ui) = weak.upgrade() {
                    let (time, date) = now_strings();
                    ui.set_clock_time(time.into());
                    ui.set_clock_date(date.into());
                }
            },
        );
    }

    // ── callbacks ───────────────────────────────────────────────────────────
    {
        let weak = ui.as_weak();
        ui.on_username_edited(move |name| {
            if let Some(ui) = weak.upgrade() {
                load_avatar(&ui, &name);
            }
        });
    }

    ui.on_poweroff(|| power_action("poweroff"));
    ui.on_reboot(|| power_action("reboot"));
    ui.on_switch_tty(move |tty_num| switch_vt(tty_num));

    // Oh-shit key (Ctrl+Shift+\, bound in velogin.slint's killswitch
    // FocusScope): bail exactly like closing the dev window — quit the event
    // loop with no PENDING login set, so the post-run() teardown below (drop
    // ui → release DRM/libseat, sleep, close_lingering_fds) runs and we exit
    // without spawning a session.
    ui.on_emergency_exit(|| {
        eprintln!("[velogin] emergency exit (Ctrl+Shift+\\) — bailing out");
        let _ = slint::quit_event_loop();
    });

    {
        let weak = ui.as_weak();
        ui.on_vk_key(move |field, ch| {
            if let Some(ui) = weak.upgrade() {
                if field == 1 {
                    ui.set_password(format!("{}{}", ui.get_password(), ch).into());
                } else {
                    let name = format!("{}{}", ui.get_username(), ch);
                    load_avatar(&ui, &name);
                    ui.set_username(name.into());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_vk_backspace(move |field| {
            if let Some(ui) = weak.upgrade() {
                if field == 1 {
                    let s = ui.get_password();
                    ui.set_password(pop_char(&s).into());
                } else {
                    let s = ui.get_username();
                    let name = pop_char(&s);
                    load_avatar(&ui, &name);
                    ui.set_username(name.into());
                }
            }
        });
    }

    // Failed-attempt backoff: no cooldown for the first couple of tries, then
    // an escalating delay (3s, 6s, 9s, …) so a wrong password isn't a free
    // unlimited-guess loop against PAM. Purely a UI-level throttle — faillock
    // (via PAM) is still the real enforcement backstop.
    let fail_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    {
        let weak = ui.as_weak();
        let session_names: Vec<String> =
            sessions.iter().map(|s| s.name.clone()).collect();
        let fail_count = fail_count.clone();
        ui.on_login(move |username, password, session_idx| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_busy(true);
            ui.set_error_msg("".into());
            ui.set_status_msg("".into());

            let username = username.to_string();
            let password = password.to_string();
            let session_idx = session_idx.max(0) as usize;
            let session_names = session_names.clone();
            let weak = weak.clone();
            let fail_count = fail_count.clone();

            std::thread::spawn(move || {
                let status_weak = weak.clone();
                let result = if dry_run {
                    Ok(())
                } else {
                    spawn::verify(&username, &password, move |msg| {
                        let status_weak = status_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = status_weak.upgrade() {
                                ui.set_status_msg(msg.into());
                            }
                        });
                    })
                    .map_err(|e| e.to_string())
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match result {
                        Ok(()) => {
                            fail_count.store(0, std::sync::atomic::Ordering::Relaxed);
                            state::save_last_user(&username);
                            if let Some(name) = session_names.get(session_idx) {
                                state::save_last_session(name);
                            }
                            *PENDING.lock().unwrap() =
                                Some((username, password, session_idx));
                            let _ = slint::quit_event_loop();
                        }
                        Err(e) => {
                            eprintln!("[velogin] auth failed: {e}");
                            ui.set_busy(false);
                            ui.set_password("".into());
                            let msg = if !std::path::Path::new("/etc/pam.d/velogin").is_file() {
                                "missing /etc/pam.d/velogin".into()
                            } else {
                                "authentication failed".into()
                            };
                            ui.set_error_msg(msg);
                            shake_card(&ui);

                            let attempts =
                                fail_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            if attempts >= 3 {
                                start_cooldown(&ui, (attempts - 2) * 3);
                            }
                        }
                    }
                });
            });
        });
    }

    // ── run greeter ─────────────────────────────────────────────────────────
    if let Err(e) = ui.run() {
        eprintln!("[velogin] event loop error: {e}");
        std::process::exit(1);
    }

    // Release the window/backend (DRM master) before spawning the session.
    drop(ui);
    // Give the backend's own teardown (its libseat disconnect is a graceful
    // handshake, not just a socket close) a moment to actually run before we
    // resort to force-closing fds. Yanking the seatd connection out from
    // under an in-flight disable request is what was tripping a seatd
    // assertion crash (seat.c get_tty_path) on every login attempt.
    std::thread::sleep(std::time::Duration::from_millis(300));
    close_lingering_fds();

    // ── launch session, if a login succeeded ────────────────────────────────
    let Some((username, password, idx)) = PENDING.lock().unwrap().take() else {
        return; // window closed without login (dev mode)
    };
    let entry = sessions.get(idx).cloned().unwrap_or_else(|| sessions[0].clone());
    eprintln!("[velogin] logging {username} into {:?} ({})", entry.name, entry.exec);

    if dry_run {
        eprintln!("[velogin] dry run — would exec: {:?}", entry.exec);
        return;
    }

    // Hand off to a *fresh* process image for the PAM session + keyring + exec.
    // We (this process) rendered the greeter with Slint/FemtoVG/OpenGL/libseat;
    // opening the PAM session here means gkr-pam's session hook forks
    // gnome-keyring-daemon out of that GPU-contaminated address space, which
    // hangs `open_session()` forever (same fork-from-a-graphics-process class as
    // the old initgroups deadlock). sddm dodges this by doing PAM in a clean
    // `sddm-helper`; reexec_into_session is our equivalent — execve replaces this
    // image entirely (killing the lingering render/libseat threads) but keeps the
    // PID and controlling TTY, so systemd and the session choreography are intact.
    let err = reexec_into_session(&username, &password, &entry);
    eprintln!("[velogin] failed to re-exec into session helper: {err}");
    std::process::exit(1);
}

/// Stage 1 (greeter side): buffer the password into a pipe, then `execve`
/// ourselves as `velogin --session <user> <name> <exec> <pipe_rd_fd>`. On
/// success this never returns — the process becomes the clean session helper.
/// Returns the failing errno only if the execve itself fails.
///
/// The password goes through a pipe (read-fd inherited across execve), never
/// argv/env, so it can't leak via `ps`/`/proc/<pid>/cmdline`. It must run
/// *after* close_lingering_fds() (which frees the low fd numbers and releases
/// the seat) — the pipe then lands on fd 3/4 and survives the exec.
fn reexec_into_session(username: &str, password: &str, entry: &session::SessionEntry) -> std::io::Error {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return std::io::Error::last_os_error();
    }
    let (rd, wr) = (fds[0], fds[1]);

    // Buffer the whole password into the pipe, then close the write end so the
    // helper reads it and sees EOF. Passwords are far under the pipe capacity,
    // so this never blocks.
    let pw = password.as_bytes();
    let mut off = 0;
    while off < pw.len() {
        let n = unsafe { libc::write(wr, pw[off..].as_ptr() as *const _, pw.len() - off) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            unsafe { libc::close(wr); libc::close(rd); }
            return e;
        }
        off += n as usize;
    }
    unsafe { libc::close(wr) };
    // pipe(2) fds are not close-on-exec, but be explicit — the helper needs rd.
    unsafe {
        let flags = libc::fcntl(rd, libc::F_GETFD);
        libc::fcntl(rd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
    }

    let exe = std::env::current_exe()
        .or_else(|_| std::fs::read_link("/proc/self/exe"))
        .unwrap_or_else(|_| std::path::PathBuf::from("velogin"));
    let Ok(exe_c) = CString::new(exe.as_os_str().as_bytes()) else {
        return std::io::Error::new(std::io::ErrorKind::InvalidInput, "exe path has NUL");
    };
    // name/exec/user carried as argv (no secrets); a NUL in any is fatal but
    // can't occur for real session metadata.
    let args = [
        CString::new("velogin"),
        CString::new("--session"),
        CString::new(username),
        CString::new(entry.name.as_bytes()),
        CString::new(entry.exec.as_bytes()),
        CString::new(rd.to_string()),
    ];
    let mut cstrs = Vec::with_capacity(args.len());
    for a in args {
        match a {
            Ok(c) => cstrs.push(c),
            Err(_) => return std::io::Error::new(std::io::ErrorKind::InvalidInput, "arg has NUL"),
        }
    }
    let mut ptrs: Vec<*const libc::c_char> = cstrs.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    unsafe { libc::execv(exe_c.as_ptr(), ptrs.as_ptr()) };
    std::io::Error::last_os_error() // execv only returns on failure
}

/// Stage 2: the clean session helper. Reads the password back from the inherited
/// pipe fd and runs the real PAM session + fork/exec via [`spawn::launch`], which
/// waits for the session and closes PAM when it ends. Then we exit and systemd
/// (Restart=always) respawns the greeter.
fn run_session_helper(argv: &[String]) {
    let username = argv.get(2).cloned().unwrap_or_default();
    let name = argv.get(3).cloned().unwrap_or_default();
    let exec = argv.get(4).cloned().unwrap_or_default();
    let rd: i32 = argv.get(5).and_then(|s| s.parse().ok()).unwrap_or(-1);

    read_pipe_password(rd); // drains and closes the pipe fd; password not re-used (no double-auth)
    let entry = session::SessionEntry { name, exec };
    eprintln!("[velogin:session] clean helper up, launching {username} into {:?}", entry.name);
    match spawn::launch(&username, &entry) {
        Ok(()) => eprintln!("[velogin:session] session ended, exiting (systemd respawns greeter)"),
        Err(e) => {
            eprintln!("[velogin:session] launch failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Drain the password from the inherited pipe fd, then close it. Uses a raw
/// read loop rather than `File::from_raw_fd` so a bogus fd yields an empty
/// string instead of tripping Rust's I/O-safety double-close abort.
fn read_pipe_password(fd: i32) -> String {
    if fd < 0 {
        eprintln!("[velogin:session] no password fd — nothing to read");
        return String::new();
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            eprintln!(
                "[velogin:session] reading password pipe failed: {}",
                std::io::Error::last_os_error()
            );
            break;
        }
        if n == 0 {
            break; // EOF: writer closed
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    unsafe { libc::close(fd) };
    String::from_utf8_lossy(&buf).into_owned()
}

fn load_avatar(ui: &LoginWindow, username: &str) {
    let initial = username
        .trim()
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".into());
    ui.set_user_initial(initial.into());
    match state::avatar_path(username.trim()).and_then(|p| load_image(&p)) {
        Some(img) => {
            ui.set_avatar(img);
            ui.set_has_avatar(true);
        }
        None => ui.set_has_avatar(false),
    }
}

/// Local time as ("HH:MM:SS", "Weekday, Month D") via libc — no chrono dep.
fn now_strings() -> (String, String) {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        let mut buf = [0u8; 64];
        let n = libc::strftime(buf.as_mut_ptr() as *mut _, 64, c"%H:%M:%S".as_ptr(), &tm);
        let time = String::from_utf8_lossy(&buf[..n]).into_owned();
        let n = libc::strftime(buf.as_mut_ptr() as *mut _, 64, c"%A, %B %e".as_ptr(), &tm);
        let date = String::from_utf8_lossy(&buf[..n]).into_owned();
        (time, date.trim().to_string())
    }
}

/// Decode an image by *content*, not extension — AccountsService avatars are
/// extensionless, which Slint's own load_from_path can't identify.
fn load_image(path: &std::path::Path) -> Option<slint::Image> {
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let buf =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(img.as_raw(), w, h);
    Some(slint::Image::from_rgba8(buf))
}

/// Drop the last char of a string (UTF-8 aware) — Slint can't slice strings.
fn pop_char(s: &str) -> String {
    let mut s = s.to_string();
    s.pop();
    s
}

/// Switch the foreground VT (chvt equivalent). Needs root; the linuxkms
/// backend releases the display via libseat on the switch and reacquires
/// when the user comes back (Ctrl+Alt+F1). No-op failure under winit dev.
fn switch_vt(n: i32) {
    const VT_ACTIVATE: libc::c_ulong = 0x5606;
    eprintln!("[velogin] switching to tty{n}");
    unsafe {
        let fd = libc::open(c"/dev/tty0".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if fd < 0 {
            eprintln!("[velogin] open /dev/tty0 failed: {}", std::io::Error::last_os_error());
            return;
        }
        if libc::ioctl(fd, VT_ACTIVATE as _, n) < 0 {
            eprintln!("[velogin] VT_ACTIVATE failed: {}", std::io::Error::last_os_error());
        }
        libc::close(fd);
    }
}

/// Force-close every fd above stderr in this still-running process.
///
/// `drop(ui)` tears down Slint's linuxkms backend at the Rust level, but its
/// libseat connection to seatd may live on a background I/O thread that a
/// Drop impl only *asks* to stop rather than joining — no guarantee it has
/// actually disconnected by the time we return. This process stays alive for
/// the whole session (waiting in launch()'s waitpid), so if that seatd
/// socket is still open, we are still the seat's active client and the
/// incoming compositor (e.g. niri via systemd --user) will block forever
/// trying to acquire DRM master that we never gave up. Closing every fd
/// forces an immediate socket EOF instead of trusting an async teardown that
/// may never run before it matters.
fn close_lingering_fds() {
    // Use opendir/readdir/dirfd directly so we can exclude the directory's
    // own fd from the close list.  The previous read_dir approach collected
    // all fds and then closed them, but ReadDir closes its internal dirfd
    // on drop (which happens after collect()) — so we'd double-close that
    // fd number in the loop, potentially hitting a reused descriptor.
    let dir = unsafe { libc::opendir(c"/proc/self/fd".as_ptr()) };
    if dir.is_null() { return; }
    let dir_fd = unsafe { libc::dirfd(dir) };
    let mut fds = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() { break; }
        let name = unsafe { (*entry).d_name.as_ptr() };
        if let Ok(s) = unsafe { std::ffi::CStr::from_ptr(name) }.to_str() {
            if let Ok(fd) = s.parse::<i32>() {
                if fd > 2 && fd != dir_fd {
                    fds.push(fd);
                }
            }
        }
    }
    unsafe { libc::closedir(dir) };
    for fd in fds {
        unsafe { libc::close(fd) };
    }
}

/// Nudge the login card side to side — a few quick single-shot timers step
/// through offsets since Slint's `animate` only tweens start→end, not a
/// multi-point wiggle.
fn shake_card(ui: &LoginWindow) {
    const OFFSETS: [i32; 8] = [-14, 12, -10, 8, -5, 3, -1, 0];
    for (i, off) in OFFSETS.iter().enumerate() {
        let weak = ui.as_weak();
        let off = *off;
        slint::Timer::single_shot(std::time::Duration::from_millis(40 * i as u64), move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_shake_offset(off as f32);
            }
        });
    }
}

/// Escalating lockout after repeated failed logins — a UI-level throttle;
/// PAM's own faillock is still the real backstop against brute-forcing.
/// Chained single-shots (like shake_card) — no timer handle to keep alive.
fn start_cooldown(ui: &LoginWindow, secs: u32) {
    ui.set_cooldown_secs(secs as i32);
    for tick in 1..=secs {
        let weak = ui.as_weak();
        let left = secs - tick;
        slint::Timer::single_shot(std::time::Duration::from_secs(tick as u64), move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_cooldown_secs(left as i32);
            }
        });
    }
}

fn power_action(verb: &str) {
    eprintln!("[velogin] {verb} requested");
    let st = std::process::Command::new("systemctl").arg(verb).status();
    if !st.map(|s| s.success()).unwrap_or(false) {
        eprintln!("[velogin] systemctl {verb} failed");
    }
}
