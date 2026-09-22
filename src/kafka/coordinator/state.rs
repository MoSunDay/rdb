//! Pure group-coordination state machine (no IO, no clocks): the
//! coordinator runtime holds one `GroupState` per group and runs these
//! transition functions under its write lock, applying the returned
//! event list (`session::notify_events` wakes parked JoinGroup/
//! SyncGroup waiters).
//!
//! Eager-rebalance model (the classic protocol, pre-KIP-429):
//! Empty -> PreparingRebalance (first join / membership change) ->
//! CompletingSync (join barrier: every known member rejoined, leader
//! elected = oldest joiner, generation bumped) -> Stable (leader's
//! SyncGroup assignments stored) -> PreparingRebalance (leave/expire/
//! rejoin) -> ... -> Empty (last member gone, generation bumped again).
//!
//! Dead is DescribeGroups-speak for "unknown group"; the runtime never
//! stores a Dead state (an absent map entry IS Dead).

use std::collections::HashMap;

use crate::kafka::errors as code;

/// Group lifecycle stage (the DescribeGroups state strings).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GroupStage {
    Empty,
    PreparingRebalance,
    CompletingSync,
    Stable,
    Dead,
}

impl GroupStage {
    /// The DescribeGroups wire string.
    pub fn name(self) -> &'static str {
        match self {
            GroupStage::Empty => "Empty",
            GroupStage::PreparingRebalance => "PreparingRebalance",
            GroupStage::CompletingSync => "CompletingSync",
            GroupStage::Stable => "Stable",
            GroupStage::Dead => "Dead",
        }
    }
}

/// One consumer member. Deadlines are wall-clock ms; `joined` tracks
/// the PreparingRebalance barrier, `join_gen` the generation this
/// member's JoinGroup reply carried (set at barrier completion).
#[derive(Clone, Debug)]
pub struct Member {
    pub session_timeout_ms: i64,
    pub rebalance_timeout_ms: i64,
    pub client_id: String,
    pub client_host: String,
    pub instance_id: Option<String>,
    /// First protocol of the member's subscription list (the candidate
    /// the leader-side assignor runs under).
    pub protocol_name: String,
    pub metadata: Vec<u8>,
    pub assignment: Vec<u8>,
    pub session_deadline_ms: i64,
    pub rebalance_deadline_ms: i64,
    pub joined: bool,
    pub join_gen: i32,
}

/// Live group state (in-memory only: restart drops membership, clients
/// rejoin natively; committed offsets live in the kind 0x20 ledger).
#[derive(Clone, Debug)]
pub struct GroupState {
    pub stage: GroupStage,
    pub generation: i32,
    pub protocol_type: String,
    pub protocol_name: Option<String>,
    pub leader: String,
    pub members: HashMap<String, Member>,
    /// Member ids oldest-joiner-first (the leader-election order).
    pub order: Vec<String>,
}

/// What the runtime should do after a transition (every event wakes
/// the group's parked waiters; over-notifying is harmless).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// Join barrier completed: parked JoinGroup calls can answer.
    BarrierComplete,
    /// Leader's assignments stored: parked SyncGroup calls can answer.
    SyncReady,
    /// Membership changed (kick/leave/expire): re-evaluate waiters.
    RebalanceKicked,
    /// Last member gone: group is Empty (waiters see themselves gone).
    GroupEmpty,
}

/// Inputs of one JoinGroup call (ids pre-resolved by the caller: the
/// runtime generates the candidate member id).
pub struct JoinArgs<'a> {
    pub member_id: &'a str,
    pub instance_id: Option<&'a str>,
    pub client_id: &'a str,
    pub client_host: &'a str,
    pub session_timeout_ms: i64,
    pub rebalance_timeout_ms: i64,
    pub protocol_type: &'a str,
    pub protocol_name: &'a str,
    pub metadata: &'a [u8],
    pub now_ms: i64,
}

/// Result of the join transition.
#[derive(PartialEq, Eq, Debug)]
pub enum JoinOutcome {
    /// Empty member id: reply UNKNOWN_MEMBER_ID + the candidate id (the
    /// client re-joins with it; the standard two-phase handshake).
    NeedMemberId,
    /// Static-membership conflict (same instance id, other member id).
    FencedInstanceId,
    /// protocol_type differs from the group's registered one.
    InconsistentProtocol,
    /// Parked until the barrier completes (or the rebalance deadline).
    Waiting,
    /// Barrier completed with this member in it.
    Joined { is_leader: bool },
}

/// Result of the sync transition.
#[derive(PartialEq, Eq, Debug)]
pub enum SyncOutcome {
    Ok,
    /// Follower parked until the leader's assignments land.
    Waiting,
    UnknownMember,
    IllegalGeneration,
    Rebalance,
}

/// Fresh group (Empty, generation 0; the first barrier bumps it to 1).
pub fn new_group(protocol_type: &str) -> GroupState {
    new_group_seeded(protocol_type, 0)
}

/// Fresh group whose generation resumes at `generation` -- the restart
/// path: the first join after a restart seeds the runtime counter from
/// the durable ledger's high-water generation (`join::join_group`), so
/// the first runtime generation already tops every pre-restart ledger
/// row and the cross-restart fencing check in `offsets_commit::commit_one`
/// never rejects a legitimate new member (the pre-fix death zone).
pub fn new_group_seeded(protocol_type: &str, generation: i32) -> GroupState {
    GroupState {
        stage: GroupStage::Empty,
        generation,
        protocol_type: protocol_type.to_string(),
        protocol_name: None,
        leader: String::new(),
        members: HashMap::new(),
        order: Vec::new(),
    }
}

/// Transition to Empty (generation bumped: zombie commits of the dead
/// generation stay behind the group's high-water generation; the
/// committed-offset ledger rows are kept untouched).
pub fn to_empty(mut st: GroupState) -> GroupState {
    st.stage = GroupStage::Empty;
    st.generation += 1;
    st.leader.clear();
    st.protocol_name = None;
    st.members.clear();
    st.order.clear();
    st
}

/// JoinGroup transition: kicks Stable/CompletingSync groups back into
/// PreparingRebalance, registers/refreshes the member, completes the
/// barrier when every known member has (re)joined.
pub fn join(mut st: GroupState, a: &JoinArgs) -> (GroupState, Vec<Event>, JoinOutcome) {
    if a.member_id.is_empty() {
        return (st, vec![], JoinOutcome::NeedMemberId);
    }
    if let Some(x) = a.instance_id {
        let mine = st.members.get(a.member_id).and_then(|m| m.instance_id.as_deref());
        if mine != Some(x) && st.members.values().any(|m| m.instance_id.as_deref() == Some(x)) {
            return (st, vec![], JoinOutcome::FencedInstanceId);
        }
    }
    if !st.protocol_type.is_empty() && st.protocol_type != a.protocol_type {
        return (st, vec![], JoinOutcome::InconsistentProtocol);
    }
    let mut events = Vec::new();
    match st.stage {
        GroupStage::Stable | GroupStage::CompletingSync => {
            st.stage = GroupStage::PreparingRebalance;
            st.protocol_name = None;
            for m in st.members.values_mut() {
                m.joined = false;
            }
            events.push(Event::RebalanceKicked);
        }
        GroupStage::Empty => {
            st.stage = GroupStage::PreparingRebalance;
            st.protocol_type = a.protocol_type.to_string();
        }
        _ => {}
    }
    match st.members.get_mut(a.member_id) {
        Some(m) => {
            m.session_timeout_ms = a.session_timeout_ms;
            m.rebalance_timeout_ms = a.rebalance_timeout_ms;
            m.client_id = a.client_id.to_string();
            m.client_host = a.client_host.to_string();
            m.instance_id = a.instance_id.map(str::to_string);
            m.protocol_name = a.protocol_name.to_string();
            m.metadata = a.metadata.to_vec();
            m.session_deadline_ms = a.now_ms + a.session_timeout_ms;
            m.rebalance_deadline_ms = a.now_ms + a.rebalance_timeout_ms;
            m.joined = true;
        }
        None => {
            let member = Member {
                session_timeout_ms: a.session_timeout_ms,
                rebalance_timeout_ms: a.rebalance_timeout_ms,
                client_id: a.client_id.to_string(),
                client_host: a.client_host.to_string(),
                instance_id: a.instance_id.map(str::to_string),
                protocol_name: a.protocol_name.to_string(),
                metadata: a.metadata.to_vec(),
                assignment: Vec::new(),
                session_deadline_ms: a.now_ms + a.session_timeout_ms,
                rebalance_deadline_ms: a.now_ms + a.rebalance_timeout_ms,
                joined: true,
                join_gen: st.generation,
            };
            st.members.insert(a.member_id.to_string(), member);
            st.order.push(a.member_id.to_string());
        }
    }
    if st.members.values().all(|m| m.joined) {
        let is_leader = st.order.first().map(String::as_str) == Some(a.member_id);
        let (st2, mut ev) = complete_barrier(st, a.now_ms);
        events.append(&mut ev);
        return (st2, events, JoinOutcome::Joined { is_leader });
    }
    (st, events, JoinOutcome::Waiting)
}

/// Complete the join barrier: drop members that never rejoined, elect
/// the oldest joiner as leader, bump the generation, arm per-member
/// session/rebalance deadlines and hand the protocol to the leader.
pub fn complete_barrier(mut st: GroupState, now_ms: i64) -> (GroupState, Vec<Event>) {
    st.members.retain(|_, m| m.joined);
    st.order.retain(|id| st.members.contains_key(id));
    if st.members.is_empty() {
        return (to_empty(st), vec![Event::GroupEmpty]);
    }
    st.generation += 1;
    st.leader = st.order[0].clone();
    st.protocol_name = Some(st.members[&st.leader].protocol_name.clone());
    st.stage = GroupStage::CompletingSync;
    for m in st.members.values_mut() {
        m.joined = false;
        m.join_gen = st.generation;
        m.session_deadline_ms = now_ms + m.session_timeout_ms;
        m.rebalance_deadline_ms = now_ms + m.rebalance_timeout_ms;
    }
    (st, vec![Event::BarrierComplete])
}

/// Kick a Stable/CompletingSync group back to PreparingRebalance with
/// every member marked not-yet-rejoined (shared by leave/expire).
pub fn kick(mut st: GroupState) -> GroupState {
    st.stage = GroupStage::PreparingRebalance;
    st.protocol_name = None;
    for m in st.members.values_mut() {
        m.joined = false;
    }
    st
}

/// SyncGroup transition. Role is decided server-side (the wire cannot
/// tell an empty leader assignment list from a follower probe): during
/// CompletingSync only the LEADER's call distributes and settles; a
/// follower's parks (Waiting). `Stable` re-syncs answer idempotently.
pub fn sync(
    mut st: GroupState,
    member_id: &str,
    generation: i32,
    assignments: &[(String, Vec<u8>)],
    now_ms: i64,
) -> (GroupState, Vec<Event>, SyncOutcome) {
    let Some(m) = st.members.get(member_id) else {
        return (st, vec![], SyncOutcome::UnknownMember);
    };
    if generation != st.generation || m.join_gen != st.generation {
        return (st, vec![], SyncOutcome::IllegalGeneration);
    }
    match st.stage {
        GroupStage::PreparingRebalance => (st, vec![], SyncOutcome::Rebalance),
        GroupStage::Stable => (st, vec![], SyncOutcome::Ok),
        GroupStage::CompletingSync => {
            if st.leader != member_id {
                return (st, vec![], SyncOutcome::Waiting);
            }
            for (id, bytes) in assignments {
                if let Some(m) = st.members.get_mut(id.as_str()) {
                    m.assignment = bytes.clone();
                }
            }
            st.stage = GroupStage::Stable;
            for m in st.members.values_mut() {
                m.session_deadline_ms = now_ms + m.session_timeout_ms;
            }
            (st, vec![Event::SyncReady], SyncOutcome::Ok)
        }
        _ => (st, vec![], SyncOutcome::UnknownMember),
    }
}

/// Heartbeat transition: refreshes the session deadline except while a
/// rebalance is in flight (the client must rejoin instead).
pub fn heartbeat(mut st: GroupState, member_id: &str, generation: i32, now_ms: i64) -> (GroupState, Vec<Event>, i16) {
    let Some(m) = st.members.get_mut(member_id) else {
        return (st, vec![], code::UNKNOWN_MEMBER_ID);
    };
    if generation != st.generation {
        return (st, vec![], code::ILLEGAL_GENERATION);
    }
    if st.stage == GroupStage::PreparingRebalance {
        return (st, vec![], code::REBALANCE_IN_PROGRESS);
    }
    m.session_deadline_ms = now_ms + m.session_timeout_ms;
    (st, vec![], code::NONE)
}

/// LeaveGroup transition (multi-member at the state layer; the v0-v2
/// wire carries one). Returns per-request-member error codes.
pub fn leave(mut st: GroupState, member_ids: &[String], now_ms: i64) -> (GroupState, Vec<Event>, Vec<i16>) {
    let mut codes = Vec::new();
    let mut removed = false;
    for id in member_ids {
        if st.members.remove(id).is_some() {
            st.order.retain(|x| x != id);
            removed = true;
            codes.push(code::NONE);
        } else {
            codes.push(code::UNKNOWN_MEMBER_ID);
        }
    }
    if !removed {
        return (st, vec![], codes);
    }
    if st.members.is_empty() {
        return (to_empty(st), vec![Event::GroupEmpty], codes);
    }
    if st.stage == GroupStage::PreparingRebalance {
        if st.members.values().all(|m| m.joined) {
            // The leaver was the only missing rejoin: finish the wait.
            let (st, events) = complete_barrier(st, now_ms);
            return (st, events, codes);
        }
        return (st, vec![Event::RebalanceKicked], codes);
    }
    (kick(st), vec![Event::RebalanceKicked], codes)
}

/// Sweep transition (the ~500ms background pass): PreparingRebalance
/// groups drop members whose rebalance deadline passed (then finish
/// with whoever did rejoin), Stable/CompletingSync groups drop members
/// whose session deadline passed (then rebalance the remainder).
pub fn expire(mut st: GroupState, now_ms: i64) -> (GroupState, Vec<Event>) {
    match st.stage {
        GroupStage::Empty | GroupStage::Dead => (st, vec![]),
        GroupStage::PreparingRebalance => {
            let before = st.members.len();
            // keep while the deadline has not STRICTLY passed (a sweep exactly at
            // the deadline still sees the member alive)
            st.members.retain(|_, m| m.joined || m.rebalance_deadline_ms >= now_ms);
            st.order.retain(|id| st.members.contains_key(id));
            if st.members.len() == before {
                return (st, vec![]);
            }
            if st.members.is_empty() {
                return (to_empty(st), vec![Event::GroupEmpty]);
            }
            if st.members.values().all(|m| m.joined) {
                return complete_barrier(st, now_ms);
            }
            (st, vec![Event::RebalanceKicked])
        }
        GroupStage::Stable | GroupStage::CompletingSync => {
            let before = st.members.len();
            st.members.retain(|_, m| m.session_deadline_ms >= now_ms);
            st.order.retain(|id| st.members.contains_key(id));
            if st.members.len() == before {
                return (st, vec![]);
            }
            if st.members.is_empty() {
                return (to_empty(st), vec![Event::GroupEmpty]);
            }
            (kick(st), vec![Event::RebalanceKicked])
        }
    }
}
