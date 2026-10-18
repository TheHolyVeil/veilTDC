//! Velogin — graphical TTY login manager (veil-login crate).
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
mod theme;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::sync::Mutex;

slint::include_modules!();

/// Handoff from the auth thread to the main thread across the event-loop quit.
static PENDING: Mutex<Option<(String, String, usize)>> = Mutex::new(None);

/// VT switch requested from the greeter UI (0 = none). The switch is performed
/// *after* the UI/DRM/seat are torn down (see `main`), never from inside the
/// running event loop: Slint's linuxkms backend cannot survive its seat being
/// disabled underneath it (`SeatEvent::Disable => unimplemented!`, or an EACCES
/// on the next page flip, which is the "Permission denied" crash we saw).
static SWITCH_TO: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn active_vt() -> Option<i32> {
    std::fs::read_to_string("/sys/class/tty/tty0/active")
        .ok()?
        .trim()
        .strip_prefix("tty")?
        .parse()
        .ok()
}

/// The VT this greeter *belongs to*: the VT of its own stdin tty (the unit sets
/// `TTYPath=/dev/tty1`), then `$XDG_VTNR`, and only then whichever VT happens
/// to be in front. Using the foreground VT first (the old behaviour) made every
/// respawn land on whatever VT you had just switched to, so velogin hijacked
/// tty2/tty3 and registered its logind session on the wrong VT.
fn own_vt() -> i32 {
    let from_tty = unsafe {
        let name = libc::ttyname(0);
        if name.is_null() {
            None
        } else {
            std::ffi::CStr::from_ptr(name)
                .to_str()
                .ok()
                .and_then(|s| s.strip_prefix("/dev/tty"))
                .and_then(|n| n.parse::<i32>().ok())
        }
    };
    from_tty
        .or_else(|| std::env::var("XDG_VTNR").ok()?.parse().ok())
        .or_else(active_vt)
        .unwrap_or(1)
}

/// Idle (no DRM, no logind session, no UI) until `vt` is in front. After a
/// VT switch the greeter exits and systemd respawns it at once; the respawn must
/// not grab the display from the VT you switched to, so it waits here until you
/// come back. A 0.3s sysfs poll costs nothing.
fn wait_until_vt_active(vt: i32) {
    if active_vt().is_none_or(|a| a == vt) {
        return;
    }
    eprintln!("[velogin] tty{vt} is not in front; waiting for it");
    while active_vt().is_some_and(|a| a != vt) {
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// Owns the forked process that holds the greeter's logind session open.
/// Dropping it closes the keepalive pipe (the holder sees EOF, closes its PAM
/// session, exits) and reaps it, so the session is gone before we go on.
struct GreeterHolder {
    keepalive: i32,
    pid: libc::pid_t,
}

impl Drop for GreeterHolder {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.keepalive);
            let mut st = 0;
            libc::waitpid(self.pid, &mut st, 0);
        }
    }
}

/// Register the greeter with logind from a *forked holder process* and return
/// its session id.
///
/// pam_systemd moves the process that calls `pam_open_session` into the new
/// session's scope. If the main velogin process did that, it would still be
/// "in a session" when it later opens the *user's* PAM session, and logind
/// refuses to create a second session for a process that is already in one
/// ("Already running in a session"). pam_systemd treats that as success and
/// only logs at debug level, so the login appeared to work: PAM said OK, no
/// logind session for the user was ever created, and the compositor's libseat
/// (logind backend) died in ~300 ms. With the holder, the main process never
/// joins a session; it reads the id and exports `XDG_SESSION_ID`, which
/// libseat uses when the process is not itself in a session.
fn open_greeter_session(vt: i32) -> Option<(GreeterHolder, String)> {
    let mut info = [0i32; 2];
    let mut keep = [0i32; 2];
    unsafe {
        if libc::pipe2(info.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return None;
        }
        if libc::pipe2(keep.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            libc::close(info[0]);
            libc::close(info[1]);
            return None;
        }
    }
    // Forked before Slint/GL start any thread, so running ordinary Rust (PAM)
    // in the child is safe.
    match unsafe { libc::fork() } {
        -1 => {
            unsafe {
                for fd in [info[0], info[1], keep[0], keep[1]] {
                    libc::close(fd);
                }
            }
            None
        }
        0 => {
            unsafe {
                libc::close(info[0]);
                libc::close(keep[1]);
            }
            greeter_holder_child(vt, info[1], keep[0])
        }
        pid => {
            unsafe {
                libc::close(info[1]);
                libc::close(keep[0]);
            }
            let holder = GreeterHolder {
                keepalive: keep[1],
                pid,
            };
            let mut buf = [0u8; 32];
            let n = loop {
                let n = unsafe { libc::read(info[0], buf.as_mut_ptr() as *mut _, buf.len()) };
                if n >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break n;
                }
            };
            unsafe { libc::close(info[0]) };
            let id = if n > 0 {
                String::from_utf8_lossy(&buf[..n as usize])
                    .trim()
                    .to_string()
            } else {
                String::new()
            };
            if id.is_empty() || id == "-" {
                return None; // holder exits on its own; Drop reaps it
            }
            Some((holder, id))
        }
    }
}

/// Child side of [`open_greeter_session`]: open the PAM/logind session, report
/// its id, then park until the parent closes the keepalive pipe (or dies, which
/// also reads as EOF, so a crashed greeter can't leak its session).
fn greeter_holder_child(vt: i32, info_wr: i32, keep_rd: i32) -> ! {
    let fail = |msg: &str| -> ! {
        eprintln!("[velogin] greeter session: {msg}");
        unsafe {
            libc::write(info_wr, b"-".as_ptr() as *const _, 1);
            libc::_exit(1)
        }
    };
    let Ok(mut ctx) = pam_client2::Context::new("velogin", Some("root"), GreeterConv) else {
        fail("PAM context failed")
    };
    let _ = ctx.set_tty(Some(&format!("tty{vt}")));
    let _ = ctx.putenv(format!("XDG_VTNR={vt}"));
    let _ = ctx.putenv("XDG_SEAT=seat0");
    let _ = ctx.putenv("XDG_SESSION_CLASS=greeter");
    let _ = ctx.putenv("XDG_SESSION_TYPE=wayland");
    let session = match ctx.open_session(pam_client2::Flag::NONE) {
        Ok(s) => s,
        Err(e) => fail(&format!("open_session failed: {e}")),
    };
    let mut id = String::new();
    for (k, v) in session.envlist().iter_tuples() {
        if k == "XDG_SESSION_ID" {
            id = v.to_string_lossy().into_owned();
        }
    }
    if id.is_empty() {
        fail("pam_systemd did not create a logind session (no XDG_SESSION_ID)");
    }
    unsafe {
        libc::write(
            info_wr,
            format!("{id}\n").as_ptr() as *const _,
            id.len() + 1,
        )
    };
    unsafe { libc::close(info_wr) };
    let mut b = [0u8; 1];
    loop {
        let n = unsafe { libc::read(keep_rd, b.as_mut_ptr() as *mut _, 1) };
        if n == 0 || (n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR))
        {
            break;
        }
    }
    drop(session); // pam_close_session
    drop(ctx);
    unsafe { libc::_exit(0) }
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
}

struct GreeterConv;

impl pam_client2::ConversationHandler for GreeterConv {
    fn prompt_echo_on(
        &mut self,
        _prompt: &std::ffi::CStr,
    ) -> Result<std::ffi::CString, pam_client2::ErrorCode> {
        Ok(std::ffi::CString::default())
    }
    fn prompt_echo_off(
        &mut self,
        _prompt: &std::ffi::CStr,
    ) -> Result<std::ffi::CString, pam_client2::ErrorCode> {
        Ok(std::ffi::CString::default())
    }
    fn text_info(&mut self, _msg: &std::ffi::CStr) {}
    fn error_msg(&mut self, _msg: &std::ffi::CStr) {}
}

/// Boolean env var: unset, empty, `0`, `false`, `no`, `off` are all false.
/// (`VELOGIN_DRY_RUN=0` used to *enable* dry run because only presence counted.)
fn parse_flag(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| parse_flag(&v))
}

/// `fd` is a pipe (the password pipe the greeter hands over), not a regular
/// file, tty or a stray low fd someone smuggled in through argv.
fn fd_is_pipe(fd: i32) -> bool {
    if fd <= 2 {
        return false;
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::fstat(fd, &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFIFO }
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

    let dry_run = env_flag("VELOGIN_DRY_RUN");
    check_prerequisites(dry_run);

    let sessions = session::detect();
    eprintln!(
        "[velogin] sessions: {:?}",
        sessions.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );

    // Pinned to our own VT, not whichever is in front (see own_vt). If the user
    // is on another VT right now (we were respawned after a VT switch), idle
    // here instead of grabbing the display from them.
    let current_tty = own_vt();
    let bare_tty = !dry_run
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
        && std::env::var_os("DISPLAY").is_none()
        && unsafe { libc::geteuid() } == 0;
    if bare_tty {
        wait_until_vt_active(current_tty);
    }

    // Register an active greeter session with systemd-logind (seat0/tty{current_tty})
    // so libseat can acquire DRM master cleanly over D-Bus without seatd. Done
    // from a forked holder process: see open_greeter_session for why this
    // process must never join a logind session itself.
    let greeter_holder = if bare_tty {
        eprintln!("[velogin] opening logind greeter session on seat0/tty{current_tty}…");
        match open_greeter_session(current_tty) {
            Some((holder, id)) => {
                std::env::set_var("XDG_SESSION_ID", id);
                std::env::set_var("XDG_SEAT", "seat0");
                std::env::set_var("XDG_VTNR", current_tty.to_string());
                std::env::set_var("LIBSEAT_BACKEND", "logind");
                Some(holder)
            }
            None => {
                eprintln!("[velogin] warning: failed to open logind greeter session");
                None
            }
        }
    } else {
        None
    };

    let ui = match LoginWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("[velogin] failed to create UI: {e}");
            std::process::exit(1);
        }
    };

    // ── static setup ────────────────────────────────────────────────────────
    let names: Vec<slint::SharedString> = sessions.iter().map(|s| s.name.as_str().into()).collect();
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
    if let Some(buf) = state::wallpaper_path().and_then(|p| decode_image(&p, &WALLPAPER_CAPS)) {
        // Derive the whole UI palette from the wallpaper (sampled, ~4k pixels).
        // No wallpaper, or one that fails to decode, keeps the sage defaults
        // baked into the Slint `Theme` global.
        let palette = theme::extract_palette(buf.as_slice(), buf.width(), buf.height());
        theme::apply(&ui, &palette);
        ui.set_wallpaper(slint::Image::from_rgba8(buf));
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
    // Don't switch from inside the event loop: quit it, and `main` switches
    // after the display and seat have been released (see SWITCH_TO).
    ui.on_switch_tty(|n| {
        if n > 0 {
            SWITCH_TO.store(n, std::sync::atomic::Ordering::SeqCst);
            let _ = slint::quit_event_loop();
        }
    });

    // Oh-shit key (HOME, bound in velogin.slint's killswitch
    // FocusScope): bail exactly like closing the dev window — quit the event
    // loop with no PENDING login set, so the post-run() teardown below (drop
    // ui → release DRM/libseat, close_lingering_fds) runs and we exit
    // without spawning a session.
    ui.on_emergency_exit(|| {
        eprintln!("[velogin] emergency exit (HOME) — bailing out");
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
        let session_names: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
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
                            *PENDING.lock().unwrap() = Some((username, password, session_idx));
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

    // Release the window/backend (DRM master) and logind greeter session before spawning.
    drop(ui);
    drop(greeter_holder);
    close_lingering_fds();

    // VT switch requested from the UI: now that the display and seat are
    // released, switch, then exit. systemd respawns us immediately and the
    // respawn idles in wait_until_vt_active() until our VT is back in front.
    let target = SWITCH_TO.swap(0, std::sync::atomic::Ordering::SeqCst);
    if target > 0 {
        switch_vt(target);
        return;
    }

    // ── launch session, if a login succeeded ────────────────────────────────
    let Some((username, password, idx)) = PENDING.lock().unwrap().take() else {
        return; // window closed without login (dev mode)
    };
    let entry = sessions
        .get(idx)
        .cloned()
        .unwrap_or_else(|| sessions[0].clone());
    eprintln!(
        "[velogin] logging {username} into {:?} ({})",
        entry.name, entry.exec
    );

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
fn reexec_into_session(
    username: &str,
    password: &str,
    entry: &session::SessionEntry,
) -> std::io::Error {
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
            unsafe {
                libc::close(wr);
                libc::close(rd);
            }
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

    // Exec via /proc/self/exe, NOT current_exe(): if a package upgrade replaced
    // the binary while this greeter sat idle, current_exe() resolves to
    // "/usr/local/bin/velogin (deleted)" and the execv below fails *after* a
    // successful auth. /proc/self/exe re-execs the already-running inode no
    // matter what happened to its path.
    let exe_c = c"/proc/self/exe";
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
    // Only reached if that failed (/proc not mounted, which shouldn't happen
    // under systemd): fall back to the on-disk path before giving up.
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(path) = CString::new(exe.as_os_str().as_bytes()) {
            unsafe { libc::execv(path.as_ptr(), ptrs.as_ptr()) };
        }
    }
    std::io::Error::last_os_error() // execv only returns on failure
}

/// Stage 2: the clean session helper. Reads the password back from the inherited
/// pipe fd and runs the real PAM session + fork/exec via [`spawn::launch`], which
/// waits for the session and closes PAM when it ends. Then we exit and systemd
/// (Restart=always) respawns the greeter.
fn run_session_helper(argv: &[String]) {
    // The helper does no authentication of its own beyond what PAM does with
    // the piped password, and it trusts argv for user/exec. Only the greeter
    // (root) is supposed to start it; refuse anyone else outright, and refuse
    // a password fd that isn't the pipe the greeter made.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("[velogin:session] refusing to run: the session helper must be started by the root greeter");
        std::process::exit(1);
    }
    let username = argv.get(2).cloned().unwrap_or_default();
    let name = argv.get(3).cloned().unwrap_or_default();
    let exec = argv.get(4).cloned().unwrap_or_default();
    let rd: i32 = argv.get(5).and_then(|s| s.parse().ok()).unwrap_or(-1);
    if !fd_is_pipe(rd) {
        eprintln!("[velogin:session] refusing to run: fd {rd} is not the password pipe");
        std::process::exit(1);
    }

    // Drains and closes the pipe fd. The password is needed here: launch()
    // authenticates on the same PAM handle it opens the session on, which is
    // what lets gnome-keyring's session hook unlock the login keyring.
    let password = read_pipe_password(rd);
    let entry = session::SessionEntry { name, exec };
    eprintln!(
        "[velogin:session] clean helper up, launching {username} into {:?}",
        entry.name
    );
    match spawn::launch(&username, password, &entry) {
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

/// Limits for decoding an image file. Avatars come from a user-writable
/// settings panel and are decoded *as root before login*, so they are hard
/// capped (file size, dimensions, decoder allocation) and decoded off the UI
/// thread. The wallpaper lives in root-owned `/etc/velogin` and gets roomier
/// limits (a 5K/8K background is legitimate).
struct ImageCaps {
    max_bytes: u64,
    max_dim: u32,
    max_alloc: u64,
}

const AVATAR_CAPS: ImageCaps = ImageCaps {
    max_bytes: 2 * 1024 * 1024,
    max_dim: 2048,
    max_alloc: 64 * 1024 * 1024,
};

const WALLPAPER_CAPS: ImageCaps = ImageCaps {
    max_bytes: 64 * 1024 * 1024,
    max_dim: 16384,
    max_alloc: 1024 * 1024 * 1024,
};

type Pixels = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

/// Latest avatar request; a decode that finishes for an older one is dropped.
static AVATAR_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn avatar_cache() -> &'static Mutex<std::collections::HashMap<String, Option<Pixels>>> {
    static CACHE: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Option<Pixels>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn apply_avatar(ui: &LoginWindow, px: Option<Pixels>) {
    match px {
        Some(buf) => {
            ui.set_avatar(slint::Image::from_rgba8(buf));
            ui.set_has_avatar(true);
        }
        None => ui.set_has_avatar(false),
    }
}

/// Update the initial letter at once; the avatar image only when a file exists,
/// from cache or from a worker thread (never a decode on the UI thread, and
/// never twice for the same user, since this runs on every typed character).
fn load_avatar(ui: &LoginWindow, username: &str) {
    use std::sync::atomic::Ordering::SeqCst;
    let name = username.trim();
    let initial = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".into());
    ui.set_user_initial(initial.into());

    let generation = AVATAR_GEN.fetch_add(1, SeqCst) + 1;
    let Some(path) = state::avatar_path(name) else {
        ui.set_has_avatar(false);
        return;
    };
    let cached = avatar_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned();
    if let Some(px) = cached {
        apply_avatar(ui, px);
        return;
    }
    ui.set_has_avatar(false);
    let weak = ui.as_weak();
    let key = name.to_string();
    std::thread::spawn(move || {
        let px = decode_image(&path, &AVATAR_CAPS);
        avatar_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, px.clone());
        let _ = slint::invoke_from_event_loop(move || {
            if AVATAR_GEN.load(SeqCst) != generation {
                return;
            }
            if let Some(ui) = weak.upgrade() {
                apply_avatar(&ui, px);
            }
        });
    });
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
/// extensionless, which Slint's own load_from_path can't identify. Reads at
/// most `max_bytes + 1` (so a huge or endless file can't balloon memory) and
/// runs the decoder under dimension and allocation limits. `None` on anything
/// over a cap or not a PNG/JPEG.
fn decode_image(path: &std::path::Path, caps: &ImageCaps) -> Option<Pixels> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(caps.max_bytes + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > caps.max_bytes {
        eprintln!(
            "[velogin] {} is over {} bytes; ignoring",
            path.display(),
            caps.max_bytes
        );
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(caps.max_dim);
    limits.max_image_height = Some(caps.max_dim);
    limits.max_alloc = Some(caps.max_alloc);
    reader.limits(limits);
    let img = match reader.decode() {
        Ok(i) => i.into_rgba8(),
        Err(e) => {
            eprintln!("[velogin] cannot decode {}: {e}", path.display());
            return None;
        }
    };
    let (w, h) = img.dimensions();
    Some(Pixels::clone_from_slice(img.as_raw(), w, h))
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
            eprintln!(
                "[velogin] open /dev/tty0 failed: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        if libc::ioctl(fd, VT_ACTIVATE as _, n) < 0 {
            eprintln!(
                "[velogin] VT_ACTIVATE failed: {}",
                std::io::Error::last_os_error()
            );
        }
        libc::close(fd);
    }
}

/// Force-close every fd above stderr in this still-running process.
///
/// `drop(ui)` tears down Slint's linuxkms backend at the Rust level. Closing
/// any lingering device descriptors ensures that graphics/input fds are cleanly
/// released so that the incoming compositor can acquire DRM master via logind
/// without contention.
fn close_lingering_fds() {
    // Use opendir/readdir/dirfd directly so we can exclude the directory's
    // own fd from the close list.  The previous read_dir approach collected
    // all fds and then closed them, but ReadDir closes its internal dirfd
    // on drop (which happens after collect()) — so we'd double-close that
    // fd number in the loop, potentially hitting a reused descriptor.
    let dir = unsafe { libc::opendir(c"/proc/self/fd".as_ptr()) };
    if dir.is_null() {
        return;
    }
    let dir_fd = unsafe { libc::dirfd(dir) };
    let mut fds = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            break;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(name: &str, w: u32, h: u32) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("velogin-img-{name}-{}", std::process::id()));
        image::RgbaImage::new(w, h)
            .save_with_format(&p, image::ImageFormat::Png)
            .unwrap();
        p
    }

    #[test]
    fn flag_parsing() {
        for t in ["1", "true", "yes", "on", "TRUE", " 1 ", "anything"] {
            assert!(parse_flag(t), "{t:?}");
        }
        for f in ["", "0", "false", "no", "off", "OFF", "  "] {
            assert!(!parse_flag(f), "{f:?}");
        }
    }

    #[test]
    fn fd_is_pipe_accepts_pipes_only() {
        let mut p = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        assert!(fd_is_pipe(p[0]));
        let f = std::fs::File::open("/dev/null").unwrap();
        use std::os::fd::AsRawFd;
        assert!(!fd_is_pipe(f.as_raw_fd()));
        assert!(!fd_is_pipe(0) && !fd_is_pipe(-1) && !fd_is_pipe(2));
        unsafe {
            libc::close(p[0]);
            libc::close(p[1]);
        }
    }

    #[test]
    fn decode_accepts_small_png() {
        let p = write_png("ok", 16, 8);
        let px = decode_image(&p, &AVATAR_CAPS).expect("decodes");
        assert_eq!((px.width(), px.height()), (16, 8));
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn decode_rejects_oversize_dimensions_bytes_and_garbage() {
        let wide = write_png("wide", 3000, 4);
        assert!(
            decode_image(&wide, &AVATAR_CAPS).is_none(),
            "3000px > 2048 cap"
        );
        assert!(
            decode_image(&wide, &WALLPAPER_CAPS).is_some(),
            "fine for a wallpaper"
        );
        let tiny_cap = ImageCaps {
            max_bytes: 10,
            max_dim: 2048,
            max_alloc: 1 << 20,
        };
        assert!(
            decode_image(&wide, &tiny_cap).is_none(),
            "over the byte cap"
        );
        std::fs::remove_file(wide).unwrap();

        let junk = std::env::temp_dir().join(format!("velogin-img-junk-{}", std::process::id()));
        std::fs::write(&junk, b"not an image at all").unwrap();
        assert!(decode_image(&junk, &AVATAR_CAPS).is_none());
        std::fs::remove_file(junk).unwrap();
    }
}
