//! Multi-store scenario #97 -- tenant token gate on the kafka front.
//! A node started with `kafka_token` demands SASL PLAIN before any
//! data command: pre-auth ApiVersions still answers (advertising the
//! SASL pair 17/36), an unauthenticated Produce is dropped without a
//! reply, a wrong mechanism answers 33 + close, a wrong token answers
//! 58 (message carries no token fragment) + close, and the real token
//! unlocks the full surface on one conn: Produce v3 lands (RESP seed
//! and produced record both count) and OffsetCommit v0/OffsetFetch
//! v0 round-trip the committed offset.

mod common;
mod kafka_front_common;
mod scenario_common;

use kafka_front_common::groups::{
    commit_v0, expect_closed, fetch_offset_v0, sasl_auth, sasl_handshake,
};
use kafka_front_common::{kafka_req, kafka_round};
use rdb::kafka::errors;
use rdb::kafka::frame::{put_array_len, put_i16, put_i32, put_string, Reader};
use rdb::kafka::record::{build_batch, BatchRecord};
use scenario_common::{front_world, FrontSpec};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

const TOKEN: &str = "tenant-gw-token-97";

/// Produce v3 (null transactional_id) over an EXISTING socket, i.e. a
/// connection that has already authenticated: the partition error code.
async fn produce_v3(sock: &mut TcpStream, corr: i32, topic: &str, value: &[u8]) -> i16 {
    let batch = build_batch(
        0,
        1_000,
        &[BatchRecord {
            timestamp_delta: 0,
            key: None,
            value: Some(value),
            headers: vec![],
        }],
    );
    let mut b = Vec::new();
    put_i16(&mut b, -1); // transactional_id: null
    put_i16(&mut b, 1); // acks
    put_i32(&mut b, 5_000);
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, 0);
    put_i32(&mut b, batch.len() as i32);
    b.extend_from_slice(&batch);
    let payload = kafka_round(sock, &kafka_req(0, 3, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0), "partition");
    let code = r.i16().unwrap();
    assert_eq!(r.i64().unwrap(), 1, "base_offset: seed occupies 0");
    code
}

/// Send one framed request without reading a reply.
async fn send_only(sock: &mut TcpStream, req: &[u8]) {
    let mut framed = (req.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(req);
    sock.write_all(&framed).await.expect("write req");
}

#[tokio::test]
async fn tenant_gateway_sasl_token_gates_the_kafka_front() {
    let (front, _world) = front_world(
        "sasl97",
        &FrontSpec {
            kafka: true,
            extra_yaml: format!("kafka_token: \"{TOKEN}\"\n"),
            ..Default::default()
        },
    )
    .await;
    let kafka = front.kafka.clone();

    // (1) Pre-auth ApiVersions answers WITH the SASL pair advertised.
    let mut a = TcpStream::connect(&kafka).await.expect("connect a");
    let payload = kafka_round(&mut a, &kafka_req(18, 0, 1, false, b"")).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(1));
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.i16(), Some(0));
    let n = r.array_len().unwrap().unwrap_or(0);
    let mut keys = Vec::new();
    for _ in 0..n {
        keys.push(r.i16().unwrap());
        r.i16();
        r.i16();
    }
    assert_eq!(keys.len(), 22, "20 + the SASL pair");
    assert!(keys.contains(&17) && keys.contains(&36), "{keys:?}");

    // (2) Unauthenticated Produce v3 is dropped without any reply.
    let mut b = Vec::new();
    put_i16(&mut b, -1);
    put_i16(&mut b, 1);
    put_i32(&mut b, 5_000);
    put_array_len(&mut b, 1);
    put_string(&mut b, "sasl");
    put_array_len(&mut b, 1);
    put_i32(&mut b, 0);
    put_i32(&mut b, 0); // empty batch body: request still dropped
    send_only(&mut a, &kafka_req(0, 3, 2, false, &b)).await;
    expect_closed(&mut a, "unauthenticated produce").await;

    // (3) Wrong mechanism: 33, then close.
    let mut c = TcpStream::connect(&kafka).await.expect("connect c");
    assert_eq!(
        sasl_handshake(&mut c, 1, "SCRAM-SHA-512").await,
        errors::UNSUPPORTED_SASL_MECHANISM
    );
    expect_closed(&mut c, "unsupported mechanism").await;

    // (4) Wrong token: 58 + fixed message (no token fragments), close.
    let mut d = TcpStream::connect(&kafka).await.expect("connect d");
    assert_eq!(sasl_handshake(&mut d, 1, "PLAIN").await, errors::NONE);
    let (code, msg) = sasl_auth(&mut d, 2, "not-the-token").await;
    assert_eq!(code, errors::SASL_AUTHENTICATION_FAILED);
    assert!(!msg.unwrap().contains(TOKEN), "no token fragments");
    expect_closed(&mut d, "wrong token").await;

    // (5) The real token unlocks produce + offset ledger on one conn.
    let mut e = TcpStream::connect(&kafka).await.expect("connect e");
    assert_eq!(sasl_handshake(&mut e, 1, "PLAIN").await, errors::NONE);
    let (code, msg) = sasl_auth(&mut e, 2, TOKEN).await;
    assert_eq!(code, errors::NONE, "{msg:?}");
    let seeded = scenario_common::r(
        &front.node.resp,
        &[b"XADD", b"sasl/q0", b"*", b"gw", b"seed"],
    )
    .await;
    assert!(!seeded.starts_with(b"-"), "RESP seed: {seeded:?}");
    assert_eq!(
        produce_v3(&mut e, 3, "sasl", b"tenant-event-97").await,
        errors::NONE,
        "produce works post-auth"
    );
    assert_eq!(commit_v0(&mut e, 4, "g97", "sasl", 2).await, errors::NONE);
    assert_eq!(
        fetch_offset_v0(&mut e, 5, "g97", "sasl", 0).await,
        2,
        "seed + produced = committed hwm"
    );
}
