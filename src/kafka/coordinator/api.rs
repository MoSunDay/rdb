//! Coordinator wire APIs (classic framing only in this file):
//! FindCoordinator (10) v0-v1, Heartbeat (12) v0-v4 (v3+ flexible),
//! LeaveGroup (13) v0-v2, DescribeGroups (15) v0-v3.
//!
//! Version-schema notes (anchored by the per-version roundtrip tests
//! in `api_tests.rs`; sources: the official protocol schemas):
//! - FindCoordinator v1 adds coordinator_type (int8) to the request
//!   and error_message (nullable) to the response. Only type 0
//!   (group) exists here; other types answer INVALID_REQUEST.
//! - Heartbeat v1+ responses carry throttle_time_ms BEFORE the error
//!   code; v3 adds group_instance_id (request) and switches to
//!   flexible framing (compact strings + tagged tails).
//! - LeaveGroup v0-v2 requests carry ONE member id (the members array
//!   arrives at v3, which is also flexible -- not advertised); v1+
//!   responses lead with throttle_time_ms.
//! - DescribeGroups v1+ responses lead with throttle_time_ms; v3 adds
//!   the per-group authorized_operations bitmask (unset sentinel here)
//!   and the request tail include_authorized_operations; member rows
//!   gain group_instance_id only at v4. Unknown groups report state
//!   "Dead" with error NONE (the broker behavior).

use super::session;
use super::state;
use super::CoordRuntime;
use crate::kafka::errors;
use crate::kafka::frame::{
    put_array_len, put_bytes, put_i16, put_i32, put_nullable_string, put_string, Reader,
};
use crate::kafka::handshake::NODE_ID;

/// FindCoordinator v0-v1: any group key resolves to THIS broker (the
/// only coordinator; tx-type keys are rejected).
pub fn handle_find_coordinator(body: &mut Reader<'_>, version: i16, ad: &(String, i32)) -> Result<Vec<u8>, String> {
    let bad = || "malformed findcoordinator request".to_string();
    let _key = body.string().ok_or_else(bad)?;
    let ctype = if version >= 1 { body.i8().ok_or_else(bad)? } else { 0 };
    let mut out = Vec::new();
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms (v1+ leads with it)
    }
    if ctype == 0 {
        put_i16(&mut out, errors::NONE);
        if version >= 1 {
            put_nullable_string(&mut out, None);
        }
        put_i32(&mut out, NODE_ID);
        put_string(&mut out, &ad.0);
        put_i32(&mut out, ad.1);
    } else {
        // The coordinator-type registry only defines group (0) and
        // transaction (1); no transaction coordinator exists here.
        put_i16(&mut out, errors::INVALID_REQUEST);
        if version >= 1 {
            put_nullable_string(&mut out, Some("only group coordination is supported"));
        }
        put_i32(&mut out, -1);
        put_string(&mut out, "");
        put_i32(&mut out, -1);
    }
    Ok(out)
}

/// Heartbeat v0-v4: refreshes the session deadline (see
/// `state::heartbeat` for the error lattice).
pub async fn handle_heartbeat(body: &mut Reader<'_>, version: i16, rt: &CoordRuntime) -> Result<Vec<u8>, String> {
    let bad = || "malformed heartbeat request".to_string();
    let (group, generation, member) = if version >= 4 {
        // v4 is the first flexible Heartbeat (compact framing + tags).
        let group = body.compact_string().ok_or_else(bad)?;
        let generation = body.i32().ok_or_else(bad)?;
        let member = body.compact_string().ok_or_else(bad)?;
        let _instance = body.compact_nullable_string().ok_or_else(bad)?;
        body.skip_tagged_fields().ok_or_else(bad)?;
        (group, generation, member)
    } else {
        let group = body.string().ok_or_else(bad)?;
        let generation = body.i32().ok_or_else(bad)?;
        let member = body.string().ok_or_else(bad)?;
        (group, generation, member)
    };
    let now = session::now_ms();
    let code = session::apply(rt, &group, |st| state::heartbeat(st, &member, generation, now))
        .unwrap_or(errors::UNKNOWN_MEMBER_ID); // no such group
    let mut out = Vec::new();
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    put_i16(&mut out, code);
    if version >= 4 {
        crate::kafka::frame::put_empty_tagged_fields(&mut out);
    }
    Ok(out)
}

/// LeaveGroup v0-v2 (single member): removal + rebalance/empty per
/// `state::leave`. An unknown group or member answers
/// UNKNOWN_MEMBER_ID (the v0/v1 broker behavior; v2 reports the same
/// code per member).
pub fn handle_leave_group(body: &mut Reader<'_>, version: i16, rt: &CoordRuntime) -> Result<Vec<u8>, String> {
    let bad = || "malformed leavegroup request".to_string();
    let group = body.string().ok_or_else(bad)?;
    let member = body.string().ok_or_else(bad)?;
    let code = session::apply(rt, &group, |st| state::leave(st, &[member.clone()], session::now_ms()))
        .map(|codes| codes[0])
        .unwrap_or(errors::UNKNOWN_MEMBER_ID);
    let mut out = Vec::new();
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms (v1+ responses lead with it)
    }
    put_i16(&mut out, code);
    Ok(out)
}

/// DescribeGroups v0-v3: a read-only snapshot per requested group.
pub fn handle_describe_groups(body: &mut Reader<'_>, version: i16, rt: &CoordRuntime) -> Result<Vec<u8>, String> {
    let bad = || "malformed describegroups request".to_string();
    let count = body.array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        names.push(body.string().ok_or_else(bad)?);
    }
    // v3 request tail: include_authorized_operations bool (this front
    // has no ACL model; the response still carries the field).
    if version >= 3 {
        body.boolean().ok_or_else(bad)?;
    }
    let groups = rt
        .groups
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out = Vec::new();
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms (v1+ leads with it)
    }
    put_array_len(&mut out, count);
    for name in &names {
        match groups.get(name) {
            None => {
                // Unknown group: "Dead" + NONE (the broker reports the
                // state, not an error).
                put_i16(&mut out, errors::NONE);
                put_string(&mut out, name);
                put_string(&mut out, "Dead");
                put_string(&mut out, "");
                put_nullable_string(&mut out, None);
                put_array_len(&mut out, 0);
                if version >= 3 {
                    put_i32(&mut out, i32::MIN); // authorized_operations unset
                }
            }
            Some(st) => {
                put_i16(&mut out, errors::NONE);
                put_string(&mut out, &name);
                put_string(&mut out, st.stage.name());
                put_string(&mut out, &st.protocol_type);
                put_nullable_string(&mut out, st.protocol_name.as_deref());
                put_array_len(&mut out, st.members.len());
                for id in &st.order {
                    let m = &st.members[id];
                    put_string(&mut out, id);
                    if version >= 4 {
                        // group_instance_id joins member rows at v4
                        // (above this front's v3 cap).
                        put_nullable_string(&mut out, m.instance_id.as_deref());
                    }
                    put_string(&mut out, &m.client_id);
                    put_string(&mut out, &m.client_host);
                    put_bytes(&mut out, &m.metadata);
                    put_bytes(&mut out, &m.assignment);
                }
                if version >= 3 {
                    put_i32(&mut out, i32::MIN); // authorized_operations unset
                }
            }
        }
    }
    Ok(out)
}
