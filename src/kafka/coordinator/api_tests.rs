//! Wire roundtrips for every advertised version of FindCoordinator
//! (v0-v1), Heartbeat (v0-v4), LeaveGroup (v0-v2) and DescribeGroups
//! (v0-v3): each test hand-encodes the request body the way a client
//! would, runs the handler, and decodes the response byte-for-byte
//! (self-consistent anchors for the schema notes in `api.rs`).

use super::api;
use super::state;
use super::CoordRuntime;
use crate::kafka::errors;
use crate::kafka::frame::{
    put_array_len, put_bool as pbool, put_i32 as p32, put_nullable_string, put_string, Reader,
};

const AD: (&str, i32) = ("broker1", 9092);

fn find_req(version: i16, key: &str, ctype: Option<i8>) -> Vec<u8> {
    let mut b = Vec::new();
    put_string(&mut b, key);
    if version >= 1 {
        b.push(ctype.unwrap_or(0) as u8);
    }
    b
}

fn round_finding(body: &[u8], version: i16) -> (i16, Option<String>, i32, String, i32) {
    let mut r = Reader::new(body);
    if version >= 1 {
        assert_eq!(r.i32(), Some(0), "v1+ leads with throttle_time_ms");
    }
    let err = r.i16().unwrap();
    let msg = if version >= 1 { r.nullable_string().unwrap() } else { None };
    let node = r.i32().unwrap();
    let host = r.string().unwrap();
    let port = r.i32().unwrap();
    assert_eq!(r.remaining(), 0, "no trailing bytes");
    (err, msg, node, host, port)
}

#[test]
fn find_coordinator_roundtrips() {
    for version in 0..=1 {
        let req = find_req(version, "g1", None);
        let mut r = Reader::new(&req);
        let body = api::handle_find_coordinator(&mut r, version, &(AD.0.into(), AD.1)).unwrap();
        let (err, msg, node, host, port) = round_finding(&body, version);
        assert_eq!(err, errors::NONE);
        assert_eq!(msg, None);
        assert_eq!(node, 1, "this broker is the only coordinator");
        assert_eq!(host, "broker1");
        assert_eq!(port, 9092);
    }
    // v1 with a non-group coordinator type: INVALID_REQUEST + null node.
    let req = find_req(1, "tx-1", Some(1));
        let mut r = Reader::new(&req);
    let body = api::handle_find_coordinator(&mut r, 1, &(AD.0.into(), AD.1)).unwrap();
    let (err, msg, node, host, port) = round_finding(&body, 1);
    assert_eq!(err, errors::INVALID_REQUEST);
    assert!(msg.is_some(), "v1 carries an error message");
    assert_eq!((node, port), (-1, -1));
    assert_eq!(host, "");
    // v0 has no type field: any key is a group key.
    let req = find_req(0, "anything", None);
        let mut r = Reader::new(&req);
    let body = api::handle_find_coordinator(&mut r, 0, &(AD.0.into(), AD.1)).unwrap();
    assert_eq!(round_finding(&body, 0).0, errors::NONE);
}

/// One enrolled member (generation 1, synced) so Heartbeat/LeaveGroup/
/// DescribeGroups have a Stable group to talk to.
fn group_with_member(rt: &CoordRuntime, member: &str) {
    let a = state::JoinArgs {
        member_id: member,
        instance_id: None,
        client_id: "cid",
        client_host: "10.0.0.9",
        session_timeout_ms: 100_000,
        rebalance_timeout_ms: 400_000,
        protocol_type: "consumer",
        protocol_name: "range",
        metadata: b"sub",
        now_ms: 0,
    };
    let (st, _, _) = state::join(state::new_group("consumer"), &a);
    let (st, _, _) = state::sync(st, member, 1, &[], 0);
    rt.groups.write().unwrap().insert("g1".into(), st);
}

fn hb_req(version: i16, group: &str, gen: i32, member: &str) -> Vec<u8> {
    let mut b = Vec::new();
    if version >= 4 {
        crate::kafka::frame::put_compact_string(&mut b, group);
        p32(&mut b, gen);
        crate::kafka::frame::put_compact_string(&mut b, member);
        crate::kafka::frame::put_compact_nullable_string(&mut b, None); // instance id
        crate::kafka::frame::put_empty_tagged_fields(&mut b);
    } else if version >= 3 {
        put_string(&mut b, group);
        p32(&mut b, gen);
        put_string(&mut b, member);
        put_nullable_string(&mut b, None); // instance id (v3+, classic framing)
    } else {
        put_string(&mut b, group);
        p32(&mut b, gen);
        put_string(&mut b, member);
    }
    b
}

#[tokio::test]
async fn heartbeat_roundtrips() {
    for version in 0..=4 {
        let rt = CoordRuntime::new();
        group_with_member(&rt, "m1");
        let req = hb_req(version, "g1", 1, "m1");
        let mut r = Reader::new(&req);
        let body = api::handle_heartbeat(&mut r, version, &rt).await.unwrap();
        let mut r = Reader::new(&body);
        if version >= 1 {
            assert_eq!(r.i32(), Some(0), "throttle first (v1+)");
        }
        assert_eq!(r.i16(), Some(errors::NONE));
        if version >= 4 {
            assert_eq!(r.remaining(), 1, "flexible tagged tail");
            assert_eq!(body.last(), Some(&0));
        } else {
            assert_eq!(r.remaining(), 0);
        }
    }
    // Error lattice: unknown group/member 25, stale generation 22.
    let rt = CoordRuntime::new();
    group_with_member(&rt, "m1");
    for (gen, member, want) in [(1, "ghost", 25i16), (0, "m1", 22), (1, "m1", 0)] {
        let req = hb_req(0, "g1", gen, member);
        let mut r = Reader::new(&req);
        let body = api::handle_heartbeat(&mut r, 0, &rt).await.unwrap();
        let mut r = Reader::new(&body);
        assert_eq!(r.i16(), Some(want));
    }
    let req = hb_req(0, "nope", 1, "m1");
        let mut r = Reader::new(&req);
    let body = api::handle_heartbeat(&mut r, 0, &rt).await.unwrap();
    assert_eq!(Reader::new(&body).i16(), Some(errors::UNKNOWN_MEMBER_ID));
}

#[test]
fn leave_group_roundtrips() {
    for version in 0..=2 {
        let rt = CoordRuntime::new();
        group_with_member(&rt, "m1");
        let mut b = Vec::new();
        put_string(&mut b, "g1");
        put_string(&mut b, "m1");
        let mut r = Reader::new(&b);
        let body = api::handle_leave_group(&mut r, version, &rt).unwrap();
        let mut r = Reader::new(&body);
        if version >= 1 {
            assert_eq!(r.i32(), Some(0), "v1+ throttle first");
        }
        assert_eq!(r.i16(), Some(errors::NONE));
        assert_eq!(r.remaining(), 0);
        assert!(rt.groups.read().unwrap()["g1"].members.is_empty(), "left -> Empty");
    }
    // Unknown group/member: UNKNOWN_MEMBER_ID on every version.
    let rt = CoordRuntime::new();
    let mut b = Vec::new();
    put_string(&mut b, "nope");
    put_string(&mut b, "m1");
    let mut r = Reader::new(&b);
    let body = api::handle_leave_group(&mut r, 2, &rt).unwrap();
    let mut r = Reader::new(&body);
    r.i32();
    assert_eq!(r.i16(), Some(errors::UNKNOWN_MEMBER_ID));
}

#[test]
fn describe_groups_roundtrips() {
    // Two entries: one live group (2 members, one with instance id at
    // v3) and one unknown group ("Dead" + NONE).
    let rt = CoordRuntime::new();
    group_with_member(&rt, "m1");
    for version in 0..=3 {
        let mut b = Vec::new();
        put_array_len(&mut b, 2);
        put_string(&mut b, "g1");
        put_string(&mut b, "ghost-group");
        if version >= 3 {
            pbool(&mut b, false); // include_authorized_operations
        }
        let mut r = Reader::new(&b);
        let body = api::handle_describe_groups(&mut r, version, &rt).unwrap();
        let mut r = Reader::new(&body);
        if version >= 1 {
            assert_eq!(r.i32(), Some(0), "v1+ throttle first");
        }
        assert_eq!(r.array_len(), Some(Some(2)));
        // Live group row.
        assert_eq!(r.i16(), Some(errors::NONE));
        assert_eq!(r.string().as_deref(), Some("g1"));
        assert_eq!(r.string().as_deref(), Some("Stable"));
        assert_eq!(r.string().as_deref(), Some("consumer"), "protocol_type");
        assert_eq!(r.nullable_string(), Some(Some("range".to_string())));
        assert_eq!(r.array_len(), Some(Some(1)), "members follow protocol");
        assert_eq!(r.string().as_deref(), Some("m1"));
        // member-row group_instance_id joins at v4 (above the v3 cap)
        assert_eq!(r.string().as_deref(), Some("cid"));
        assert_eq!(r.string().as_deref(), Some("10.0.0.9"));
        assert_eq!(r.bytes().unwrap(), Some(b"sub".as_slice()));
        assert_eq!(r.bytes().unwrap(), Some(b"".as_slice()), "assignment unset");
        if version >= 3 {
            assert_eq!(r.i32(), Some(i32::MIN), "v3 authorized_operations");
        }
        // Unknown group row: Dead + NONE.
        assert_eq!(r.i16(), Some(errors::NONE));
        assert_eq!(r.string().as_deref(), Some("ghost-group"));
        assert_eq!(r.string().as_deref(), Some("Dead"));
        assert_eq!(r.string().as_deref(), Some(""));
        assert_eq!(r.nullable_string(), Some(None));
        assert_eq!(r.array_len(), Some(Some(0)));
        if version >= 3 {
            assert_eq!(r.i32(), Some(i32::MIN), "v3 authorized_operations");
        }
        assert_eq!(r.remaining(), 0);
    }
}
