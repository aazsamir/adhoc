//! Small concurrency helpers used by the service lifecycle.
//!
//! There is exactly one genuinely tricky piece of synchronisation in adhoc: a
//! `stop` wants to *wait* until the app process is gone, while the thread that is
//! reaping that app wants nothing more than to be woken up and take the same
//! lock. A [`Latch`] captures that idea in one small type, and everything else
//! in the codebase can stay straightforward because of it.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// A one-shot "this happened" flag with a condition variable.
///
/// [`Latch::signal`] can be called more than once; waiters wake on the first
/// call and see [`Latch::is_set`] forever after.
#[derive(Default)]
pub struct Latch {
    set: Mutex<bool>,
    changed: Condvar,
}

impl Latch {
    pub fn new() -> Latch {
        Latch::default()
    }

    /// Sets the latch and wakes everyone waiting on it.
    pub fn signal(&self) {
        *lock(&self.set) = true;
        self.changed.notify_all();
    }

    pub fn is_set(&self) -> bool {
        *lock(&self.set)
    }

    /// Waits at most `timeout`; returns whether the latch is set.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let guard = lock(&self.set);
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, timeout, |set| !*set)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard
    }
}

/// Locks a mutex, ignoring poisoning.
///
/// A panic while holding one of our locks (a handler thread, say) must not take
/// the whole launcher down: the data behind these locks is either plain counters
/// or state we can still interpret. Recovering is the pragmatic choice.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn a_latch_times_out_then_wakes() {
        let latch = Arc::new(Latch::new());
        let started = Instant::now();
        assert!(!latch.wait_timeout(Duration::from_millis(50)));
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(!latch.is_set());

        let waiter = {
            let latch = Arc::clone(&latch);
            std::thread::spawn(move || latch.wait_timeout(Duration::from_secs(5)))
        };
        std::thread::sleep(Duration::from_millis(20));
        latch.signal();
        assert!(waiter.join().unwrap());
        assert!(latch.is_set());
        // Signalling twice is harmless.
        latch.signal();
        assert!(latch.wait_timeout(Duration::from_millis(1)));
    }

    #[test]
    fn locks_survive_a_panicking_holder() {
        let mutex = Mutex::new(7);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock(&mutex);
            panic!("boom");
        }));
        assert_eq!(*lock(&mutex), 7);
    }
}
