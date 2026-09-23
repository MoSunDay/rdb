//! Membership session bookkeeping: wall clock, the per-group
//! `tokio::sync::Notify` registry (the join-barrier / sync handoff
//! wakeup -- the condvar pattern from `ds::wait`, adapted to async),
//! and the ~500ms background sweep that applies `state::expire` to
//! every group and wakes the groups whose state changed.
//!
//! Why Notify and not a plain poll: JoinGroup/SyncGroup long-park per
//! CONNECTION TASK (one task per conn in `conn::handle_conn`), so an
//! awaiting member never blocks another connection; the sweep pokes
//! exactly the groups whose deadlines lapsed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use std::sync::RwLockWriteGuard;

use tokio::sync::Notify;

use super::state::{self, Event, GroupState};
use super::CoordRuntime;

/// Sweep cadence: session timeouts in the seconds range make 500ms a
/// fine expiry granularity (tests use >= 1.5s sessions).
pub const SWEEP_INTERVAL_MS: u64 = 500;

/// Wall clock in ms since the epoch (the deadline unit of `state.rs`).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The group's Notify handle (created on first use, never removed:
/// entries are one per group name, so the map stays bounded by the
/// group count; removal could strand a waiter holding a stale Arc).
pub fn notify_of(rt: &CoordRuntime, group: &str) -> Arc<Notify> {
    let mut map = rt
        .notifies
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.entry(group.to_string())
        .or_insert_with(|| Arc::new(Notify::new()))
        .clone()
}

/// Wake every waiter parked on this group (waiters re-check state and
/// re-park on spurious wakes; over-notifying is harmless).
pub fn notify_group(rt: &CoordRuntime, group: &str) {
    notify_of(rt, group).notify_waiters();
}

/// Wake the group when a transition produced any event.
pub fn notify_events(rt: &CoordRuntime, group: &str, events: &[Event]) {
    if !events.is_empty() {
        notify_group(rt, group);
    }
}

/// Take the group's state out of the map under the write lock, run one
/// pure transition, store the result and wake the group if it emitted
/// events. `f` returns the transition's non-state output.
pub fn apply<T>(
    rt: &CoordRuntime,
    group: &str,
    f: impl FnOnce(GroupState) -> (GroupState, Vec<Event>, T),
) -> Option<T> {
    let mut groups: RwLockWriteGuard<'_, HashMap<String, GroupState>> = rt
        .groups
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let st = groups.get(group).cloned()?;
    let (st2, events, out) = f(st);
    groups.insert(group.to_string(), st2);
    drop(groups);
    notify_events(rt, group, &events);
    Some(out)
}

/// One expiry pass over every group; returns the groups that changed
/// (already notified). Empty/unchanged groups cost one clone check.
pub fn sweep_once(rt: &CoordRuntime) -> Vec<String> {
    let now = now_ms();
    let mut changed = Vec::new();
    let mut groups = rt
        .groups
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let names: Vec<String> = groups.keys().cloned().collect();
    for name in names {
        let Some(st) = groups.get(&name).cloned() else {
            continue;
        };
        let (st2, events) = state::expire(st, now);
        if events.is_empty() {
            continue;
        }
        groups.insert(name.clone(), st2);
        changed.push(name);
    }
    drop(groups);
    for name in &changed {
        notify_group(rt, name);
    }
    changed
}

/// The background sweep task (spawned once per process by
/// `kafka::serve`).
pub async fn run_sweep(rt: Arc<CoordRuntime>) {
    let mut tick = tokio::time::interval(Duration::from_millis(SWEEP_INTERVAL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        sweep_once(&rt);
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::{self, GroupStage};
    use super::*;

    fn rt() -> Arc<CoordRuntime> {
        Arc::new(CoordRuntime::new())
    }

    #[test]
    fn notify_entries_are_stable_handles() {
        let rt = rt();
        let a = notify_of(&rt, "g");
        let b = notify_of(&rt, "g");
        assert!(Arc::ptr_eq(&a, &b), "one Notify per group");
        notify_group(&rt, "g");
    }

    #[test]
    fn sweep_expires_stale_sessions() {
        let rt = rt();
        let mut st = state::new_group("consumer");
        let member = state::JoinArgs {
            member_id: "m1",
            instance_id: None,
            client_id: "c",
            client_host: "h",
            session_timeout_ms: 1,
            rebalance_timeout_ms: 4,
            protocol_type: "consumer",
            protocol_name: "range",
            metadata: b"",
            now_ms: 0,
        };
        let (st2, _, _) = state::join(st.clone(), &member);
        st = st2;
        rt.groups.write().unwrap().insert("g".into(), st);
        // Pin every deadline far out: the sweep finds nothing to do
        // regardless of wall-clock drift (sessions here are 1ms).
        {
            let mut groups = rt.groups.write().unwrap();
            let st = groups.get_mut("g").unwrap();
            for m in st.members.values_mut() {
                m.session_deadline_ms = i64::MAX / 4;
                m.rebalance_deadline_ms = i64::MAX / 4;
            }
        }
        assert!(sweep_once(&rt).is_empty());
        // Push the session deadlines into the past: the next pass
        // empties the group and reports it as changed.
        {
            let mut groups = rt.groups.write().unwrap();
            let st = groups.get_mut("g").unwrap();
            for m in st.members.values_mut() {
                m.session_deadline_ms = 0;
            }
        }
        let changed = sweep_once(&rt);
        assert_eq!(changed, vec!["g".to_string()]);
        assert_eq!(rt.groups.read().unwrap()["g"].stage, GroupStage::Empty);
    }

    #[tokio::test]
    async fn sweep_task_is_cancellable() {
        // The task holds no resources beyond the runtime Arc; dropping
        // the handle (or process exit) ends it. Just prove one tick.
        let rt = rt();
        let task = tokio::spawn(run_sweep(Arc::clone(&rt)));
        tokio::time::sleep(Duration::from_millis(SWEEP_INTERVAL_MS + 50)).await;
        task.abort();
    }
}
