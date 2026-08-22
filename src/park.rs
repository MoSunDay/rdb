//! Dedicated park pool: Condvar parking OFF tokio's shared blocking pool.
//!
//! WHY: BLPOP/BZPOPMIN/XREAD BLOCK park on a sync Condvar (`ds::wait`)
//! for up to the caller's FULL timeout. `tokio::task::spawn_blocking`
//! draws from one bounded pool (default 512 threads) shared with every
//! blocking job in the process -- including the RocksDB fsync writes.
//! 512 forever-parked `BLPOP k 0` calls would occupy the entire pool,
//! starve the fsync tasks, and stall ALL writes (P1). This module gives
//! parks their own fixed pool of cheap small-stack threads so they can
//! never crowd out storage I/O. Free functions only; workers are plain
//! closures looping on one shared queue.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use tokio::sync::{mpsc, oneshot};

/// A unit of parked work: an owned closure run on a pool thread.
type ParkJob = Box<dyn FnOnce() + Send + 'static>;

/// Fixed worker count matching the prior shared-pool capacity (tokio's
/// default blocking pool is also 512), so park throughput is unchanged.
const WORKERS: usize = 512;

/// Bounded queue: a saturated pool delays submitters via the async send
/// instead of blocking a tokio worker thread on submission.
const QUEUE_DEPTH: usize = 1024;

/// The pool handle, lazily started on the first park.
fn pool() -> &'static mpsc::Sender<ParkJob> {
    static POOL: OnceLock<mpsc::Sender<ParkJob>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<ParkJob>(QUEUE_DEPTH);
        // All workers share one receiver behind a std Mutex: each holds
        // the guard only while WAITING for its next job, never while
        // running one, so up to WORKERS jobs execute concurrently.
        let rx = Arc::new(Mutex::new(rx));
        for n in 0..WORKERS {
            let rx = Arc::clone(&rx);
            // Small stacks: parks just Condvar-wait, they never recurse.
            // A failed spawn (out of resources) only shrinks the pool.
            let _ = thread::Builder::new()
                .name(format!("rdb-park-{n}"))
                .stack_size(256 * 1024)
                .spawn(move || loop {
                    let job = rx
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .blocking_recv();
                    match job {
                        // Channel closed at shutdown: the worker exits.
                        None => return,
                        // catch_unwind so a panicking job never kills its
                        // worker; the panic payload is deliberately
                        // swallowed here (the submitter learns via None).
                        Some(job) => {
                            let _ = catch_unwind(AssertUnwindSafe(job));
                        }
                    }
                });
        }
        tx
    })
}

/// Run `f` on the dedicated park pool and await its result.
///
/// Returns `None` when the job panicked (its oneshot sender is dropped
/// by the unwind, so the channel closes) or the pool shut down; callers
/// map that to the same fallback a `spawn_blocking` JoinError produced.
pub async fn park<F, T>(f: F) -> Option<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (done_tx, done_rx) = oneshot::channel();
    // Async send: a full queue parks THIS task, not a tokio worker.
    pool()
        .send(Box::new(move || {
            let _ = done_tx.send(f());
        }))
        .await
        .ok()?;
    done_rx.await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn park_returns_the_closure_result() {
        let got = park(|| 7 + 35).await;
        assert_eq!(got, Some(42));
    }

    #[tokio::test]
    async fn panicking_job_yields_none_and_pool_survives() {
        assert_eq!(
            park::<fn() -> (), _>(|| panic!("boom")).await,
            None,
            "unwind must surface as None"
        );
        // The worker lived through the panic and still serves jobs.
        assert_eq!(park(|| "ok").await, Some("ok"));
    }

    #[tokio::test]
    async fn parks_run_concurrently_not_serially() {
        // 64 simultaneous 150ms-equivalent barrier parks must overlap,
        // not run one-at-a-time: proof that idle waiting does not
        // serialize the workers. (Spawn: park futures are lazy.)
        let barrier = Arc::new(std::sync::Barrier::new(64));
        let t0 = std::time::Instant::now();
        let mut jobs = Vec::new();
        for _ in 0..64 {
            let b = Arc::clone(&barrier);
            jobs.push(tokio::spawn(async move {
                park(move || {
                    b.wait();
                })
                .await
            }));
        }
        for j in jobs {
            assert_eq!(j.await.expect("park task"), Some(()));
        }
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "parks overlapped: {:?}",
            t0.elapsed()
        );
    }
}
