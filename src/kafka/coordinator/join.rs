//! The async half of the coordination APIs: JoinGroup parks until the
//! join barrier completes (all members (re)joined, or the rebalance
//! deadline force-finishes via the sweep), SyncGroup followers park
//! until the leader's assignments land. Each call runs on ITS OWN
//! connection task (`conn::handle_conn` spawns one per connection), so
//! a parked member never blocks another connection.
//!
//! Wakeup pattern (tokio Notify, the async mold of `ds::wait`):
//! register interest with `Notified::enable()` BEFORE reading the
//! state, so a notify fired between the read and the await can never
//! be missed; waiters re-check state on every wake (spurious wakes are
//! harmless) and give up at the member's rebalance deadline + slack.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::session;
use super::state::{self, GroupStage, GroupState};
use super::CoordRuntime;
use crate::kafka::errors;
use crate::kafka::ledger;

/// Grace beyond the rebalance deadline: the sweep force-completes at
/// the deadline on a 500ms cadence, so waiters outlive it a little
/// before answering REBALANCE_IN_PROGRESS (clients retry the join).
const DEADLINE_SLACK_MS: i64 = 2000;

/// One JoinGroup request (member ids resolved by the caller; the
/// candidate for an empty request id comes from `next_member_id`).
pub struct JoinReq {
    pub group: String,
    pub member_id: String,
    pub candidate_member_id: String,
    pub instance_id: Option<String>,
    pub client_id: String,
    pub client_host: String,
    pub session_timeout_ms: i64,
    pub rebalance_timeout_ms: i64,
    pub protocol_type: String,
    pub protocol_name: String,
    pub metadata: Vec<u8>,
}

/// A leader-only member row (follower replies carry an empty list).
pub struct LeaderMember {
    pub member_id: String,
    pub instance_id: Option<String>,
    pub metadata: Vec<u8>,
}

/// Everything the JoinGroup wire encoder needs.
pub struct JoinReply {
    pub error: i16,
    pub member_id: String,
    pub generation: i32,
    pub protocol_type: Option<String>,
    pub protocol_name: Option<String>,
    pub leader: String,
    pub members: Vec<LeaderMember>,
}

/// Everything the SyncGroup wire encoder needs.
pub struct SyncReply {
    pub error: i16,
    pub assignment: Vec<u8>,
}

fn err_reply(error: i16, member_id: &str) -> JoinReply {
    JoinReply {
        error,
        member_id: member_id.to_string(),
        generation: 0,
        protocol_type: None,
        protocol_name: None,
        leader: String::new(),
        members: Vec::new(),
    }
}

/// The successful barrier reply: the shared generation/leader, plus
/// the full member list ONLY for the leader (followers get `[]`).
fn barrier_reply(st: &GroupState, member_id: &str) -> JoinReply {
    let is_leader = st.leader == member_id;
    JoinReply {
        error: errors::NONE,
        member_id: member_id.to_string(),
        generation: st.generation,
        protocol_type: Some(st.protocol_type.clone()),
        protocol_name: st.protocol_name.clone(),
        leader: st.leader.clone(),
        members: if is_leader {
            st.order
                .iter()
                .map(|id| {
                    let m = &st.members[id];
                    LeaderMember {
                        member_id: id.clone(),
                        instance_id: m.instance_id.clone(),
                        metadata: m.metadata.clone(),
                    }
                })
                .collect()
        } else {
            Vec::new()
        },
    }
}

/// JoinGroup: apply the transition, then park until this member's
/// barrier reply exists (or it was dropped: expiry/leave/fence).
///
/// `store` seeds the generation on a group's FIRST join of this
/// process: the runtime counter resumes above the durable ledger's
/// high-water generation, so post-restart groups are never fenced by
/// pre-restart ledger rows (the coordinator-local counter itself does
/// not survive restarts).
pub async fn join_group(
    rt: &Arc<CoordRuntime>,
    req: JoinReq,
    store: &crate::store::Store,
) -> JoinReply {
    if req.member_id.is_empty() {
        // Two-phase handshake (KIP-394): MEMBER_ID_REQUIRED carries the
        // generated id; the client re-joins with it.
        let gen = rt
            .groups
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&req.group)
            .map(|st| st.generation)
            .unwrap_or(0);
        let mut r = err_reply(errors::MEMBER_ID_REQUIRED, &req.candidate_member_id);
        r.generation = gen;
        return r;
    }
    if req.session_timeout_ms <= 0 || req.rebalance_timeout_ms <= 0 {
        return err_reply(errors::INVALID_SESSION_TIMEOUT, &req.member_id);
    }
    if req.group.is_empty() {
        return err_reply(errors::INVALID_GROUP_ID, &req.member_id);
    }
    // Create-on-first-touch, then run the pure transition under the
    // write lock and wake the group when events came out.
    let outcome = {
        let mut groups = rt
            .groups
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let st = match groups.get_mut(&req.group) {
            Some(st) => st,
            None => {
                // First join of this process: seed the generation from
                // the ledger (max(0, high-water row gen)). One bounded
                // scan per group per process -- see ledger::scan_group.
                let seed = ledger::scan_group(store, req.group.as_bytes())
                    .ok()
                    .and_then(|rows| rows.iter().map(|r| r.generation).max())
                    .unwrap_or(0)
                    .max(0);
                groups
                    .entry(req.group.clone())
                    .or_insert_with(|| state::new_group_seeded(&req.protocol_type, seed))
            }
        };
        let args = state::JoinArgs {
            member_id: &req.member_id,
            instance_id: req.instance_id.as_deref(),
            client_id: &req.client_id,
            client_host: &req.client_host,
            session_timeout_ms: req.session_timeout_ms,
            rebalance_timeout_ms: req.rebalance_timeout_ms,
            protocol_type: &req.protocol_type,
            protocol_name: &req.protocol_name,
            metadata: &req.metadata,
            now_ms: session::now_ms(),
        };
        let (new, events, out) = state::join(std::mem::replace(st, state::new_group("")), &args);
        *st = new;
        drop(groups);
        session::notify_events(rt, &req.group, &events);
        out
    };
    match outcome {
        state::JoinOutcome::NeedMemberId => {
            err_reply(errors::UNKNOWN_MEMBER_ID, &req.candidate_member_id)
        }
        state::JoinOutcome::FencedInstanceId => {
            err_reply(errors::FENCED_INSTANCE_ID, &req.member_id)
        }
        state::JoinOutcome::InconsistentProtocol => {
            err_reply(errors::INCONSISTENT_GROUP_PROTOCOL, &req.member_id)
        }
        state::JoinOutcome::Joined { .. } | state::JoinOutcome::Waiting => {
            // Barrier already carried this member (or completed in
            // somebody else's call): the resolver returns immediately.
            let deadline = session::now_ms() + req.rebalance_timeout_ms + DEADLINE_SLACK_MS;
            wait_state(rt, &req.group, deadline, |st| {
                let Some(st) = st else {
                    // Group entry gone under the parked waiter
                    // (DeleteGroups / FLUSHDB runtime eviction): the
                    // member is unknown in a group that no longer
                    // exists; the client re-joins natively.
                    return Some(err_reply(errors::UNKNOWN_MEMBER_ID, &req.member_id));
                };
                let Some(m) = st.members.get(&req.member_id) else {
                    return Some(err_reply(errors::UNKNOWN_MEMBER_ID, &req.member_id));
                };
                match st.stage {
                    GroupStage::CompletingSync | GroupStage::Stable
                        if m.join_gen == st.generation =>
                    {
                        Some(barrier_reply(st, &req.member_id))
                    }
                    _ => None,
                }
            })
            .await
            .unwrap_or_else(|| err_reply(errors::REBALANCE_IN_PROGRESS, &req.member_id))
        }
    }
}

/// SyncGroup: the LEADER's call (decided server-side by role, not by
/// the wire shape -- followers also send an empty assignments array)
/// distributes and settles the group; a follower parks until the
/// assignments land (or the group rebalances again: 27 sends it back
/// to JoinGroup).
pub async fn sync_group(
    rt: &Arc<CoordRuntime>,
    group: &str,
    member_id: &str,
    generation: i32,
    assignments: Vec<(String, Vec<u8>)>,
) -> SyncReply {
    let fenced = |code: i16| SyncReply {
        error: code,
        assignment: Vec::new(),
    };
    if group.is_empty() {
        return fenced(errors::INVALID_GROUP_ID);
    }
    let now = session::now_ms();
    let out = session::apply(rt, group, |st| {
        state::sync(st, member_id, generation, &assignments, now)
    });
    match out {
        // The leader (or an idempotent re-sync while Stable): answer
        // with the member's own assignment.
        Some(state::SyncOutcome::Ok) => {
            let groups = rt
                .groups
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match groups.get(group).and_then(|st| st.members.get(member_id)) {
                Some(m) => SyncReply {
                    error: errors::NONE,
                    assignment: m.assignment.clone(),
                },
                None => fenced(errors::UNKNOWN_MEMBER_ID),
            }
        }
        // A follower awaiting the leader: park on the SyncReady event.
        Some(state::SyncOutcome::Waiting) => {
            let deadline = {
                let groups = rt
                    .groups
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match groups.get(group).and_then(|st| st.members.get(member_id)) {
                    None => return fenced(errors::UNKNOWN_MEMBER_ID),
                    Some(m) => m.rebalance_deadline_ms + DEADLINE_SLACK_MS,
                }
            };
            wait_state(rt, group, deadline, |st| {
                let Some(st) = st else {
                    // Same as above: the whole entry was evicted.
                    return Some(fenced(errors::UNKNOWN_MEMBER_ID));
                };
                let Some(m) = st.members.get(member_id) else {
                    return Some(fenced(errors::UNKNOWN_MEMBER_ID));
                };
                if generation != st.generation || m.join_gen != st.generation {
                    return Some(fenced(errors::ILLEGAL_GENERATION));
                }
                match st.stage {
                    GroupStage::Stable => Some(SyncReply {
                        error: errors::NONE,
                        assignment: m.assignment.clone(),
                    }),
                    GroupStage::PreparingRebalance => Some(fenced(errors::REBALANCE_IN_PROGRESS)),
                    _ => None, // CompletingSync: still waiting on the leader
                }
            })
            .await
            .unwrap_or_else(|| fenced(errors::REBALANCE_IN_PROGRESS))
        }
        None => fenced(errors::UNKNOWN_MEMBER_ID), // no such group
        Some(state::SyncOutcome::UnknownMember) => fenced(errors::UNKNOWN_MEMBER_ID),
        Some(state::SyncOutcome::IllegalGeneration) => fenced(errors::ILLEGAL_GENERATION),
        Some(state::SyncOutcome::Rebalance) => fenced(errors::REBALANCE_IN_PROGRESS),
    }
}

/// Park until `resolve` sees a settled answer in the group state; the
/// closure receives `None` when the group ENTRY itself is gone
/// (DeleteGroups / FLUSHDB eviction) so it can fence the waiter
/// instead of parking out its deadline. Registration happens
/// (`enable()`) before the state read, so a concurrent notify cannot
/// slip between read and await; a permit that was already stored is
/// consumed by `enable()` and handled by re-reading the state (never
/// awaited, it would sleep forever).
async fn wait_state<T>(
    rt: &Arc<CoordRuntime>,
    group: &str,
    deadline_ms: i64,
    resolve: impl Fn(Option<&GroupState>) -> Option<T>,
) -> Option<T> {
    let remaining = (deadline_ms - session::now_ms()).max(1) as u64;
    let deadline = Instant::now() + Duration::from_millis(remaining);
    loop {
        let notify = session::notify_of(rt, group);
        let notified = notify.notified();
        tokio::pin!(notified);
        let permitted = notified.as_mut().enable();
        let got = {
            let groups = rt
                .groups
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            resolve(groups.get(group))
        };
        if got.is_some() {
            return got;
        }
        if permitted {
            continue; // stored permit consumed: re-read before parking
        }
        if tokio::time::timeout_at(deadline, notified.as_mut())
            .await
            .is_err()
        {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JoinGroup waiter parked in PreparingRebalance must not outlive
    /// a runtime wipe (FLUSHDB / DeleteGroups): `clear_groups` notifies
    /// the group, the waiter re-reads an ABSENT entry and answers
    /// UNKNOWN_MEMBER_ID promptly -- the 5s cap here is far below the
    /// member's 62s deadline parking, so the pass proves the wakeup
    /// path (the deadline stays as the no-notify safety net).
    #[tokio::test]
    async fn parked_waiter_is_fenced_promptly_by_clear_groups() {
        let shared = crate::state::testutil::shared_with(crate::state::testutil::test_config());
        let rt = Arc::new(CoordRuntime::new());
        let req = |member: &str| JoinReq {
            group: "g1".into(),
            member_id: member.into(),
            candidate_member_id: member.into(),
            instance_id: None,
            client_id: "c".into(),
            client_host: "h".into(),
            session_timeout_ms: 30_000,
            rebalance_timeout_ms: 60_000,
            protocol_type: "consumer".into(),
            protocol_name: "range".into(),
            metadata: b"sub".to_vec(),
        };
        // m1 completes the first barrier alone (gen 1, Stable); m2's
        // join kicks PreparingRebalance and parks as Waiting.
        let r = join_group(&rt, req("m1"), &shared.store).await;
        assert_eq!(r.error, errors::NONE);
        let rt2 = Arc::clone(&rt);
        let store = std::sync::Arc::clone(&shared.store);
        let waiter = tokio::spawn(async move { join_group(&rt2, req("m2"), &store).await });
        for _ in 0..500 {
            if rt
                .groups
                .read()
                .unwrap()
                .get("g1")
                .is_some_and(|st| st.members.contains_key("m2"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let groups = rt.groups.read().unwrap();
        let st = groups.get("g1").expect("m2 joined the table");
        assert_eq!(
            st.stage,
            GroupStage::PreparingRebalance,
            "m2's join kicked the rebalance"
        );
        drop(groups);

        let evicted = crate::kafka::coordinator::clear_groups(&rt);
        assert_eq!(evicted, vec!["g1".to_string()]);

        let r = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter must not park out its 62s deadline")
            .expect("task alive");
        assert_eq!(r.error, errors::UNKNOWN_MEMBER_ID);
        assert!(rt.groups.read().unwrap().is_empty());
    }

    /// A group's first join of this process seeds its generation from
    /// the durable ledger high-water mark: the reply generation is
    /// seed + 1, so a post-restart member outranks every pre-restart
    /// ledger row (restart death-zone regression, e2e companion in
    /// tests/kafka_group_failover_e2e.rs).
    #[tokio::test]
    async fn first_join_seeds_generation_from_ledger() {
        let shared = crate::state::testutil::shared_with(crate::state::testutil::test_config());
        let stream = b"t/p0".to_vec();
        let (slot, prefix) = crate::hash::slot_with_prefix(&stream);
        let _ = slot;
        let mut batch = rocksdb::WriteBatch::default();
        crate::kafka::ledger::put_rows(
            &mut batch,
            &[crate::kafka::ledger::LedgerRow {
                stream: stream.clone(),
                group: b"g1".to_vec(),
                prefix: prefix.clone(),
                committed_ordinal: 3,
                generation: 5, // a pre-restart group that reached gen 5
                leader: "old-member".into(),
            }],
        );
        crate::store::ops::batch_write(&shared.store, batch).unwrap();

        let rt = Arc::new(CoordRuntime::new());
        let req = JoinReq {
            group: "g1".into(),
            member_id: "fresh".into(),
            candidate_member_id: "fresh".into(),
            instance_id: None,
            client_id: "c".into(),
            client_host: "h".into(),
            session_timeout_ms: 30_000,
            rebalance_timeout_ms: 60_000,
            protocol_type: "consumer".into(),
            protocol_name: "range".into(),
            metadata: b"sub".to_vec(),
        };
        let r = join_group(&rt, req, &shared.store).await;
        assert_eq!(r.error, errors::NONE);
        assert_eq!(r.generation, 6, "resumes above the ledger gen 5");

        // Unknown group (no ledger rows): unchanged, counts from zero.
        let req2 = JoinReq {
            group: "g2".into(),
            member_id: "m".into(),
            candidate_member_id: "m".into(),
            instance_id: None,
            client_id: "c".into(),
            client_host: "h".into(),
            session_timeout_ms: 30_000,
            rebalance_timeout_ms: 60_000,
            protocol_type: "consumer".into(),
            protocol_name: "range".into(),
            metadata: b"sub".to_vec(),
        };
        let r2 = join_group(&rt, req2, &shared.store).await;
        assert_eq!(r2.error, errors::NONE);
        assert_eq!(r2.generation, 1);
    }
}
