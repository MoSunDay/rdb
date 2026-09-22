//! JoinGroup (11) v0-v4 and SyncGroup (14) v0-v4 wire handlers: parse
//! the request ladder, drive the async barrier (`join.rs`), encode the
//! reply ladder.
//!
//! Version-schema notes (anchored by the roundtrip tests in
//! `group_api_tests.rs`, gates verified against the canonical Kafka
//! message schemas -- librdkafka omits instance_id on v4 joins):
//! - JoinGroup v0 request already carries protocol_type (it is NOT a
//!   v1 field); v1 adds rebalance_timeout_ms AND the two-phase
//!   member-id flow; v5 (not v4!) adds group_instance_id between
//!   member_id and protocol_type. Response: v2 adds throttle_time_ms
//!   (first field), v1 adds nullable protocol_name; protocol_type and
//!   member-row group_instance_id join at v5+/v7+. v0-v5 are all
//!   classic-framed (v6 is the first flexible version).
//! - SyncGroup v0 already carries generation_id AND the assignments
//!   array (there is no single-assignment v0). v3 adds
//!   group_instance_id; v4 switches to flexible framing (compact
//!   strings/arrays + tagged tails). Response: v1 adds
//!   throttle_time_ms (first); protocol_type/protocol_name join at v5
//!   (above this cap, never emitted), then the assignment bytes.
//! - JoinGroup with an EMPTY member id: v1+ answers UNKNOWN_MEMBER_ID
//!   carrying the generated id (the two-phase handshake); v0 predates
//!   the handshake, so the id is assigned and the join proceeds.

use std::sync::Arc;

use super::join::{self, JoinReq};
use super::CoordRuntime;
use crate::kafka::frame::{
    put_array_len, put_bytes, put_i16, put_i32, put_nullable_string, put_string, Reader,
};

/// Handle JoinGroup v0-v4. `shared` reaches the durable offset ledger
/// so a group's first join of this process can seed its generation
/// above the ledger high-water mark (see `join::join_group`).
pub async fn handle_join_group(
    body: &mut Reader<'_>,
    version: i16,
    rt: &Arc<CoordRuntime>,
    shared: &crate::state::Shared,
    client_id: Option<&str>,
    client_host: &str,
) -> Result<Vec<u8>, String> {
    let bad = || "malformed joingroup request".to_string();
    let group = body.string().ok_or_else(bad)?;
    let session_ms = body.i32().ok_or_else(bad)? as i64;
    let rebalance_ms = if version >= 1 { body.i32().ok_or_else(bad)? as i64 } else { session_ms };
    let mut member_id = body.string().ok_or_else(bad)?;
    // Canonical gate: group_instance_id arrives at v5 (v4 only added
    // the two-phase member-id flow) -- librdkafka omits it on v4.
    let instance_id = if version >= 5 {
        body.nullable_string().ok_or_else(bad)?
    } else {
        None
    };
    let protocol_type = body.string().ok_or_else(bad)?;
    let count = body.array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut protocol_name = String::new();
    let mut metadata = Vec::new();
    for _ in 0..count {
        let name = body.string().ok_or_else(bad)?;
        let md = body.bytes().ok_or_else(bad)?.unwrap_or(&[]).to_vec();
        if protocol_name.is_empty() {
            protocol_name = name;
            metadata = md;
        }
    }
    let candidate = super::next_member_id(rt, &group);
    // v0 predates the two-phase handshake: an empty id is assigned.
    if member_id.is_empty() && version < 1 {
        member_id = candidate.clone();
    }
    let req = JoinReq {
        group,
        member_id,
        candidate_member_id: candidate,
        instance_id,
        client_id: client_id.unwrap_or("").to_string(),
        client_host: client_host.to_string(),
        session_timeout_ms: session_ms,
        rebalance_timeout_ms: rebalance_ms,
        protocol_type,
        protocol_name,
        metadata,
    };
    let r = join::join_group(rt, req, &shared.store).await;
    let mut out = Vec::new();
    if version >= 2 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    put_i16(&mut out, r.error);
    put_i32(&mut out, r.generation);
    // protocol_type joins the response only at v7 (protocol_name v1+).
    if version >= 7 {
        put_nullable_string(&mut out, r.protocol_type.as_deref());
    }
    if version >= 1 {
        put_nullable_string(&mut out, r.protocol_name.as_deref());
    } else {
        put_string(&mut out, r.protocol_name.as_deref().unwrap_or(""));
    }
    put_string(&mut out, &r.leader);
    put_string(&mut out, &r.member_id);
    put_array_len(&mut out, r.members.len());
    for m in &r.members {
        put_string(&mut out, &m.member_id);
        if version >= 5 {
            put_nullable_string(&mut out, m.instance_id.as_deref());
        }
        put_bytes(&mut out, &m.metadata);
    }
    Ok(out)
}

/// Handle SyncGroup v0-v4.
pub async fn handle_sync_group(
    body: &mut Reader<'_>,
    version: i16,
    rt: &Arc<CoordRuntime>,
) -> Result<Vec<u8>, String> {
    let bad = || "malformed syncgroup request".to_string();
    let (group, generation, member, list) = if version >= 4 {
        // v4 is the first flexible SyncGroup (compact framing + tags).
        let group = body.compact_string().ok_or_else(bad)?;
        let generation = body.i32().ok_or_else(bad)?;
        let member = body.compact_string().ok_or_else(bad)?;
        let _instance = body.compact_nullable_string().ok_or_else(bad)?;
        let list = parse_assignments_compact(body)?;
        body.skip_tagged_fields().ok_or_else(bad)?;
        (group, generation, member, list)
    } else {
        let group = body.string().ok_or_else(bad)?;
        let generation = body.i32().ok_or_else(bad)?;
        let member = body.string().ok_or_else(bad)?;
        if version >= 3 {
            // group_instance_id (v3+, classic nullable string): static
            // membership is not supported; parse and ignore.
            body.nullable_string().ok_or_else(bad)?;
        }
        let count = body.array_len().ok_or_else(bad)?.unwrap_or(0);
        let mut list = Vec::new();
        for _ in 0..count {
            let id = body.string().ok_or_else(bad)?;
            let a = body.bytes().ok_or_else(bad)?.unwrap_or(&[]).to_vec();
            list.push((id, a));
        }
        (group, generation, member, list)
    };
    // The role (leader vs follower) is decided server-side by the
    // coordinator -- both send an assignments array on the wire.
    sync_and_encode(rt, version, group, member, generation, list).await
}

/// Shared tail: drive the sync barrier and encode the reply ladder.
async fn sync_and_encode(
    rt: &Arc<CoordRuntime>,
    version: i16,
    group: String,
    member: String,
    generation: i32,
    assignments: Vec<(String, Vec<u8>)>,
) -> Result<Vec<u8>, String> {
    let r = join::sync_group(rt, &group, &member, generation, assignments).await;
    let mut out = Vec::new();
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    put_i16(&mut out, r.error);
    // protocol_type/protocol_name join the SyncGroup reply at v5 --
    // above this front's v4 cap, so they are never emitted.
    put_bytes(&mut out, &r.assignment);
    if version >= 4 {
        crate::kafka::frame::put_empty_tagged_fields(&mut out);
    }
    Ok(out)
}

/// Flexible (v4+) assignments array: compact strings/bytes.
fn parse_assignments_compact(body: &mut Reader<'_>) -> Result<Vec<(String, Vec<u8>)>, String> {
    let bad = || "malformed syncgroup assignments".to_string();
    let count = body.compact_array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut list = Vec::new();
    for _ in 0..count {
        let id = body.compact_string().ok_or_else(bad)?;
        let a = body.compact_bytes().ok_or_else(bad)?.unwrap_or(&[]).to_vec();
        list.push((id, a));
    }
    Ok(list)
}
