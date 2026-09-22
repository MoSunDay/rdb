//! P2 of the Kafka wire front against the REAL binary: the
//! committed-offset ledger -- OffsetCommit v2 and OffsetFetch v0/v7
//! (explicit + null topics), generation fencing, and durability
//! across a kill + respawn of the process.

mod common;
mod kafka_front_common;

use kafka_front_common::{kafka_req, kafka_round, resp_one_shot, spawn_kafka_node, wait_accepting};
use rdb::kafka::frame::{put_i32, put_i64, put_array_len, put_nullable_string, put_string, Reader};
use tokio::net::TcpStream;

const API_OFFSET_COMMIT: i16 = 8;
const API_OFFSET_FETCH: i16 = 9;

async fn offset_fetch_round(sock: &mut TcpStream, corr: i32, version: i16, body: &[u8]) -> Vec<u8> {
    // OffsetFetch turns flexible at v6: header + body both compact.
    let payload =
        kafka_round(sock, &kafka_req(API_OFFSET_FETCH, version, corr, version >= 6, body)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    if version >= 6 {
        assert_eq!(r.skip_tagged_fields(), Some(()), "flexible response header");
    }
    let hdr = r.pos();
    payload[hdr..].to_vec()
}

async fn xadd(resp: &str, stream: &str, key: &str, val: &str) {
    resp_one_shot(resp, &[b"XADD", stream.as_bytes(), b"*", key.as_bytes(), val.as_bytes()]).await;
}

/// OffsetCommit v2 request: group, generation, member, retention,
/// topics[topic, partitions[part, offset, metadata]].
fn commit_body(group: &str, gen: i32, member: &str, topic: &str, part: i32, off: i64) -> Vec<u8> {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_i32(&mut b, gen);
    put_string(&mut b, member);
    put_i64(&mut b, -1); // retention_time_ms (ignored)
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, part);
    put_i64(&mut b, off);
    put_nullable_string(&mut b, None);
    b
}

/// (partition, error) of the first topic of an OffsetCommit response.
async fn commit_round(sock: &mut TcpStream, corr: i32, body: &[u8]) -> (i32, i16) {
    let payload = kafka_round(sock, &kafka_req(API_OFFSET_COMMIT, 2, corr, false, body)).await;
    let mut r = Reader::new(&payload);
    r.i32();
    let hdr = r.pos();
    let mut r = Reader::new(&payload[hdr..]);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    (r.i32().unwrap(), r.i16().unwrap())
}

fn offset_fetch_body(version: i16, group: &str, topic: Option<&str>) -> Vec<u8> {
    use rdb::kafka::frame::{
        put_compact_array_len, put_compact_nullable_string, put_compact_string,
    };
    if version >= 6 {
        // Flexible shape: compact strings/arrays, a tag byte per topic
        // struct, require_stable (v7), and the message tag tail.
        let mut b = Vec::new();
        put_compact_string(&mut b, group);
        match topic {
            None => put_compact_nullable_string(&mut b, None), // null array = uvarint 0
            Some(t) => {
                put_compact_array_len(&mut b, 1);
                put_compact_string(&mut b, t);
                put_compact_array_len(&mut b, 1);
                put_i32(&mut b, 0);
                b.push(0); // topic-struct tag section
            }
        }
        if version >= 7 {
            b.push(0); // require_stable: false
        }
        b.push(0); // message tag section
        b
    } else {
        let mut b = Vec::new();
        put_string(&mut b, group);
        match topic {
            None => put_i32(&mut b, -1),
            Some(t) => {
                put_array_len(&mut b, 1);
                put_string(&mut b, t);
                put_array_len(&mut b, 1);
                put_i32(&mut b, 0);
            }
        }
        b.push(0); // require_stable (v7 tail)
        b
    }
}

#[tokio::test]
async fn offset_commit_fetch_and_restart_persistence() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-offsets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    xadd(&resp, "f3/q0", "k1", "v1").await;
    xadd(&resp, "f3/q0", "k2", "v2").await;
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka");

    // v2 commit at offset 2 (fully consumed), then read it back v0/v7.
    let (p, e) = commit_round(&mut sock, 401, &commit_body("g1", 5, "m-1", "f3", 0, 2)).await;
    assert_eq!((p, e), (0, 0));
    let body = offset_fetch_body(0, "g1", Some("f3"));
    let body = offset_fetch_round(&mut sock, 402, 0, &body).await;
    let mut r = Reader::new(&body);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i64(), Some(2), "committed offset as stored");
    assert_eq!(r.nullable_string(), Some(None), "metadata null");
    assert_eq!(r.i16(), Some(0));

    let body = offset_fetch_body(7, "g1", Some("f3"));
    let body = offset_fetch_round(&mut sock, 403, 7, &body).await;
    let mut r = Reader::new(&body);
    assert_eq!(r.i32(), Some(0), "throttle");
    assert_eq!(r.compact_array_len(), Some(Some(1)));
    assert_eq!(r.compact_string().as_deref(), Some("f3"));
    assert_eq!(r.compact_array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i64(), Some(2));
    assert_eq!(r.i32(), Some(-1), "committed_leader_epoch");
    assert_eq!(r.compact_nullable_string(), Some(None));
    assert_eq!(r.i16(), Some(0));
    assert_eq!(r.skip_tagged_fields(), Some(()), "partition tags");
    assert_eq!(r.skip_tagged_fields(), Some(()), "topic tags");
    assert_eq!(r.i16(), Some(0), "top-level error after the array");
    assert_eq!(r.skip_tagged_fields(), Some(()), "message tags");
    assert_eq!(r.remaining(), 0);

    // Stale generation: ILLEGAL_GENERATION, stored row untouched.
    let (p, e) = commit_round(&mut sock, 404, &commit_body("g1", 3, "m-1", "f3", 0, 1)).await;
    assert_eq!((p, e), (0, 22), "ILLEGAL_GENERATION");

    // Null topics (v7): the whole-group walk finds f3/0.
    let body = offset_fetch_body(7, "g1", None);
    let body = offset_fetch_round(&mut sock, 405, 7, &body).await;
    let mut r = Reader::new(&body);
    r.i32(); // throttle
    assert_eq!(r.compact_array_len(), Some(Some(1)));
    assert_eq!(r.compact_string().as_deref(), Some("f3"));
    assert_eq!(r.compact_array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i64(), Some(2));

    // Kill + respawn the SAME data dir: the ledger is durable state.
    node.kill_now();
    node.respawn();
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("reconnect kafka");
    let body = offset_fetch_body(0, "g1", Some("f3"));
    let body = offset_fetch_round(&mut sock, 406, 0, &body).await;
    let mut r = Reader::new(&body);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    r.array_len();
    r.i32();
    assert_eq!(r.i64(), Some(2), "commit survives the restart");
    assert_eq!(r.nullable_string(), Some(None));
    assert_eq!(r.i16(), Some(0));
}
