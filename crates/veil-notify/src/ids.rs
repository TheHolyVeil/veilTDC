//! The notification id counter, shared between the D-Bus thread (which must
//! return an id synchronously from `Notify`) and the main loop (which persists
//! it). Ids are never 0 and are not reused across a restart: the main loop
//! saves [`IdGen::peek`] with the history and raises the counter on load.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct IdGen(Arc<AtomicU32>);

impl IdGen {
    /// Start the counter at `start`; 0 is not a valid id, so it becomes 1.
    pub fn new(start: u32) -> IdGen {
        IdGen(Arc::new(AtomicU32::new(start.max(1))))
    }

    /// A fresh id. Wraps from `u32::MAX` to 1, skipping 0.
    pub fn alloc(&self) -> u32 {
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(if v == u32::MAX { 1 } else { v + 1 }))
            .unwrap_or(1)
    }

    /// The id the next `alloc` will return.
    pub fn peek(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    /// Make sure the next id is at least `floor` (used after loading history).
    pub fn raise_to(&self, floor: u32) {
        self.0.fetch_max(floor.max(1), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_nonzero_and_increase() {
        let g = IdGen::new(0);
        assert_eq!((g.alloc(), g.alloc()), (1, 2));
        assert_eq!(g.peek(), 3);
    }

    #[test]
    fn wraps_past_zero() {
        let g = IdGen::new(u32::MAX);
        assert_eq!(g.alloc(), u32::MAX);
        assert_eq!(g.alloc(), 1);
    }

    #[test]
    fn raise_never_lowers() {
        let g = IdGen::new(10);
        g.raise_to(5);
        assert_eq!(g.peek(), 10);
        g.raise_to(50);
        assert_eq!(g.alloc(), 50);
        g.raise_to(0);
        assert_eq!(g.peek(), 51);
    }

    #[test]
    fn clones_share_the_counter_across_threads() {
        let g = IdGen::new(1);
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let g = g.clone();
                std::thread::spawn(move || (0..250).map(|_| g.alloc()).collect::<Vec<_>>())
            })
            .collect();
        let mut all: Vec<u32> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 1000, "no id handed out twice");
    }
}
