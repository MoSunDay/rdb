//! The two P0 handshake APIs: ApiVersions (18) and Metadata (3).
//!
//! Version policy (the registry lives in [`super`]):
//! - ApiVersions v0-v3; v3 is flexible. An out-of-range version gets a
//!   v0-body UNSUPPORTED_VERSION reply carrying the full supported list
//!   (the documented client fallback parse).
//! - Metadata v0-v8; v8 is the advertised max so the flexible v9+ (UUID
//!   topic ids) never appears. v8 adds the authorized-ops bitmasks; both
//!   are emitted as the "unset" sentinel (this front has no ACL model).

use crate::kafka::catalog;
use crate::kafka::errors;
use crate::kafka::frame::{
    put_array_len, put_bool, put_i16, put_i32, put_nullable_string, put_string, Reader,
};
use crate::state::Shared;

/// Single-broker identity (advertised in Metadata; also the leader /
/// only replica of every partition).
pub const NODE_ID: i32 = 1;
/// "Authorized operations unknown" per the protocol default.
const AUTHORIZED_OPS_UNSET: i32 = i32::MIN;

/// Broker advertisement derived from the `kafka_bind` address: the bind
/// host/port, wildcard hosts rewritten to `localhost`. The explicit
/// `kafka_advertised_host`/`kafka_advertised_port` overrides (empty/0
/// = unset) win over the derived values -- binding 0.0.0.0 while
/// advertising a reachable address is the production pattern.
pub fn advertise(bind: &str, host_override: &str, port_override: i32) -> (String, i32) {
    let (host, port) = match bind.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => ("localhost", "9092"),
    };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    let host = if !host_override.is_empty() {
        host_override
    } else if host.is_empty() || host == "0.0.0.0" || host == "::" {
        "localhost"
    } else {
        host
    };
    let port = if port_override != 0 {
        port_override
    } else {
        port.parse().unwrap_or(9092)
    };
    (host.to_string(), port)
}

/// ApiVersions response body (0..=3). Entries come from the registry so
/// the advertised set can never drift from dispatch.
pub fn api_versions_body(version: i16, error: i16) -> Vec<u8> {
    let mut out = Vec::new();
    put_i16(&mut out, error);
    let ranges = super::implemented_apis();
    if version >= 3 {
        // v3 layout (Kafka protocol page): error_code, [api_versions]
        // compact array, throttle_time_ms, body TAG_BUFFER -- the
        // throttle lands AFTER the array in every ApiVersions version
        // (mis-ordering aborts librdkafka's ApiVersion parser).
        crate::kafka::frame::put_compact_array_len(&mut out, ranges.len());
        for (key, lo, hi) in ranges {
            put_i16(&mut out, key);
            put_i16(&mut out, lo);
            put_i16(&mut out, hi);
            crate::kafka::frame::put_empty_tagged_fields(&mut out);
        }
        put_i32(&mut out, 0); // throttle_time_ms
        crate::kafka::frame::put_empty_tagged_fields(&mut out);
    } else {
        put_array_len(&mut out, ranges.len());
        for (key, lo, hi) in ranges {
            put_i16(&mut out, key);
            put_i16(&mut out, lo);
            put_i16(&mut out, hi);
        }
        if version >= 1 {
            put_i32(&mut out, 0); // throttle_time_ms (after the array on v1/v2)
        }
    }
    out
}

/// Metadata request topic list: null array = all topics.
#[derive(Debug)]
pub enum TopicRequest {
    All,
    Names(Vec<Option<String>>),
}

/// Parse a Metadata request body for v0-v8 (classic framing).
pub fn parse_metadata_req(body: &mut Reader<'_>, version: i16) -> Option<TopicRequest> {
    let topics = match body.array_len()? {
        None => TopicRequest::All,
        Some(n) => {
            let mut names = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                names.push(body.nullable_string()?);
            }
            TopicRequest::Names(names)
        }
    };
    if version >= 4 {
        body.boolean()?; // allow_auto_topic_creation
    }
    if version >= 8 {
        body.boolean()?; // include_cluster_authorized_operations (v8-10)
        body.boolean()?; // include_topic_authorized_operations (v8+)
    }
    Some(topics)
}

/// One topic entry of the Metadata response.
pub struct TopicMeta {
    pub name: String,
    pub error: i16,
    pub partitions: Vec<i32>,
}

/// Handle a Metadata request: resolve topics against the Lite catalog,
/// encode the v0-v8 body.
pub fn handle_metadata(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
    ad: &(String, i32),
) -> Result<Vec<u8>, String> {
    let req = parse_metadata_req(body, version).ok_or("malformed metadata request")?;
    let names: Vec<Option<String>> = match req {
        TopicRequest::All => catalog::list_topics(&shared.store)?
            .into_iter()
            .map(|t| Some(String::from_utf8_lossy(&t).into_owned()))
            .collect(),
        TopicRequest::Names(n) => n,
    };
    let mut topics = Vec::with_capacity(names.len());
    for name in names {
        let (name, ok) = match name {
            Some(n) if !n.is_empty() => (n, true),
            _ => (String::new(), false), // null/empty name: unknown topic
        };
        let partitions = if ok {
            catalog::partitions_of(&shared.store, name.as_bytes())?
        } else {
            Vec::new()
        };
        let error = if partitions.is_empty() {
            errors::UNKNOWN_TOPIC_OR_PARTITION
        } else {
            errors::NONE
        };
        topics.push(TopicMeta {
            name,
            error,
            partitions,
        });
    }
    Ok(metadata_body(version, topics, ad))
}

/// Metadata response body, v0-v8.
pub fn metadata_body(version: i16, topics: Vec<TopicMeta>, ad: &(String, i32)) -> Vec<u8> {
    let mut out = Vec::new();
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    // brokers: the single node.
    put_array_len(&mut out, 1);
    put_i32(&mut out, NODE_ID);
    put_string(&mut out, &ad.0);
    put_i32(&mut out, ad.1);
    if version >= 1 {
        put_nullable_string(&mut out, None); // rack
    }
    if version >= 3 {
        put_nullable_string(&mut out, Some("rdb-lite")); // cluster_id
    }
    put_i32(&mut out, NODE_ID); // controller_id
                                // topics
    put_array_len(&mut out, topics.len());
    for t in &topics {
        put_i16(&mut out, t.error);
        put_string(&mut out, &t.name);
        if version >= 1 {
            put_bool(&mut out, false); // is_internal
        }
        put_array_len(&mut out, t.partitions.len());
        for &p in &t.partitions {
            put_i16(&mut out, errors::NONE);
            put_i32(&mut out, p);
            put_i32(&mut out, NODE_ID); // leader
            if version >= 7 {
                put_i32(&mut out, -1); // leader_epoch: unknown sentinel
            }
            put_array_len(&mut out, 1); // replicas
            put_i32(&mut out, NODE_ID);
            put_array_len(&mut out, 1); // isr
            put_i32(&mut out, NODE_ID);
            if version >= 5 {
                put_array_len(&mut out, 0); // offline_replicas (v5+)
            }
        }
        if version >= 8 {
            put_i32(&mut out, AUTHORIZED_OPS_UNSET); // topic_authorized_ops
        }
    }
    if version >= 8 {
        put_i32(&mut out, AUTHORIZED_OPS_UNSET); // cluster_authorized_ops
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertise_rewrites_wildcards() {
        assert_eq!(
            advertise("0.0.0.0:9092", "", 0),
            ("localhost".to_string(), 9092)
        );
        assert_eq!(
            advertise("10.1.2.3:9099", "", 0),
            ("10.1.2.3".to_string(), 9099)
        );
        assert_eq!(advertise("[::1]:9092", "", 0), ("::1".to_string(), 9092));
        assert_eq!(advertise("bogus", "", 0), ("localhost".to_string(), 9092));
    }

    #[test]
    fn advertise_overrides_win() {
        // The production pattern: wildcard bind + explicit advertisement.
        assert_eq!(
            advertise("0.0.0.0:19092", "broker.example", 9092),
            ("broker.example".to_string(), 9092)
        );
        // Host override alone keeps the bind port.
        assert_eq!(
            advertise("0.0.0.0:19092", "broker.example", 0),
            ("broker.example".to_string(), 19092)
        );
        // Port override alone keeps the (non-wildcard) bind host.
        assert_eq!(
            advertise("10.1.2.3:19092", "", 9092),
            ("10.1.2.3".to_string(), 9092)
        );
    }

    #[test]
    fn api_versions_v0_and_v3_shapes() {
        // The advertised table = implemented_apis() in order (13 rows
        // after P3 added the six coordinator apis).
        let want = super::super::implemented_apis();
        assert_eq!(want.len(), 13);

        let v0 = api_versions_body(0, 0);
        let mut r = Reader::new(&v0);
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.array_len(), Some(Some(want.len())));
        for (key, lo, hi) in &want {
            assert_eq!(
                [r.i16(), r.i16(), r.i16()],
                [Some(*key), Some(*lo), Some(*hi)]
            );
        }
        assert_eq!(r.remaining(), 0, "no throttle on v0");

        let v3 = api_versions_body(3, 0);
        let mut r = Reader::new(&v3);
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.compact_array_len(), Some(Some(want.len())));
        for (key, lo, hi) in &want {
            assert_eq!(
                [r.i16(), r.i16(), r.i16()],
                [Some(*key), Some(*lo), Some(*hi)]
            );
            assert_eq!(r.skip_tagged_fields(), Some(()));
        }
        assert_eq!(r.i32(), Some(0), "throttle AFTER the array on v3");
        assert_eq!(r.skip_tagged_fields(), Some(()), "trailing tagged section");
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn metadata_request_parses_v0_and_v8() {
        let mut v0 = Vec::new();
        put_i32(&mut v0, -1); // null topic array
        assert!(matches!(
            parse_metadata_req(&mut Reader::new(&v0), 0),
            Some(TopicRequest::All)
        ));
        let mut v8 = Vec::new();
        put_i32(&mut v8, 1);
        put_string(&mut v8, "t1");
        put_bool(&mut v8, true); // allow_auto
        put_bool(&mut v8, false); // include_cluster_authorized_ops
        put_bool(&mut v8, false); // include_topic_authorized_ops (v8+)
        match parse_metadata_req(&mut Reader::new(&v8), 8) {
            Some(TopicRequest::Names(n)) => assert_eq!(n, vec![Some("t1".to_string())]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn metadata_v8_body_layout() {
        let body = metadata_body(
            8,
            vec![TopicMeta {
                name: "t1".into(),
                error: 0,
                partitions: vec![0],
            }],
            &("127.0.0.1".to_string(), 9092),
        );
        let mut r = Reader::new(&body);
        assert_eq!(r.i32(), Some(0)); // throttle
        assert_eq!(r.array_len(), Some(Some(1))); // brokers
        assert_eq!(r.i32(), Some(1)); // node_id
        assert_eq!(r.string(), Some("127.0.0.1".into()));
        assert_eq!(r.i32(), Some(9092));
        assert_eq!(r.nullable_string(), Some(None)); // rack (v1+)
        assert_eq!(r.nullable_string(), Some(Some("rdb-lite".into()))); // cluster_id (v3+)
        assert_eq!(r.i32(), Some(1)); // controller
        assert_eq!(r.array_len(), Some(Some(1))); // topics
        assert_eq!(r.i16(), Some(0)); // topic error
        assert_eq!(r.string(), Some("t1".into()));
        assert_eq!(r.boolean(), Some(false)); // is_internal
        assert_eq!(r.array_len(), Some(Some(1))); // partitions
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.i32(), Some(0)); // index
        assert_eq!(r.i32(), Some(1)); // leader
        assert_eq!(r.i32(), Some(-1), "leader_epoch (v7+) unknown sentinel");
        assert_eq!(r.array_len(), Some(Some(1))); // replicas
        assert_eq!(r.i32(), Some(1));
        assert_eq!(r.array_len(), Some(Some(1))); // isr
        assert_eq!(r.i32(), Some(1));
        assert_eq!(r.array_len(), Some(Some(0))); // offline
        assert_eq!(r.i32(), Some(i32::MIN)); // topic_authorized_ops
        assert_eq!(r.i32(), Some(i32::MIN)); // cluster_authorized_ops
        assert_eq!(r.remaining(), 0);
    }
}
