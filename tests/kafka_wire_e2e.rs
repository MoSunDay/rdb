//! Kafka wire front, process-level e2e: the REAL rdb binary with
//! `kafka_bind` set, a Lite stream created over the RESP port, then raw
//! TCP ApiVersions v0/v3 and Metadata v0/v8 handshakes hand-decoded with
//! the production `kafka::frame::Reader` (dogfooding the parser).

mod common;
mod kafka_front_common;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use kafka_front_common::{
    kafka_port, kafka_req, kafka_round, resp_one_shot, spawn_kafka_node, spawn_kafka_node_bind,
    wait_accepting,
};
use rdb::kafka::frame::Reader;

#[tokio::test]
async fn api_versions_and_metadata_over_the_wire() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;

    // Lite stream t1/q0 over RESP (one message) -- the Kafka topic view.
    let reply = resp_one_shot(
        &node.resp,
        &[b"XADD", b"t1/q0", b"*", b"hello", b"world"],
    )
    .await;
    assert!(
        reply.starts_with(b"+OK\r\n$"),
        "auth+xadd reply: {:?}",
        String::from_utf8_lossy(&reply)
    );
    assert!(
        reply.windows(2).any(|w| w == b"-0"),
        "xadd should reply a generated id: {reply:?}"
    );
    wait_accepting(&kafka, &mut node, "kafka").await;

    let mut sock = TcpStream::connect(&node.kafka).await.expect("connect kafka");

    // ---- ApiVersions v0 (classic): the full P0-P3 registry, key-sorted ----
    let payload = kafka_round(&mut sock, &kafka_req(18, 0, 100, false, b"")).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(100), "correlation id echo");
    assert_eq!(r.i16(), Some(0), "error NONE");
    assert_eq!(r.array_len(), Some(Some(13)));
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(0), Some(0), Some(3)], "Produce v0-v3");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(1), Some(0), Some(10)], "Fetch v0-v10");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(2), Some(0), Some(1)], "ListOffsets v0-v1");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(3), Some(0), Some(8)], "Metadata v0-v8");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(8), Some(0), Some(2)], "OffsetCommit v0-v2");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(9), Some(0), Some(7)], "OffsetFetch v0-v7");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(10), Some(0), Some(1)], "FindCoordinator");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(11), Some(0), Some(4)], "JoinGroup v0-v4");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(12), Some(0), Some(4)], "Heartbeat v0-v4");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(13), Some(0), Some(2)], "LeaveGroup v0-v2");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(14), Some(0), Some(4)], "SyncGroup v0-v4");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(15), Some(0), Some(3)], "DescribeGroups");
    assert_eq!([r.i16(), r.i16(), r.i16()], [Some(18), Some(0), Some(3)], "ApiVersions");
    assert_eq!(r.remaining(), 0, "v0 has no throttle tail");

    // ---- ApiVersions v3 (flexible) ----
    let payload = kafka_round(&mut sock, &kafka_req(18, 3, 101, true, b"")).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(101));
    // ApiVersions pins its response header to v0/classic (Kafka's
    // ApiKeys.responseHeaderVersion: no tagged byte even for flexible).
    assert_eq!(r.i16(), Some(0));
    assert_eq!(r.compact_array_len(), Some(Some(13)));
    for (key, lo, hi) in [
        (0, 0, 3),
        (1, 0, 10),
        (2, 0, 1),
        (3, 0, 8),
        (8, 0, 2),
        (9, 0, 7),
        (10, 0, 1),
        (11, 0, 4),
        (12, 0, 4),
        (13, 0, 2),
        (14, 0, 4),
        (15, 0, 3),
        (18, 0, 3),
    ] {
        assert_eq!(
            [r.i16(), r.i16(), r.i16()],
            [Some(key), Some(lo), Some(hi)],
            "api key {key}"
        );
        assert_eq!(r.skip_tagged_fields(), Some(()));
    }
    // ApiVersions quirk: throttle_time_ms sits AFTER the api_keys array
    // (v0 clients stop reading at the array), then the body tag section.
    assert_eq!(r.i32(), Some(0), "throttle after the array");
    assert_eq!(r.skip_tagged_fields(), Some(()), "body tag section");
    assert_eq!(r.remaining(), 0);

    // ---- ApiVersions with an unsupported version: v0 error fallback ----
    let payload = kafka_round(&mut sock, &kafka_req(18, 9, 102, true, b"")).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(102));
    assert_eq!(r.i16(), Some(35), "UNSUPPORTED_VERSION");
    assert_eq!(r.array_len(), Some(Some(13)), "v0 body still lists apis");
    let apis: Vec<[Option<i16>; 3]> =
        (0..13).map(|_| [r.i16(), r.i16(), r.i16()]).collect();
    // Spot-check the P3 additions made the fallback table too.
    assert!(apis.contains(&[Some(11), Some(0), Some(4)]), "JoinGroup");
    assert!(apis.contains(&[Some(15), Some(0), Some(3)]), "DescribeGroups");
    assert_eq!(r.remaining(), 0);

    // ---- Metadata v0, null topics (list all): sees the Lite stream ----
    let mut v0_body = Vec::new();
    v0_body.extend_from_slice(&(-1i32).to_be_bytes());
    let payload = kafka_round(&mut sock, &kafka_req(3, 0, 200, false, &v0_body)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(200));
    assert_eq!(r.array_len(), Some(Some(1)), "single broker");
    assert_eq!(r.i32(), Some(1), "node_id 1");
    let host = r.string().expect("host");
    let port = r.i32().expect("port");
    assert_eq!((host.as_str(), port), ("127.0.0.1", kafka_port(&node.kafka)));
    assert_eq!(r.i32(), Some(1), "controller id 1");
    assert_eq!(r.array_len(), Some(Some(1)), "one topic");
    assert_eq!(r.i16(), Some(0), "topic error NONE");
    assert_eq!(r.string(), Some("t1".to_string()), "topic = lite parent");
    assert_eq!(r.array_len(), Some(Some(1)), "one partition");
    assert_eq!(r.i16(), Some(0));
    assert_eq!(r.i32(), Some(0), "partition index 0 (q0)");
    assert_eq!(r.i32(), Some(1), "leader = node 1");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(1), "replicas");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(1), "isr");
    assert_eq!(r.remaining(), 0, "v0: no offline/throttle tails");

    // ---- Metadata v8 (max advertised) ----
    let mut v8_body = Vec::new();
    v8_body.extend_from_slice(&(-1i32).to_be_bytes());
    // v8 reads THREE bools: allow_auto, include_cluster_auth_ops,
    // include_topic_auth_ops.
    v8_body.extend_from_slice(&[0, 0, 0]);
    let payload = kafka_round(&mut sock, &kafka_req(3, 8, 201, false, &v8_body)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(201));
    assert_eq!(r.i32(), Some(0), "throttle_time_ms");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(1));
    r.string().expect("host");
    r.i32().expect("port");
    assert_eq!(r.nullable_string(), Some(None), "rack null");
    assert_eq!(r.nullable_string(), Some(Some("rdb-lite".into())));
    assert_eq!(r.i32(), Some(1), "controller");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i16(), Some(0));
    assert_eq!(r.string(), Some("t1".into()));
    assert_eq!(r.boolean(), Some(false), "is_internal");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i16(), Some(0));
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i32(), Some(1));
    assert_eq!(r.i32(), Some(-1), "leader_epoch (v7+) unknown sentinel");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(1));
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(1));
    assert_eq!(r.array_len(), Some(Some(0)), "offline replicas empty");
    assert_eq!(r.i32(), Some(i32::MIN), "topic_authorized_ops sentinel");
    assert_eq!(r.i32(), Some(i32::MIN), "cluster_authorized_ops sentinel");
    assert_eq!(r.remaining(), 0);

    // ---- Metadata v0, named unknown topic -> error 3 ----
    let mut named = Vec::new();
    named.extend_from_slice(&1i32.to_be_bytes());
    named.extend_from_slice(&4i16.to_be_bytes());
    named.extend_from_slice(b"nope");
    let payload = kafka_round(&mut sock, &kafka_req(3, 0, 202, false, &named)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(202));
    let _ = r.take(4 + 4 + 2 + host.len() + 4 + 4); // brokers + controller
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i16(), Some(3), "UNKNOWN_TOPIC_OR_PARTITION");
    assert_eq!(r.string(), Some("nope".into()));
    assert_eq!(r.array_len(), Some(Some(0)), "no partitions");
}


/// Spawn + wait ready with retry: parallel tests race `free_addr`'s
/// bind-probe/release TOCTOU (a wildcard 0.0.0.0 bind can steal a
/// released port), so a node that dies at startup is replaced instead
/// of failing the test. Returns the node and its CONNECT address
/// (wildcard binds connect via 127.0.0.1).
async fn spawn_until_ready(
    dir: &std::path::Path,
    bind_override: &str,
    extra_conf: &str,
) -> (kafka_front_common::KafkaNode, String) {
    for attempt in 0..10 {
        let sub = dir.join(format!("try{attempt}"));
        let mut node = spawn_kafka_node_bind(&sub, bind_override, extra_conf);
        let connect = if bind_override.is_empty() {
            node.kafka.clone()
        } else {
            format!("127.0.0.1:{}", bind_override.rsplit_once(':').unwrap().1)
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Ok(Some(_)) = node.child.try_wait() {
                break; // died at startup: retry on fresh ports
            }
            if TcpStream::connect(&connect).await.is_ok() {
                // The readiness probe's server-side slot drains
                // asynchronously (EOF -> guard drop); let it go before
                // connection-cap assertions.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                return (node, connect);
            }
            if std::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // `node` drops here: killed + dir removed, next attempt respawns.
    }
    panic!("kafka node never became ready in 10 attempts");
}

/// A wildcard `kafka_bind` advertises an unusable "localhost" by
/// default (correct for loopback-only dev); the
/// `kafka_advertised_host`/`kafka_advertised_port` overrides replace
/// the Metadata/FindCoordinator brokers row -- the production pattern
/// for binding 0.0.0.0 while handing clients a reachable address.
#[tokio::test]
async fn advertised_overrides_beat_wildcard_bind() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-adv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bind = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("0.0.0.0:{}", probe.local_addr().unwrap().port())
    };
    let (_node, connect) = spawn_until_ready(
        &dir,
        &bind,
        "kafka_advertised_host: \"broker.example\"\nkafka_advertised_port: 9092\n",
    )
    .await;
    let mut sock = TcpStream::connect(&connect).await.expect("connect kafka");

    // Metadata v0: the brokers row carries the overrides, not the bind.
    let mut v0_body = Vec::new();
    v0_body.extend_from_slice(&(-1i32).to_be_bytes());
    let payload = kafka_round(&mut sock, &kafka_req(3, 0, 300, false, &v0_body)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(300));
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(1), "node_id 1");
    assert_eq!(r.string().expect("host"), "broker.example");
    assert_eq!(r.i32(), Some(9092), "advertised port override");

    // FindCoordinator v0 points at the same advertised endpoint.
    let mut fc_body = Vec::new();
    fc_body.extend_from_slice(&1i16.to_be_bytes());
    fc_body.extend_from_slice(b"g1");
    let payload = kafka_round(&mut sock, &kafka_req(10, 0, 301, false, &fc_body)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(301));
    assert_eq!(r.i16(), Some(0), "error NONE");
    assert_eq!(r.i32(), Some(1), "node_id 1");
    assert_eq!(r.string().expect("coordinator host"), "broker.example");
    assert_eq!(r.i32(), Some(9092));
}

/// `kafka_max_connections` caps concurrent connections: sockets beyond
/// the cap are closed by the server on accept, live ones keep working.
#[tokio::test]
async fn connection_cap_closes_excess_sockets() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-cap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (_node, connect) = spawn_until_ready(&dir, "", "kafka_max_connections: 2\n").await;

    // Two live connections both serve requests.
    let mut a = TcpStream::connect(&connect).await.expect("conn a");
    let mut b = TcpStream::connect(&connect).await.expect("conn b");
    for sock in [&mut a, &mut b] {
        let payload = kafka_round(sock, &kafka_req(18, 0, 400, false, b"")).await;
        assert_eq!(&payload[4..6], &0i16.to_be_bytes(), "ApiVersions NONE");
    }

    // The third is admitted by the kernel backlog but closed by the
    // server immediately: reads see EOF, not a hang.
    let mut c = TcpStream::connect(&connect).await.expect("conn c connects");
    let mut probe = [0u8; 8];
    let seen = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        c.read(&mut probe),
    )
    .await
    .expect("server closed within 5s");
    assert_eq!(seen.unwrap_or(0), 0, "EOF, no response bytes");

    // The survivors are unaffected by the rejection.
    let payload = kafka_round(&mut a, &kafka_req(18, 0, 401, false, b"")).await;
    assert_eq!(&payload[4..6], &0i16.to_be_bytes());
}
