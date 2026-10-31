//! Off-thread sampling for slow or blocking sources.
//!
//! The bar's main loop renders, answers the compositor's pings and handles
//! clicks. Anything that can block (a subprocess, an HTTP request, a mount
//! that went away) must not run there: a stall longer than the supervisor's
//! liveness window gets the whole bar killed. A [`Poller`] runs the sampler on
//! its own thread and hands the latest value over.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Shared<T> {
    value: Mutex<Option<T>>,
    version: AtomicU64,
    stop: AtomicBool,
}

pub struct Poller<T> {
    shared: Arc<Shared<T>>,
}

impl<T: Clone + Send + 'static> Poller<T> {
    /// `sample` runs on its own thread, first immediately, and returns the
    /// value plus how long to wait before the next sample (so a failed
    /// request can retry sooner than a successful one).
    pub fn spawn(name: &str, mut sample: impl FnMut() -> (T, Duration) + Send + 'static) -> Self {
        let shared = Arc::new(Shared {
            value: Mutex::new(None),
            version: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let spawned = std::thread::Builder::new().name(format!("veil-bar-{name}")).spawn(move || {
            while !worker.stop.load(Ordering::Relaxed) {
                let (v, wait) = sample();
                if worker.stop.load(Ordering::Relaxed) {
                    break;
                }
                *worker.value.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
                worker.version.fetch_add(1, Ordering::Release);
                // Sleep in slices so dropping the poller is noticed quickly.
                let mut left = wait;
                while left > Duration::ZERO && !worker.stop.load(Ordering::Relaxed) {
                    let slice = left.min(Duration::from_millis(100));
                    std::thread::sleep(slice);
                    left -= slice;
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("bar: could not start the {name} sampler: {e}");
        }
        Self { shared }
    }

    /// Sample at a fixed interval.
    pub fn every(name: &str, interval: Duration, mut sample: impl FnMut() -> T + Send + 'static) -> Self {
        Self::spawn(name, move || (sample(), interval))
    }

    /// The most recent sample, if there is one yet.
    pub fn latest(&self) -> Option<T> {
        self.shared.value.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Bumps with every new sample.
    pub fn version(&self) -> u64 {
        self.shared.version.load(Ordering::Acquire)
    }
}

impl<T> Drop for Poller<T> {
    fn drop(&mut self) {
        // Not joined: a sampler stuck in a hung subprocess must not hold up
        // a config reload. It exits when the call returns.
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Remembers which version of a poller's output a widget has drawn.
#[derive(Default)]
pub struct Seen(u64);

impl Seen {
    /// True once per new sample.
    pub fn changed<T: Clone + Send + 'static>(&mut self, p: &Poller<T>) -> bool {
        let v = p.version();
        if v != self.0 {
            self.0 = v;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn first_sample_arrives_without_waiting_for_the_interval() {
        let p = Poller::every("t", Duration::from_secs(60), || 7u32);
        assert!(wait_until(|| p.latest() == Some(7)));
    }

    #[test]
    fn resamples_and_bumps_the_version() {
        let mut n = 0u32;
        let p = Poller::every("t", Duration::from_millis(20), move || {
            n += 1;
            n
        });
        let mut seen = Seen::default();
        assert!(wait_until(|| seen.changed(&p)));
        assert!(wait_until(|| p.latest().is_some_and(|v| v >= 3)));
        assert!(wait_until(|| seen.changed(&p)), "a later sample is reported again");
    }

    #[test]
    fn a_slow_sampler_never_blocks_the_caller() {
        let p = Poller::every("t", Duration::from_secs(60), || {
            std::thread::sleep(Duration::from_secs(5));
            1u8
        });
        let start = Instant::now();
        assert_eq!(p.latest(), None);
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn dropping_stops_sampling() {
        use std::sync::atomic::AtomicU32;
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let p = Poller::every("t", Duration::from_millis(10), move || c.fetch_add(1, Ordering::Relaxed));
        assert!(wait_until(|| count.load(Ordering::Relaxed) >= 2));
        drop(p);
        std::thread::sleep(Duration::from_millis(150));
        let after = count.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(count.load(Ordering::Relaxed), after, "sampler kept running after drop");
    }
}
