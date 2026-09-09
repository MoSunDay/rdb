//! Queue-exclusive ownership for ordered consumer groups (Kafka
//! partition-ordering calibrated): one stream ("queue") is owned by at
//! most ONE consumer of a group at any time; ordering lives inside the
//! queue and different queues never wait on each other (same-key ->
//! same-queue comes from `XPICK <parent> hash <key>` / `pick_hash`).
//!
//! No coordinator, no rebalance protocol: ownership EMERGES from the
//! read path. A `>` read takes a free queue, keeps it while its lease
//! is fresh (every delivery refreshes it), and an idle queue migrates
//! to whichever consumer asks next. A stalled-but-leased queue changes
//! hands only through XCLAIM/XAUTOCLAIM of the PEL HEAD (queue-granularity
//! takeover, see `claim`/`autoclaim`) -- there is no stealing half a
//! batch from under a live owner.
//!
//! Fencing is a Kafka-generation analogue: every ownership change bumps
//! an epoch; PEL rows delivered under an epoch are stamped with it, and
//! a deposed owner's later `>` reads are fenced out (they deliver
//! nothing), so two consumers can never process NEW entries of one
//! queue concurrently. Acked ids of a deposed owner remain idempotent
//! row deletions -- at-least-once tolerates the duplicate.
//!
//! Pure functions over a shared map; state lives in `Runtime::owners`.

use std::collections::HashMap;

/// (stream, group) -- raw bytes, same keying discipline as the offset
/// cache (group names are not charset-validated).
pub type OwnerKey = (Vec<u8>, Vec<u8>);

/// Shared ownership table type hung off `Runtime`.
pub type OwnerMap = HashMap<OwnerKey, Owner>;

/// Ownership record of one (stream, group) queue.
#[derive(Clone, Debug, PartialEq)]
pub struct Owner {
    pub consumer: Vec<u8>,
    /// Monotonic fencing token; bumps on every ownership change.
    pub epoch: u64,
    /// Last ownership-refresh instant (`expire::now_ms` clock).
    pub active_ms: u64,
}

/// Outcome of an ownership attempt on the `>` delivery path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The consumer may deliver: it kept its lease, took a free queue,
    /// or took over an expired one (`took_over` set on a change; the
    /// caller wakes parked contenders so they re-check).
    Own { epoch: u64, took_over: bool },
    /// Another consumer holds a fresh lease: fenced out (deliver
    /// nothing; blocking readers re-park and retry on the next wake).
    Busy { epoch: u64 },
}

/// Try to (re)acquire the queue for `consumer`. Free queues and expired
/// leases are taken immediately (idle migration); a fresh foreign lease
/// is respected (`Busy`).
pub fn acquire(
    owners: &std::sync::Mutex<HashMap<OwnerKey, Owner>>,
    stream: &[u8],
    group: &[u8],
    consumer: &[u8],
    now_ms: u64,
    lease_ms: u64,
) -> Access {
    let mut map = owners
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key: OwnerKey = (stream.to_vec(), group.to_vec());
    match map.get(&key) {
        None => {
            map.insert(
                key,
                Owner {
                    consumer: consumer.to_vec(),
                    epoch: 1,
                    active_ms: now_ms,
                },
            );
            Access::Own {
                epoch: 1,
                took_over: true,
            }
        }
        Some(o) if o.consumer == consumer => {
            let epoch = o.epoch;
            map.get_mut(&key).unwrap().active_ms = now_ms;
            Access::Own {
                epoch,
                took_over: false,
            }
        }
        Some(o) if now_ms.saturating_sub(o.active_ms) >= lease_ms => {
            let epoch = o.epoch + 1;
            map.insert(
                key,
                Owner {
                    consumer: consumer.to_vec(),
                    epoch,
                    active_ms: now_ms,
                },
            );
            Access::Own {
                epoch,
                took_over: true,
            }
        }
        Some(o) => Access::Busy { epoch: o.epoch },
    }
}

/// Unconditional takeover for the claim path (XCLAIM/XAUTOCLAIM already
/// gate on `min-idle-time` themselves): flips ownership to `consumer`,
/// bumping the epoch on every real change of hands. Returns the live
/// epoch for stamping claimed PEL rows.
pub fn force_takeover(
    owners: &std::sync::Mutex<HashMap<OwnerKey, Owner>>,
    stream: &[u8],
    group: &[u8],
    consumer: &[u8],
    now_ms: u64,
) -> u64 {
    let mut map = owners
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key: OwnerKey = (stream.to_vec(), group.to_vec());
    match map.get(&key) {
        Some(o) if o.consumer == consumer => {
            let epoch = o.epoch;
            map.get_mut(&key).unwrap().active_ms = now_ms;
            epoch
        }
        Some(o) => {
            let epoch = o.epoch + 1;
            map.insert(
                key,
                Owner {
                    consumer: consumer.to_vec(),
                    epoch,
                    active_ms: now_ms,
                },
            );
            epoch
        }
        None => {
            map.insert(
                key,
                Owner {
                    consumer: consumer.to_vec(),
                    epoch: 1,
                    active_ms: now_ms,
                },
            );
            1
        }
    }
}

/// XGROUP DELCONSUMER: release the queue if the departing consumer held
/// it, so the next reader takes over without waiting out the lease.
pub fn release_consumer(
    owners: &std::sync::Mutex<HashMap<OwnerKey, Owner>>,
    stream: &[u8],
    group: &[u8],
    consumer: &[u8],
) {
    let mut map = owners
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key: OwnerKey = (stream.to_vec(), group.to_vec());
    if map.get(&key).is_some_and(|o| o.consumer == consumer) {
        map.remove(&key);
    }
}

/// XGROUP DESTROY: the group (and its ownership) is gone.
pub fn drop_group(
    owners: &std::sync::Mutex<HashMap<OwnerKey, Owner>>,
    stream: &[u8],
    group: &[u8],
) {
    owners
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(stream.to_vec(), group.to_vec()));
}

/// Stream family reaped / FLUSHDB-adjacent paths: drop every group of
/// the stream.
pub fn drop_stream(owners: &std::sync::Mutex<HashMap<OwnerKey, Owner>>, stream: &[u8]) {
    owners
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|(s, _), _| s != stream);
}

/// FLUSHDB: drop everything.
pub fn clear(owners: &std::sync::Mutex<HashMap<OwnerKey, Owner>>) {
    owners
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Current ownership for introspection (XINFO GROUPS owner/epoch).
pub fn peek(
    owners: &std::sync::Mutex<HashMap<OwnerKey, Owner>>,
    stream: &[u8],
    group: &[u8],
) -> Option<Owner> {
    owners
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(stream.to_vec(), group.to_vec()))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners() -> std::sync::Mutex<HashMap<OwnerKey, Owner>> {
        std::sync::Mutex::new(HashMap::new())
    }

    #[test]
    fn fresh_queue_is_taken_then_refreshed() {
        let m = owners();
        assert_eq!(
            acquire(&m, b"t/q0", b"g", b"c1", 100, 30_000),
            Access::Own {
                epoch: 1,
                took_over: true
            }
        );
        assert_eq!(
            acquire(&m, b"t/q0", b"g", b"c1", 200, 30_000),
            Access::Own {
                epoch: 1,
                took_over: false
            }
        );
        assert_eq!(peek(&m, b"t/q0", b"g").unwrap().active_ms, 200);
    }

    #[test]
    fn foreign_fresh_lease_is_busy() {
        let m = owners();
        acquire(&m, b"t/q0", b"g", b"c1", 100, 30_000);
        assert_eq!(
            acquire(&m, b"t/q0", b"g", b"c2", 200, 30_000),
            Access::Busy { epoch: 1 }
        );
    }

    #[test]
    fn expired_lease_migrates_with_epoch_bump() {
        let m = owners();
        acquire(&m, b"t/q0", b"g", b"c1", 100, 30_000);
        assert_eq!(
            acquire(&m, b"t/q0", b"g", b"c2", 100 + 30_000, 30_000),
            Access::Own {
                epoch: 2,
                took_over: true
            }
        );
        // The deposed owner is fenced out even though it asks first.
        assert_eq!(
            acquire(&m, b"t/q0", b"g", b"c1", 100 + 30_001, 30_000),
            Access::Busy { epoch: 2 }
        );
    }

    #[test]
    fn force_takeover_bumps_only_real_changes() {
        let m = owners();
        acquire(&m, b"t/q0", b"g", b"c1", 100, 30_000);
        assert_eq!(force_takeover(&m, b"t/q0", b"g", b"c1", 150), 1);
        assert_eq!(force_takeover(&m, b"t/q0", b"g", b"c2", 200), 2);
        assert_eq!(peek(&m, b"t/q0", b"g").unwrap().consumer, b"c2".to_vec());
    }

    #[test]
    fn release_drop_and_clear_paths() {
        let m = owners();
        acquire(&m, b"t/q0", b"g1", b"c1", 100, 30_000);
        acquire(&m, b"t/q1", b"g1", b"c1", 100, 30_000);
        acquire(&m, b"t/q1", b"g2", b"c2", 100, 30_000);
        // A departing NON-owner must not release the queue.
        release_consumer(&m, b"t/q0", b"g1", b"c2");
        assert!(peek(&m, b"t/q0", b"g1").is_some());
        release_consumer(&m, b"t/q0", b"g1", b"c1");
        assert!(peek(&m, b"t/q0", b"g1").is_none());
        drop_group(&m, b"t/q1", b"g1");
        assert!(peek(&m, b"t/q1", b"g1").is_none());
        assert!(peek(&m, b"t/q1", b"g2").is_some());
        drop_stream(&m, b"t/q1");
        assert!(peek(&m, b"t/q1", b"g2").is_none());
        acquire(&m, b"t/q2", b"g", b"c9", 100, 30_000);
        clear(&m);
        assert!(peek(&m, b"t/q2", b"g").is_none());
    }
}
