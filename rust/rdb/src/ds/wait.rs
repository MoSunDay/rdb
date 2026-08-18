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

/// Register an EXISTING waiter under `key` — lets one blocking command
/// park on several keys at once (BLPOP key1..keyN). Same FIFO semantics
/// as [`register`], which creates and registers in one step.
pub fn register_shared(hub: &WaitHub, key: &[u8], waiter: &Arc<Waiter>) {
    let mut map = hub
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.entry(key.to_vec())
        .or_default()
        .push_back(Arc::clone(waiter));
}

/// Signal ONE LIVE (not yet signaled) waiter for `key`, popping it — and
/// any stale waiters ahead of it — from the queue. A multi-key blocking
/// command parks the SAME waiter under several roots; once it was woken
/// via one key, its remaining queue entries are stale: skipping them (and
/// discarding them) keeps FIFO order for the real waiters behind, which a
/// plain pop-and-signal would silently swallow. A drained queue with no
/// live waiter removes the entry. Returns whether anyone was woken.
pub fn notify(hub: &WaitHub, key: &[u8]) -> bool {
    let mut map = hub
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut woke = false;
    if let Some(queue) = map.get_mut(key) {
        while let Some(waiter) = queue.pop_front() {
            if signal_if_armed(&waiter) {
                woke = true;
                break;
            }
        }
    }
    if map.get(key).is_some_and(|q| q.is_empty()) {
        map.remove(key);
    }
    woke
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

/// [`signal`] guarded against double-waking: flips `signaled` and wakes
/// only when it was still false; returns whether this call did the wake.
fn signal_if_armed(waiter: &Waiter) -> bool {
    let mut signaled = waiter
        .signaled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *signaled {
        return false;
    }
    *signaled = true;
    drop(signaled);
    waiter.cv.notify_all();
    true
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
    fn shared_waiter_wakes_from_either_registered_key() {
        let hub = WaitHub::new();
        let waiter = Arc::new(new_waiter());
        register_shared(&hub, b"70/k1", &waiter);
        register_shared(&hub, b"70/k2", &waiter);
        // notify on EITHER key pops and signals the same waiter
        assert!(notify(&hub, b"70/k2"));
        assert_eq!(
            wait(&waiter, Duration::from_millis(0)),
            WaitOutcome::Signaled
        );
        // the other key now holds only a STALE entry: popping it wakes
        // nobody (the command already finished) and the queue is dropped
        assert!(!notify(&hub, b"70/k1"));
        // both queues are now drained
        assert!(!notify(&hub, b"70/k1"));
        assert!(!notify(&hub, b"70/k2"));
    }

    #[test]
    fn notify_skips_stale_waiter_and_signals_next_live_one() {
        let hub = WaitHub::new();
        // A multi-key BLPOP parks one waiter under "a" AND "b"; it was
        // woken via "a", so "b"'s queue starts with that stale entry.
        let stale = register(&hub, b"70/a");
        register_shared(&hub, b"70/b", &stale);
        signal(&stale);
        let live = register(&hub, b"70/b");
        // notify(b) must skip the stale front and wake the live waiter.
        assert!(notify(&hub, b"70/b"));
        assert_eq!(
            wait(&live, Duration::from_millis(0)),
            WaitOutcome::Signaled
        );
        assert_eq!(
            wait(&stale, Duration::from_millis(0)),
            WaitOutcome::Signaled,
            "stale waiter keeps its original wake"
        );
        // queue drained behind them
        assert!(!notify(&hub, b"70/b"));
    }

    #[test]
    fn notify_on_all_stale_queue_returns_false_and_removes_entry() {
        let hub = WaitHub::new();
        let stale = register(&hub, b"70/a");
        register_shared(&hub, b"70/b", &stale);
        signal(&stale);
        assert!(!notify(&hub, b"70/b"));
        // the emptied entry is cleaned up, not left behind
        let gone = b"70/b".to_vec();
        assert!(!hub
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&gone));
        assert!(!notify(&hub, b"70/b"), "entry gone: no wake, no panic");
    }

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
