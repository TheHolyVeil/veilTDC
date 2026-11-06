//! A wake-up for the main loop: an eventfd the D-Bus and IPC threads poke after
//! queueing an event. The loop `poll(2)`s it together with the Wayland sockets
//! and a timeout, so an idle notify process makes no wakeups of its own.
//!
//! Wakes coalesce: any number of `wake()` calls before a `drain()` is one
//! readable event, and the loop then drains every queue anyway.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub struct Waker {
    fd: OwnedFd,
}

impl Waker {
    pub fn new() -> io::Result<Waker> {
        // SAFETY: eventfd takes no pointers; on success it returns a new fd we own.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is valid, open and not owned by anything else.
        Ok(Waker { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    /// Make the fd readable. Never blocks and never fails the caller: if the
    /// counter is full the loop is already due to wake.
    pub fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: writing 8 bytes from a live u64 to a valid fd.
        let _ = unsafe { libc::write(self.fd.as_raw_fd(), (&one as *const u64).cast(), 8) };
    }

    /// Reset to not-readable.
    pub fn drain(&self) {
        let mut buf: u64 = 0;
        // SAFETY: reading 8 bytes into a live u64 from a valid fd.
        let _ = unsafe { libc::read(self.fd.as_raw_fd(), (&mut buf as *mut u64).cast(), 8) };
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// `poll(2)` the given fds for readability. Returns, per fd, whether it is
/// readable. `timeout` of `None` waits forever. Interrupted calls report nothing readable.
pub fn poll_readable(fds: &[RawFd], timeout: Option<std::time::Duration>) -> Vec<bool> {
    let mut set: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    let ms = match timeout {
        None => -1,
        // round up so a 100 µs deadline does not become a busy-spinning 0
        Some(d) => i32::try_from(d.as_micros().div_ceil(1000)).unwrap_or(i32::MAX),
    };
    // SAFETY: `set` is a live, correctly sized array of pollfd.
    let n = unsafe { libc::poll(set.as_mut_ptr(), set.len() as libc::nfds_t, ms) };
    if n <= 0 {
        return vec![false; fds.len()];
    }
    // POLLHUP/POLLERR count as readable: the read will then report the problem.
    set.iter().map(|p| p.revents != 0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn wake_makes_it_readable_and_drain_resets_it() {
        let w = Waker::new().unwrap();
        assert_eq!(poll_readable(&[w.fd()], Some(Duration::ZERO)), vec![false]);
        w.wake();
        assert_eq!(poll_readable(&[w.fd()], Some(Duration::ZERO)), vec![true]);
        w.drain();
        assert_eq!(poll_readable(&[w.fd()], Some(Duration::ZERO)), vec![false]);
    }

    #[test]
    fn many_wakes_are_one_event() {
        let w = Waker::new().unwrap();
        for _ in 0..1000 {
            w.wake();
        }
        w.drain();
        assert_eq!(poll_readable(&[w.fd()], Some(Duration::ZERO)), vec![false], "one drain clears them all");
    }

    #[test]
    fn a_wake_from_another_thread_unblocks_poll_promptly() {
        let w = std::sync::Arc::new(Waker::new().unwrap());
        let w2 = w.clone();
        let t = Instant::now();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            w2.wake();
        });
        assert_eq!(poll_readable(&[w.fd()], Some(Duration::from_secs(5))), vec![true]);
        assert!(t.elapsed() < Duration::from_secs(1), "took {:?}", t.elapsed());
        h.join().unwrap();
    }

    #[test]
    fn timeout_elapses_without_a_wake_and_reports_which_fd_is_ready() {
        let (a, b) = (Waker::new().unwrap(), Waker::new().unwrap());
        let t = Instant::now();
        assert_eq!(poll_readable(&[a.fd(), b.fd()], Some(Duration::from_millis(40))), vec![false, false]);
        assert!(t.elapsed() >= Duration::from_millis(35));
        b.wake();
        assert_eq!(poll_readable(&[a.fd(), b.fd()], Some(Duration::ZERO)), vec![false, true]);
    }

    #[test]
    fn sub_millisecond_timeouts_round_up_instead_of_spinning() {
        let w = Waker::new().unwrap();
        let t = Instant::now();
        poll_readable(&[w.fd()], Some(Duration::from_micros(300)));
        assert!(t.elapsed() >= Duration::from_micros(500), "returned after {:?}", t.elapsed());
    }
}
