//! PAM session + fork/setuid/exec into the chosen session.
//!
//! Runs on the main thread *after* the Slint event loop has quit and the UI
//! has been dropped — the linuxkms backend must have released DRM master
//! before the child (e.g. veil-host in DRM mode) tries to take it.
//!
//! Standard login(1) choreography: open the PAM session as root, fork, and in
//! the child acquire the TTY as controlling terminal, drop to the user's
//! uid/gid, set up the environment (PAM env list + login basics), then exec
//! the session through the user's shell as a login shell. The parent waits,
//! closes the PAM session, and exits — systemd (Restart=always) respawns us.

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicI32, Ordering};

use pam_client2::{conv_null, Context, ConversationHandler, ErrorCode, Flag};
use uzers::os::unix::UserExt;

use crate::session::SessionEntry;

/// Conversation handler for the *visible* greeter stage (see [`verify`]).
///
/// Non-interactively supplies the given username/password like conv_mock
/// did, but — unlike conv_mock, which is a scripted/headless handler that
/// silently logs `text_info`/`error_msg` into a `log` field nobody reads —
/// forwards those messages to a caller-supplied callback. This is what
/// carries pam_u2f's `cue` prompt ("touch your security key") to the UI
/// instead of it being swallowed, which was the root cause of auth just
/// sitting there with no feedback.
struct UiConversation<F: FnMut(String) + Send> {
    username: CString,
    password: CString,
    on_message: F,
}

impl<F: FnMut(String) + Send> UiConversation<F> {
    fn new(username: &str, password: &str, on_message: F) -> Self {
        Self {
            username: CString::new(username).unwrap_or_default(),
            password: CString::new(password).unwrap_or_default(),
            on_message,
        }
    }
}

impl<F: FnMut(String) + Send> ConversationHandler for UiConversation<F> {
    fn prompt_echo_on(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
        Ok(self.username.clone())
    }

    fn prompt_echo_off(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
        Ok(self.password.clone())
    }

    fn text_info(&mut self, msg: &CStr) {
        (self.on_message)(msg.to_string_lossy().into_owned());
    }

    fn error_msg(&mut self, msg: &CStr) {
        (self.on_message)(msg.to_string_lossy().into_owned());
    }
}

/// Session-group pid the parent is currently waiting on, so a SIGTERM (from
/// `systemctl stop/restart`) can be forwarded instead of just killing us.
/// Signal-handler-safe: an atomic, no locks, no allocation.
static SESSION_PGID: AtomicI32 = AtomicI32::new(0);

/// Mirrors ly's `sessionSignalHandler` (src/auth.zig): without this, systemd
/// stopping/restarting the greeter mid-session kills the parent immediately
/// via the default SIGTERM disposition, skipping `drop(session)` below —
/// which means `pam_close_session()` never runs and logind is left holding a
/// session that's never told the session ended. Repeat that a few times and
/// the *next* login's `pam_systemd` session-open hook hangs reconciling
/// against that stale state (observed: `ctx.open_session()` never returning,
/// confirmed via loginctl showing an orphaned seat0/tty1 session still
/// attributed to the previous login after several forced restarts).
extern "C" fn forward_sigterm(_sig: libc::c_int) {
    let pgid = SESSION_PGID.load(Ordering::SeqCst);
    if pgid > 0 {
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("PAM: {0}")]
    Pam(#[from] pam_client2::Error),
    #[error("unknown user {0:?}")]
    UnknownUser(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("user field contains NUL")]
    BadString(#[from] std::ffi::NulError),
}

/// Authenticate only — used by the UI thread to validate credentials before
/// tearing down the greeter. Full session setup happens later in [`launch`],
/// which does *not* re-authenticate (see there for why).
///
/// `on_message` fires on this thread for every `text_info`/`error_msg` PAM
/// emits mid-authenticate (e.g. pam_u2f's `cue`: "touch your security key")
/// — the caller is expected to marshal it onto the UI thread.
pub fn verify(
    username: &str,
    password: &str,
    on_message: impl FnMut(String) + Send + 'static,
) -> Result<(), pam_client2::Error> {
    let conv = UiConversation::new(username, password, on_message);
    let mut ctx = Context::new("velogin", Some(username), conv)?;
    ctx.authenticate(Flag::NONE)?;
    ctx.acct_mgmt(Flag::NONE)?;
    Ok(())
}

pub fn launch(username: &str, entry: &SessionEntry) -> Result<(), LaunchError> {
    let user = uzers::get_user_by_name(username)
        .ok_or_else(|| LaunchError::UnknownUser(username.to_string()))?;
    let uid = user.uid();
    let gid = user.primary_group_id();
    let home = user.home_dir().to_path_buf();
    let shell = user.shell().to_path_buf();
    let shell = if shell.as_os_str().is_empty() { "/bin/sh".into() } else { shell };

    // Debug breadcrumb trail — opened up front so it brackets the *entire*
    // post-auth path, not just the fork/exec handoff. Temporary: goes away
    // once the hang between "PAM session opened" and the fork is actually
    // root-caused instead of guessed at.
    let dbg_fd = unsafe {
        libc::open(
            c"/tmp/velogin-fork-trace.log".as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
            0o644,
        )
    };
    fn dbg(fd: i32, msg: &str) {
        if fd >= 0 {
            unsafe { libc::write(fd, msg.as_ptr() as *const _, msg.len()) };
        }
    }

    // Credentials were already verified once in `verify()`, on the UI
    // thread, in the still-visible greeter. Calling `ctx.authenticate()`
    // again here would re-run the whole PAM auth stack from scratch in this
    // headless process — no window, no conversation UI — which for a
    // pam_u2f stack means silently demanding a *second* physical touch of
    // the security key with nothing on screen to say so. That's the
    // "sits there" bug. This is pam-client2's own sanctioned pattern for
    // "already authenticated by other means": conv_null + a preset
    // username, skip authenticate(), go straight to acct_mgmt() +
    // open_session().
    let mut ctx = Context::new("velogin", Some(username), conv_null::Conversation::new())?;
    ctx.acct_mgmt(Flag::NONE)?;
    dbg(dbg_fd, "acct_mgmt done (no re-auth — see comment)\n");

    // Controlling TTY (systemd TTYPath=/dev/tty1) and its VT number. Computed
    // *before* open_session because pam_systemd needs the VT to register an
    // activatable session (see below); also reused for the fork/exec handoff.
    let tty_c: Option<CString> = {
        let name = unsafe { libc::ttyname(0) };
        if name.is_null() {
            None
        } else {
            Some(unsafe { std::ffi::CStr::from_ptr(name) }.to_owned())
        }
    };
    let vtnr = tty_c
        .as_ref()
        .and_then(|t| t.to_str().ok())
        .and_then(|s| s.trim_start_matches("/dev/tty").parse::<u32>().ok());
    dbg(dbg_fd, "ttyname/vtnr done\n");

    // ── make this an *activatable* seat0 VT session ──────────────────────────
    // pam_systemd reads the seat, VTNr and tty at open_session() time and hands
    // them to logind. Without them logind registers the session with no VTNr,
    // never marks it Active, and the compositor's libseat/logind backend then
    // times out waiting for DRM master → "Session could not be activated in
    // time" → Hyprland aborts. (seatd isn't an option for the child: /run/
    // seatd.sock is 0770 root:seat and the user isn't in the seat group → EPERM,
    // which is exactly why aquamarine falls through to logind.) A working DM
    // session shows VTNr/Seat=seat0/Active=yes — we must reproduce that. These
    // MUST be set before open_session; the child exec-env copies below are far
    // too late for pam_systemd.
    if let Some(vt) = vtnr {
        // PAM_TTY as the bare VT name ("tty1") is what logind records as the
        // session TTY; XDG_VTNR pins the VT so it activates when foreground
        // (it is — the greeter was just rendering on it).
        let _ = ctx.set_tty(Some(&format!("tty{vt}")));
        let _ = ctx.putenv(format!("XDG_VTNR={vt}"));
    }
    let _ = ctx.putenv("XDG_SEAT=seat0");
    let _ = ctx.putenv("XDG_SESSION_CLASS=user");

    // gkr-pam's session hook (auto_start) reads getenv("XDG_RUNTIME_DIR") to
    // find/create /run/user/<uid>/keyring for its control socket. pam_systemd
    // sets it in the PAM handle env, but gkr-pam uses the *process* environ,
    // so pre-seed it here where a plain getenv() will find it.
    //
    // We deliberately do NOT pre-seed DBUS_SESSION_BUS_ADDRESS. An earlier
    // build did, on the theory that libdbus would otherwise fall back to an
    // X11 session-bus autostart — but that fallback only triggers when DISPLAY
    // is set, and we never set it on a bare TTY. What the pre-seed actually did
    // was point gnome-keyring-daemon at /run/user/<uid>/bus, a systemd socket-
    // activated address whose broker (user@.service) may not be up yet at
    // open_session() time → the daemon blocked on it and auto_start hung. sddm
    // never sets it and its keyring unlock works, so neither do we.
    std::env::set_var("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));

    // Session open runs pam_systemd (registers the logind session, spins up
    // user@.service) and then our gkr-pam auto_start line (forks + unlocks the
    // keyring daemon). The eprintln breadcrumbs bracket it in the *journal*
    // (persistent) rather than the /tmp trace file, which a reboot wipes.
    eprintln!("[velogin] opening PAM session (seat0/vt{} + keyring)…", vtnr.unwrap_or(0));
    let session = ctx.open_session(Flag::NONE)?;
    eprintln!("[velogin] PAM session opened, proceeding to fork/exec");
    dbg(dbg_fd, "open_session done\n");

    // ── prepare EVERYTHING the child needs before forking ──────────────────
    // (no allocation between fork and exec)
    let mut env: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
        std::collections::BTreeMap::new();
    for (k, v) in session.envlist().iter_tuples() {
        env.insert(k.as_bytes().to_vec(), v.as_bytes().to_vec());
    }
    dbg(dbg_fd, "envlist collected\n");
    let mut set_default = |key: &str, val: String| {
        env.entry(key.as_bytes().to_vec())
            .or_insert_with(|| val.into_bytes());
    };
    set_default("HOME", home.to_string_lossy().into_owned());
    set_default("USER", username.to_string());
    set_default("LOGNAME", username.to_string());
    set_default("SHELL", shell.to_string_lossy().into_owned());
    set_default(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin".to_string(),
    );
    set_default("TERM", "linux".to_string());

    // Belt-and-suspenders: also seed the same XDG_* into the child's exec env
    // (the compositor reads them directly too). tty_c/vtnr were computed up top,
    // before open_session, so pam_systemd/logind already have them. Mirroring ly
    // (src/auth.zig setXdgEnv): don't force LIBSEAT_BACKEND — let libseat auto-
    // detect (→ logind on our now properly-registered, active seat0 session).
    set_default("XDG_SEAT", "seat0".to_string());
    if let Some(vt) = vtnr {
        set_default("XDG_VTNR", vt.to_string());
    }
    set_default("XDG_SESSION_TYPE", "wayland".to_string());
    set_default("XDG_SESSION_CLASS", "user".to_string());
    set_default("XDG_CURRENT_DESKTOP", "veil".to_string());
    set_default("XDG_SESSION_DESKTOP", "veil".to_string());
    set_default("ELECTRON_OZONE_PLATFORM_HINT", "wayland".to_string());
    set_default("OZONE_PLATFORM", "wayland".to_string());
    set_default("MOZ_ENABLE_WAYLAND", "1".to_string());
    set_default("QT_QPA_PLATFORM", "wayland".to_string());
    set_default("GDK_BACKEND", "wayland".to_string());
    set_default("SDL_VIDEODRIVER", "wayland".to_string());
    let runtime_dir = format!("/run/user/{uid}");
    if std::path::Path::new(&runtime_dir).is_dir() {
        set_default("XDG_RUNTIME_DIR", runtime_dir);
    }

    // Hardcoded sessions (Veil = `veil-host start`) resolve through PATH, and
    // per-user installs live in ~/.local/bin — make sure it's reachable even
    // before the shell's own profile has run.
    let local_bin = format!("{}/.local/bin", home.to_string_lossy());
    if let Some(path) = env.get_mut(b"PATH".as_slice()) {
        let has_local = std::str::from_utf8(path)
            .map(|p| p.split(':').any(|d| d == local_bin))
            .unwrap_or(false);
        if !has_local {
            path.extend(format!(":{local_bin}").into_bytes());
        }
    }

    dbg(dbg_fd, "xdg env + path defaults done\n");

    let mut envp: Vec<CString> = Vec::new();
    for (k, mut v) in env {
        let mut kv = k;
        kv.push(b'=');
        kv.append(&mut v);
        if let Ok(c) = CString::new(kv) {
            envp.push(c);
        }
    }
    dbg(dbg_fd, "envp built\n");

    let shell_c = CString::new(shell.as_os_str().as_bytes().to_vec())?;
    // argv[0] as "-bash" style → login shell semantics
    let shell_name = shell.file_name().unwrap_or_default().to_string_lossy();
    let argv0 = CString::new(format!("-{shell_name}"))?;
    let argv: Vec<CString> = if entry.exec.is_empty() {
        vec![argv0]
    } else {
        vec![
            argv0,
            CString::new("-c")?,
            CString::new(format!("exec {}", entry.exec))?,
        ]
    };
    let mut argv_ptrs: Vec<*const libc::c_char> =
        argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const libc::c_char> =
        envp.iter().map(|e| e.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

    let user_c = CString::new(username)?;
    let home_c = CString::new(home.as_os_str().as_bytes().to_vec())?;
    dbg(dbg_fd, "argv/envp/user_c/home_c built\n");

    // Hand the TTY to the user for the duration of the session.
    if let Some(tty) = &tty_c {
        unsafe { libc::chown(tty.as_ptr(), uid, gid) };
    }
    dbg(dbg_fd, "tty chown done\n");

    // Resolve supplementary groups *here*, in the parent, where NSS lookups
    // and malloc are safe. initgroups(3) does both internally — calling it
    // after fork() in a process that's been running GPU/libseat threads is a
    // deadlock waiting to happen: if any other thread held glibc's malloc
    // arena lock (or the dynamic-linker lock, for NSS module dlopen) at the
    // instant of fork(), the child inherits that lock wedged forever and
    // hangs on its very first allocation. That hang holds the controlling
    // TTY and the libseat session open, which is also why VT switching can
    // freeze solid. setgroups(2) in the child is a plain syscall — no NSS,
    // no allocation, async-signal-safe.
    dbg(dbg_fd, "getgrouplist: starting\n");
    let mut groups: Vec<libc::gid_t> = vec![0; 32];
    let mut ngroups: libc::c_int = groups.len() as libc::c_int;
    loop {
        let rc = unsafe {
            libc::getgrouplist(
                user_c.as_ptr(),
                gid,
                groups.as_mut_ptr(),
                &mut ngroups,
            )
        };
        if rc >= 0 {
            groups.truncate(ngroups as usize);
            break;
        }
        // buffer too small — ngroups now holds the required size (always > 0
        // per POSIX, but guard against a bogus non-positive value defensively).
        let required = if ngroups > 0 { ngroups as usize } else { groups.len() };
        groups.resize(required.max(groups.len() * 2), 0);
    }
    dbg(dbg_fd, "getgrouplist: done\n");

    // ── fork ────────────────────────────────────────────────────────────────
    eprintln!("[velogin] forking session ({})", shell.to_string_lossy());
    match unsafe { libc::fork() } {
        -1 => Err(std::io::Error::last_os_error().into()),
        0 => {
            // child — async-signal-safe territory: syscalls only.
            unsafe {
                dbg(dbg_fd, "[child] start\n");
                libc::setsid();
                dbg(dbg_fd, "[child] setsid done\n");
                if let Some(tty) = &tty_c {
                    let fd = libc::open(tty.as_ptr(), libc::O_RDWR);
                    dbg(dbg_fd, "[child] tty open done\n");
                    if fd >= 0 {
                        libc::ioctl(fd, libc::TIOCSCTTY as _, 0);
                        dbg(dbg_fd, "[child] TIOCSCTTY done\n");
                        libc::dup2(fd, 0);
                        libc::dup2(fd, 1);
                        libc::dup2(fd, 2);
                        dbg(dbg_fd, "[child] dup2 done\n");
                        if fd > 2 {
                            libc::close(fd);
                        }
                    } else {
                        dbg(dbg_fd, "[child] tty open FAILED\n");
                    }
                }
                if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    dbg(dbg_fd, "[child] priv drop FAILED\n");
                    libc::_exit(1);
                }
                dbg(dbg_fd, "[child] priv drop done\n");
                let cd = libc::chdir(home_c.as_ptr());
                dbg(dbg_fd, if cd == 0 { "[child] chdir done\n" } else { "[child] chdir FAILED\n" });
                dbg(dbg_fd, "[child] about to execve\n");
                libc::execve(shell_c.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
                dbg(dbg_fd, "[child] execve RETURNED (failed)\n");
                libc::_exit(127);
            }
        }
        child => {
            if dbg_fd >= 0 {
                unsafe { libc::close(dbg_fd) };
            }
            // child called setsid(), so its pid is also its own process
            // group id — forwarding to -child reaches the whole session
            // tree (e.g. niri-session's descendants), not just the shell.
            SESSION_PGID.store(child, Ordering::SeqCst);
            unsafe { libc::signal(libc::SIGTERM, forward_sigterm as *const () as usize) };

            // parent: wait for the session to end, then tear down PAM.
            // Loop on EINTR — our SIGTERM handler returning (rather than
            // exiting) interrupts this syscall without restarting it.
            let mut status = 0i32;
            loop {
                let r = unsafe { libc::waitpid(child, &mut status, 0) };
                if r >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break;
                }
            }
            SESSION_PGID.store(0, Ordering::SeqCst);

            // Take the TTY back before the next greeter respawn.
            if let Some(tty) = &tty_c {
                unsafe { libc::chown(tty.as_ptr(), 0, 0) };
            }
            drop(session); // close PAM session (pam_close_session)
            Ok(())
        }
    }
}
