//! Transition-table coverage for `state.rs`: the full eager-rebalance
//! cycle Empty -> PR -> CS -> Stable -> PR -> ... -> Empty, session
//! expiry, rebalance-deadline expiry, static fencing and the join
//! barrier's all-joined / not-yet branches. All calls are pure; `now`
//! is an explicit parameter.

use super::state::{
    complete_barrier, expire, heartbeat, join, leave, new_group, sync, to_empty, Event,
    GroupStage, JoinArgs, JoinOutcome, SyncOutcome,
};
use crate::kafka::errors;

fn args(member: &str, session_ms: i64, now: i64) -> JoinArgs<'_> {
    JoinArgs {
        member_id: member,
        instance_id: None,
        client_id: "c",
        client_host: "127.0.0.1",
        session_timeout_ms: session_ms,
        rebalance_timeout_ms: session_ms * 4,
        protocol_type: "consumer",
        protocol_name: "range",
        metadata: b"sub",
        now_ms: now,
    }
}

/// Two members through one barrier: both join replies share the
/// generation/leader; the leader is the OLDEST joiner.
#[test]
fn empty_to_stable_via_barrier() {
    let st = new_group("consumer");
    // First join: barrier incomplete (nobody else to wait for is a
    // one-member group, so it completes immediately).
    let (st, ev, out) = join(st, &args("m1", 1000, 0));
    assert_eq!(out, JoinOutcome::Joined { is_leader: true });
    assert!(ev.contains(&Event::BarrierComplete));
    assert_eq!(st.stage, GroupStage::CompletingSync);
    assert_eq!(st.generation, 1);
    assert_eq!(st.leader, "m1");

    // Leader syncs: assignments stored, group Stable.
    let assigns = vec![("m1".to_string(), b"a1".to_vec())];
    let (st, ev, out) = sync(st, "m1", 1, &assigns, 10);
    assert_eq!(out, SyncOutcome::Ok);
    assert!(ev.contains(&Event::SyncReady));
    assert_eq!(st.stage, GroupStage::Stable);
    assert_eq!(st.members["m1"].assignment, b"a1");
}

/// The full cycle: m1/m2 stable, m3 joins -> kick -> both rejoin ->
/// barrier -> sync -> m2 leaves -> m1 alone -> m1 leaves -> Empty.
#[test]
fn full_cycle_with_kicks_and_empty() {
    let st = new_group("consumer");
    let (st, _, _) = join(st, &args("m1", 1000, 0));
    // m2 joining a CompletingSync group kicks nothing (still CS: the
    // leader has not synced yet) -- m2 joins the SAME barrier only
    // after a fresh kick. Kick explicitly via the Stable path.
    let assigns = vec![("m1".to_string(), b"a".to_vec())];
    let (st, _, _) = sync(st, "m1", 1, &assigns, 5);
    assert_eq!(st.stage, GroupStage::Stable);

    // Stable + new member -> PreparingRebalance, m1 must rejoin.
    let (st, ev, out) = join(st, &args("m2", 1000, 10));
    assert_eq!(out, JoinOutcome::Waiting);
    assert!(ev.contains(&Event::RebalanceKicked));
    assert_eq!(st.stage, GroupStage::PreparingRebalance);
    assert!(!st.members["m1"].joined);

    // m1's heartbeat during PR: REBALANCE_IN_PROGRESS.
    let (_, _, hb) = heartbeat(st.clone(), "m1", 1, 11);
    assert_eq!(hb, errors::REBALANCE_IN_PROGRESS);

    let (st, _, out) = join(st, &args("m1", 1000, 12));
    assert_eq!(out, JoinOutcome::Joined { is_leader: true }, "oldest joiner leads");
    assert_eq!(st.generation, 2);
    let assigns = vec![
        ("m1".to_string(), b"a1".to_vec()),
        ("m2".to_string(), b"a2".to_vec()),
    ];
    let (st, _, _) = sync(st, "m1", 2, &assigns, 13);
    assert_eq!(st.stage, GroupStage::Stable);

    // m2 leaves: m1 alone, back to PR.
    let (st, ev, codes) = leave(st, &["m2".to_string()], 14);
    assert_eq!(codes, vec![errors::NONE]);
    assert!(ev.contains(&Event::RebalanceKicked));
    assert_eq!(st.stage, GroupStage::PreparingRebalance);
    assert_eq!(st.members.len(), 1);

    // m1 rejoins (generation stays 2 until the next barrier -> 3).
    let (st, _, out) = join(st, &args("m1", 1000, 15));
    assert_eq!(out, JoinOutcome::Joined { is_leader: true });
    assert_eq!(st.generation, 3);
    let (st, _, _) = sync(st, "m1", 3, &[("m1".to_string(), b"a".to_vec())], 16);

    // Last member leaves: Empty + generation bump, ledger untouched.
    let (st, ev, codes) = leave(st, &["m1".to_string()], 17);
    assert_eq!(codes, vec![errors::NONE]);
    assert!(ev.contains(&Event::GroupEmpty));
    assert_eq!(st.stage, GroupStage::Empty);
    assert_eq!(st.generation, 4);
    assert_eq!(st.stage.name(), "Empty");
}

#[test]
fn join_barrier_waits_for_straggler() {
    let st = new_group("consumer");
    let (st, _, out) = join(st, &args("m1", 1000, 0));
    assert_eq!(out, JoinOutcome::Joined { is_leader: true });
    let (st, ev, out) = join(st, &args("m2", 1000, 5));
    assert_eq!(out, JoinOutcome::Waiting);
    assert!(ev.contains(&Event::RebalanceKicked));
    assert_eq!(st.stage, GroupStage::PreparingRebalance);
    // m1 rejoins -> barrier completes for BOTH.
    let (st, _, out1) = join(st.clone(), &args("m1", 1000, 6));
    let is_lead = matches!(out1, JoinOutcome::Joined { is_leader: true });
    assert!(is_lead);
    assert_eq!(st.members.len(), 2);
    assert_eq!(st.members.values().filter(|m| m.join_gen == 2).count(), 2);
}

#[test]
fn empty_member_id_and_fence_paths() {
    let st = new_group("consumer");
    let (st, ev, out) = join(st.clone(), &args("", 1000, 0));
    assert_eq!(out, JoinOutcome::NeedMemberId);
    assert!(ev.is_empty());
    // protocol_type mismatch against a registered group.
    let (st, _, _) = join(st, &args("m1", 1000, 0));
    let mut bad = args("m2", 1000, 1);
    bad.protocol_type = "connect";
    let (st, _, out) = join(st.clone(), &bad);
    assert_eq!(out, JoinOutcome::InconsistentProtocol);
    assert_eq!(st.members.len(), 1, "rejected join mutates nothing");
    // Static-membership conflict: same instance id, other member id.
    let mut with_inst = args("m2", 1000, 1);
    with_inst.instance_id = Some("i-1");
    let (st, _, _) = join(st, &with_inst);
    let mut clash = args("m3", 1000, 2);
    clash.instance_id = Some("i-1");
    let (_, _, out) = join(st, &clash);
    assert_eq!(out, JoinOutcome::FencedInstanceId);
    // The SAME member rejoining with its instance id is fine.
    let (_, _, out) = join(new_group("consumer"), &with_inst);
    assert_eq!(out, JoinOutcome::Joined { is_leader: true });
}

#[test]
fn sync_fencing_matrix() {
    let st = new_group("consumer");
    let (st, _, _) = join(st, &args("m1", 1000, 0));
    // Unknown member / stale generation / wrong-stage replies.
    let (st, _, out) = sync(st.clone(), "mx", 1, &[], 1);
    assert_eq!(out, SyncOutcome::UnknownMember);
    let (st, _, out) = sync(st.clone(), "m1", 0, &[], 1);
    assert_eq!(out, SyncOutcome::IllegalGeneration);
    // m2 joins the group (kick), m1 rejoins as leader, m2 is the
    // follower whose probe parks.
    let (st, _, _) = join(st, &args("m2", 1000, 1));
    let (st, _, _) = join(st, &args("m1", 1000, 1));
    let (st, _, out) = sync(st.clone(), "m2", 2, &[], 1);
    assert_eq!(out, SyncOutcome::Waiting, "follower parks in CS");
    let (st, ev, out) = sync(st, "m1", 2, &[("m1".to_string(), b"x".to_vec())], 2);
    assert_eq!(out, SyncOutcome::Ok);
    assert!(ev.contains(&Event::SyncReady));
    // After Stable: re-sync is idempotent; a kicked group answers 27.
    let (st, _, out) = sync(st.clone(), "m1", 2, &[], 3);
    assert_eq!(out, SyncOutcome::Ok);
    let (st, _, _) = join(st, &args("m2", 1000, 3)); // kick
    let (_, _, out) = sync(st, "m1", 2, &[], 4);
    assert_eq!(out, SyncOutcome::Rebalance);
}

#[test]
fn session_expiry_kicks_and_empties() {
    let st = new_group("consumer");
    let (st, _, _) = join(st, &args("m1", 500, 0));
    let (st, _, _) = sync(st, "m1", 1, &[("m1".to_string(), b"x".to_vec())], 1);
    assert_eq!(st.stage, GroupStage::Stable);
    // Before the deadline: no change.
    let (st, ev) = expire(st.clone(), 500);
    assert!(ev.is_empty());
    // m2 joins so the expiry leaves one member behind.
    let (st, _, _) = join(st, &args("m2", 500, 10));
    let (st, _, _) = join(st, &args("m1", 500, 11)); // barrier -> gen 2
    let (st, _, _) = sync(st, "m1", 2, &[], 12); // Stable, sessions 12+500
    // m1 keeps heartbeating (session armed to 10_300); m2 goes quiet
    // and its session (512) lapses.
    let (st, _, code) = heartbeat(st, "m1", 2, 9800);
    assert_eq!(code, errors::NONE);
    let (st, ev) = expire(st, 10_000);
    assert!(ev.contains(&Event::RebalanceKicked));
    assert_eq!(st.stage, GroupStage::PreparingRebalance);
    assert_eq!(st.members.len(), 1);
    assert!(st.members.contains_key("m1"));
    // The survivor never rejoins; its rebalance deadline (11 + 2000)
    // lapses too: Empty with a generation bump.
    let (st, ev) = expire(st, 10_000);
    assert!(ev.contains(&Event::GroupEmpty));
    assert_eq!(st.stage, GroupStage::Empty);
    assert_eq!(st.generation, 3);
}

#[test]
fn rebalance_deadline_completes_with_rejoined() {
    let st = new_group("consumer");
    let (st, _, _) = join(st, &args("m1", 1000, 0)); // barrier gen 1
    let (st, _, _) = sync(st, "m1", 1, &[], 1); // Stable
    // m2 joins: kick into PreparingRebalance. m2 rejoined, m1 did not.
    let (st, _, _) = join(st, &args("m2", 1000, 2));
    assert_eq!(st.stage, GroupStage::PreparingRebalance);
    assert!(st.members["m2"].joined);
    assert!(!st.members["m1"].joined);
    // Before m1's rebalance deadline (0 + 4000): nothing changes.
    let (st, ev) = expire(st.clone(), 4000);
    assert!(ev.is_empty(), "at the deadline the member is still alive");
    // Past it: m1 (never rejoined) drops and m2 carries the barrier.
    let (st, ev) = expire(st, 4001);
    assert!(ev.contains(&Event::BarrierComplete), "deadline completes with the rejoined");
    assert_eq!(st.stage, GroupStage::CompletingSync);
    assert!(!st.members.contains_key("m1"));
    assert_eq!(st.leader, "m2");
    assert_eq!(st.generation, 2);
}

#[test]
fn leave_during_prepare_completes_barrier() {
    let st = new_group("consumer");
    let (st, _, _) = join(st, &args("m1", 1000, 0)); // barrier -> CS gen 1
    let (st, _, _) = join(st, &args("m2", 1000, 1)); // kick -> PR, m2 rejoined
    // m1 is the only member that has NOT rejoined: its leave finishes
    // the barrier for m2 alone.
    let (st, ev, codes) = leave(st.clone(), &["m1".to_string()], 2);
    assert_eq!(codes, vec![errors::NONE]);
    assert!(ev.contains(&Event::BarrierComplete));
    assert_eq!(st.stage, GroupStage::CompletingSync);
    assert_eq!(st.leader, "m2");
    assert_eq!(st.generation, 2);
    // Leaving an unknown member: per-member 25, no state change.
    let (st, ev, codes) = leave(st.clone(), &["mx".to_string()], 3);
    assert_eq!(codes, vec![errors::UNKNOWN_MEMBER_ID]);
    assert!(ev.is_empty());
    assert_eq!(st.stage, GroupStage::CompletingSync);
    // Leaving the last member: Empty with another generation bump.
    let (st, ev, codes) = leave(st, &["m2".to_string()], 4);
    assert_eq!(codes, vec![errors::NONE]);
    assert!(ev.contains(&Event::GroupEmpty));
    assert_eq!(st.stage, GroupStage::Empty);
    assert_eq!(st.generation, 3);
    // On a group that does not exist: per-member 25, no events.
    let (_, ev, codes) = leave(new_group("consumer"), &["mx".to_string()], 5);
    assert_eq!(codes, vec![errors::UNKNOWN_MEMBER_ID]);
    assert!(ev.is_empty());
}

#[test]
fn heartbeat_refresh_and_errors() {
    let st = new_group("consumer");
    let (st, _, _) = join(st, &args("m1", 1000, 0));
    let (st, _, code) = heartbeat(st.clone(), "mx", 1, 1);
    assert_eq!(code, errors::UNKNOWN_MEMBER_ID);
    let (st, _, code) = heartbeat(st.clone(), "m1", 7, 1);
    assert_eq!(code, errors::ILLEGAL_GENERATION);
    let (st, _, code) = heartbeat(st, "m1", 1, 900);
    assert_eq!(code, errors::NONE);
    assert_eq!(st.members["m1"].session_deadline_ms, 1900, "deadline refreshed");
    let (_, ev) = expire(st, 1900);
    assert!(ev.is_empty(), "refresh beat the sweep");
}

#[test]
fn barrier_completes_empty_when_all_lapsed() {
    // A PR where the only members are lapsed non-rejoiners: Empty.
    let st = new_group("consumer");
    let (st, _, _) = join(st, &args("m1", 500, 0)); // barrier gen 1 @0
    let (st, _, _) = join(st, &args("m2", 500, 1)); // kick -> PR
    let (st, _, _) = join(st, &args("m1", 500, 2)); // barrier gen 2 @2
    let (st, _, _) = sync(st, "m1", 2, &[], 3); // Stable, sessions 503
    // m1 heartbeats; m2 goes quiet. Sessions were armed at 3 + 500;
    // at exactly 503 both are still alive, past it m2 lapses.
    let (st, _, _) = heartbeat(st, "m1", 2, 600);
    let (st, ev) = expire(st.clone(), 503);
    assert!(ev.is_empty(), "the deadline itself is still alive");
    let (st, ev) = expire(st.clone(), 504);
    assert!(ev.contains(&Event::RebalanceKicked), "m2 lapsed, m1 alive");
    assert_eq!(st.stage, GroupStage::PreparingRebalance);
    assert_eq!(st.members.len(), 1);
    // m1 never rejoins either; its rebalance deadline (2 + 2000) lapses
    // and the memberless group goes Empty.
    let (st, ev) = expire(st, 2100);
    assert!(ev.contains(&Event::GroupEmpty));
    assert_eq!(st.stage, GroupStage::Empty);
    assert_eq!(st.generation, 3);
    // complete_barrier on a memberless group is the same transition.
    let (st, ev) = complete_barrier(st, 11);
    assert_eq!(ev, vec![Event::GroupEmpty]);
    assert_eq!(st.stage.name(), "Empty");
    // to_empty bumps the generation for zombie fencing.
    let g = to_empty(new_group("consumer"));
    assert_eq!(g.generation, 1);
}
