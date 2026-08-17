//! Blocking-command wait queue (primitives only; the bounded blocking
//! thread-pool that runs BLPOP/BZPOPMIN/XREAD BLOCK arrives in a later
//! phase and lives in the resp layer).
//!
//! [`WaitHub`] maps a physical root key to a FIFO queue of [`Waiter`]s.
//! A blocking command [`register`]s a waiter, then [`wait`]s on it with a
//! timeout; a mutating command calls [`notify`] to pop and signal the
//! OLDEST waiter. Waiters are plain data carriers; all logic is free
//! functions.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// One parked blocking command. `signaled` is only touched under `mu`.
pub struct Waiter {
    signaled: Mutex<bool>,
    cv: Condvar,
}

/// Root-key -> FIFO waiter queue.
pub struct WaitHub {
    inner: Mutex<HashMap<Vec<u8>, VecDeque<Arc<Waiter>>>>,
}

impl WaitHub {
    pub fn new() -> WaitHub {
        WaitHub {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for WaitHub {
    fn default() -> Self {
        WaitHub::new()
    }
}

/// Outcome of parking on a waiter.
#[derive(Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Signaled,
    Timeout,
}

/// Fresh, un-signaled waiter.
pub fn new_waiter() -> Waiter {
    Waiter {
        signaled: Mutex::new(false),
        cv: Condvar::new(),
    }
}

/// Push a waiter for `key`; returns the waiter handle to park on.
pub fn register(hub: &WaitHub, key: &[u8]) -> Arc<Waiter> {
    let waiter = Arc::new(new_waiter());
    let mut map = hub
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.entry(key.to_vec())
        .or_default()
        .push_back(Arc::clone(&waiter));
    waiter
}

/// Signal ONE waiter (the oldest) for `key` and pop it from the queue.
/// A key with no waiters is a no-op. Returns whether anyone was woken.
pub fn notify(hub: &WaitHub, key: &[u8]) -> bool {
    let mut map = hub
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match map.get_mut(key).and_then(VecDeque::pop_front) {
        Some(waiter) => {
            signal(&waiter);
            if map.get(key).is_some_and(|q| q.is_empty()) {
                map.remove(key);
            }
            true
        }
        None => false,
    }
}

/// Timeout cleanup: drop a waiter that was never signaled.
pub fn unregister(hub: &WaitHub, key: &[u8], waiter: &Arc<Waiter>) {
    let mut map = hub
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(queue) = map.get_mut(key) {
        queue.retain(|w| !Arc::ptr_eq(w, waiter));
        if queue.is_empty() {
            map.remove(key);
        }
    }
}

/// Signal a waiter: wakes [`wait`] with `WaitOutcome::Signaled`.
pub fn signal(waiter: &Waiter) {
    let mut signaled = waiter
        .signaled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *signaled = true;
    drop(signaled);
    waiter.cv.notify_all();
}

/// Park until signaled or `timeout` elapses.
pub fn wait(waiter: &Waiter, timeout: Duration) -> WaitOutcome {
    let deadline = std::time::Instant::now() + timeout;
    let mut signaled = waiter
        .signaled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !*signaled {
        let now = std::time::Instant::now();
        if now >= deadline {
            return WaitOutcome::Timeout;
        }
        let (guard, result) = waiter
            .cv
            .wait_timeout(signaled, deadline - now)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        signaled = guard;
        if !result.timed_out() && !*signaled {
            continue; // spurious wakeup
        }
        if result.timed_out() && !*signaled {
            return WaitOutcome::Timeout;
        }
    }
    WaitOutcome::Signaled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    #[test]
    fn wait_times_out_when_nobody_notifies() {
        let hub = WaitHub::new();
        let w = register(&hub, b"70/list");
        assert_eq!(wait(&w, Duration::from_millis(20)), WaitOutcome::Timeout);
        unregister(&hub, b"70/list", &w);
        // queue empty again: a second unregister is a harmless no-op
        unregister(&hub, b"70/list", &w);
    }

    #[test]
    fn notify_before_wait_returns_immediately() {
        let hub = WaitHub::new();
        let w = register(&hub, b"70/list");
        signal(&w);
        assert_eq!(wait(&w, Duration::from_millis(0)), WaitOutcome::Signaled);
    }

    #[test]
    fn two_threads_one_waits_other_notifies() {
        let hub = Arc::new(WaitHub::new());
        let key = b"70/blpop-root".to_vec();
        let waiter = Arc::new(register(&hub, &key));
        let parked = Arc::new(AtomicBool::new(false));
        let w2 = Arc::clone(&waiter);

        let notifier_key = key.clone();
        let notifier_parked = Arc::clone(&parked);
        let notifier = {
            let hub = Arc::clone(&hub);
            thread::spawn(move || {
                while !notifier_parked.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
                thread::sleep(Duration::from_millis(20));
                assert!(notify(&hub, &notifier_key));
            })
        };
        let waiter_thread = thread::spawn(move || {
            parked.store(true, Ordering::SeqCst);
            wait(&w2, Duration::from_secs(5))
        });
        assert_eq!(waiter_thread.join().unwrap(), WaitOutcome::Signaled);
        notifier.join().unwrap();
    }

    #[test]
    fn notify_is_fifo_and_ignores_missing_keys() {
        let hub = WaitHub::new();
        let key = b"70/q".to_vec();
        let first = register(&hub, &key);
        let second = register(&hub, &key);
        assert!(notify(&hub, &key));
        assert_eq!(
            wait(&first, Duration::from_millis(0)),
            WaitOutcome::Signaled
        );
        assert_eq!(
            wait(&second, Duration::from_millis(0)),
            WaitOutcome::Timeout
        );
        assert!(notify(&hub, &key));
        assert_eq!(
            wait(&second, Duration::from_millis(0)),
            WaitOutcome::Signaled
        );
        assert!(!notify(&hub, &key), "queue drained");
        assert!(!notify(&hub, b"99/nope"), "unknown key");
    }
}
