//! RENAME vs the kafka committed-offset ledger (kind 0x20), process
//! level against the REAL binary: the ledger rows travel with the
//! stream family (so the XTRIM/XDEL guard follows them), the vacated
//! old name no longer commits (UNKNOWN_TOPIC_OR_PARTITION -- no
//! dangling row), the new name's committed offset survives a kill -9 +
//! respawn, and a ledger-less stream renames unguarded.

mod common;
mod kafka_front_common;

use common::contains_bytes;
use kafka_front_common::groups::{commit_v2, fetch_offset_v0, join_v1, sync_v1};
use kafka_front_common::{resp_one_shot, spawn_kafka_node, wait_accepting};
use rdb::hash::{hash_tag, slot_with_prefix};
use rdb::kafka::errors;
use tokio::net::TcpStream;

const SESSION_MS: i32 = 30_000;
const GUARD_TEXT: &[u8] = b"consumer-group offsets";

/// Cluster slot of a key exactly like dispatch routes it
/// (`hash::hash_tag` + CRC16).
fn slot_of(key: &[u8]) -> u16 {
    slot_with_prefix(hash_tag(key)).0
}

/// Rename pairs whose parents AND full `parent/child` names all hash to
/// ONE slot. Wire-renameable lite streams need all three coincidences:
/// (1) slot(full src) == slot(src parent): dispatch derives the RENAME
///     prefix from the FULL key name while lite stores the stream under
///     the PARENT-derived prefix (`model::stream_prefix`) -- without it
///     the source resolves as missing and RENAME answers "no such key";
/// (2) slot(full dst) == slot(full src): `require_same_slot`;
/// (3) slot(dst parent) == slot(full src): `move_family` writes the
///     destination under the routing prefix, so post-rename lite/kafka
///     commands (XTRIM guard, OffsetFetch) must hash the new name onto
///     that same prefix.
/// Lite topic names reject `{` (valid_part), so `{tag}` routing cannot
/// provide these. The pairs were brute-forced with the crate's own slot
/// function (parents `t0..t59999` x children `p0..p7`/`q0..q7`, first
/// slot reaching three fixed points) and the whole lattice is
/// re-verified below on every run -- deterministic, no coincidence.
const SRC: &str = "t3107/q5";
const DST: &str = "t43847/q5";
const CONTROL: &str = "t52806/q5"; // ledger-less; renamed onto the vacated SRC

fn parent_of(stream: &str) -> &[u8] {
    stream
        .split_once('/')
        .map(|(p, _)| p.as_bytes())
        .unwrap_or_default()
}

#[test]
fn rename_pairs_hash_to_one_slot() {
    let want = slot_of(SRC.as_bytes());
    for name in [SRC, DST, CONTROL] {
        let full = name.as_bytes();
        assert_eq!(slot_of(full), want, "{name}: full-name slot");
        assert_eq!(slot_of(parent_of(name)), want, "{name}: parent slot");
    }
}

/// Drop the pipelined AUTH `+OK` reply so assertions see only the
/// command's own frame.
fn strip_auth(reply: &[u8]) -> &[u8] {
    reply.strip_prefix(b"+OK\r\n").unwrap_or(reply)
}

async fn xadd_ids(resp: &str, stream: &str, ids: &[&str]) {
    for id in ids {
        let reply = resp_one_shot(
            resp,
            &[b"XADD", stream.as_bytes(), id.as_bytes(), b"f", b"v"],
        )
        .await;
        let want = format!("${}\r\n{id}\r\n", id.len()).into_bytes();
        assert!(
            strip_auth(&reply).ends_with(&want),
            "xadd {stream} {id}: {reply:?}"
        );
    }
}

async fn xtrim_reply(resp: &str, stream: &str, tail: &[&[u8]]) -> Vec<u8> {
    let mut args = vec![b"XTRIM".as_slice(), stream.as_bytes()];
    args.extend_from_slice(tail);
    strip_auth(&resp_one_shot(resp, &args).await).to_vec()
}

#[tokio::test]
async fn rename_moves_ledger_guard_and_offset() {
    rename_pairs_hash_to_one_slot();
    let dir = std::env::temp_dir().join(format!("rdb-kafka-rename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    xadd_ids(&resp, SRC, &["1-1", "2-1", "3-1"]).await;
    xadd_ids(&resp, CONTROL, &["1-1", "2-1"]).await;
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka");

    // One consumer, one group: the KIP-394 handshake -> join -> sync at
    // generation 1, then a durable commit of offset 2 on partition 5
    // (the queue is `q5`; `p5` does not exist, so the mapping takes the
    // fallback name exactly like the P2 offsets e2e).
    let r = join_v1(&mut sock, 1, "g1", SESSION_MS, 60_000, "", b"sub").await;
    assert_eq!(r.error, errors::MEMBER_ID_REQUIRED);
    let member = r.member_id;
    let r = join_v1(&mut sock, 2, "g1", SESSION_MS, 60_000, &member, b"sub").await;
    assert_eq!(r.error, errors::NONE);
    assert_eq!(r.generation, 1);
    let (err, _) = sync_v1(&mut sock, 3, "g1", &member, 1, &[(member.as_str(), b"[a]")]).await;
    assert_eq!(err, errors::NONE);
    let topic_src = SRC.split_once('/').unwrap().0;
    assert_eq!(
        commit_v2(&mut sock, 4, "g1", 1, &member, topic_src, 5, 2).await,
        errors::NONE
    );
    assert_eq!(fetch_offset_v0(&mut sock, 5, "g1", topic_src, 5).await, 2);

    // Guard is live under the OLD name (the ledger row pins the
    // ordinal<->id map; XTRIM/XDEL must not shift it).
    let reply = xtrim_reply(&resp, SRC, &[b"MINID", b"=", b"0-1"]).await;
    assert!(
        contains_bytes(&reply, GUARD_TEXT),
        "guard before rename: {reply:?}"
    );

    // The rename itself: same slot, source found under its parent
    // prefix, destination free -> +OK.
    let raw = resp_one_shot(&resp, &[b"RENAME", SRC.as_bytes(), DST.as_bytes()]).await;
    assert_eq!(strip_auth(&raw), b"+OK\r\n", "rename {SRC} -> {DST}");

    // Ledger rows FOLLOWED the stream family: the guard now fires for
    // the new name and no longer for the vacated old name.
    let reply = xtrim_reply(&resp, DST, &[b"MINID", b"=", b"0-1"]).await;
    assert!(
        contains_bytes(&reply, GUARD_TEXT),
        "guard followed the rename: {reply:?}"
    );
    let reply = xtrim_reply(&resp, SRC, &[b"MAXLEN", b"=", b"1"]).await;
    assert_eq!(reply, b":0\r\n", "old name vacated (no stream, no ledger)");
    let reply = xtrim_reply(&resp, SRC, &[b"MINID", b"=", b"0-1"]).await;
    assert_eq!(reply, b":0\r\n");

    // Old-name commit is REJECTED before any ledger write: `commit_one`
    // resolves the partition -> queue mapping first, and the renamed
    // away topic has no queues left, so the answer is
    // UNKNOWN_TOPIC_OR_PARTITION and no dangling row appears.
    assert_eq!(
        commit_v2(&mut sock, 6, "g1", 1, &member, topic_src, 5, 3).await,
        errors::UNKNOWN_TOPIC_OR_PARTITION,
        "old-name commit fails"
    );
    // The new name serves the SAME committed offset (the row kept its
    // value; the topic name moved with the stream).
    let topic_dst = DST.split_once('/').unwrap().0;
    assert_eq!(fetch_offset_v0(&mut sock, 7, "g1", topic_dst, 5).await, 2);

    // Durability: kill -9 + respawn on the same data dir keeps the
    // moved row (guard still armed at the new name, still absent at the
    // old one) and the committed offset.
    node.kill_now();
    node.respawn();
    wait_accepting(&resp, &mut node, "resp").await;
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("reconnect kafka");
    assert_eq!(fetch_offset_v0(&mut sock, 8, "g1", topic_dst, 5).await, 2);
    let reply = xtrim_reply(&resp, DST, &[b"MINID", b"=", b"0-1"]).await;
    assert!(
        contains_bytes(&reply, GUARD_TEXT),
        "guard durable after restart"
    );
    let reply = xtrim_reply(&resp, SRC, &[b"MINID", b"=", b"0-1"]).await;
    assert_eq!(reply, b":0\r\n", "old name still unguarded after restart");

    // Control: a ledger-less stream renames uneventfully and trims
    // normally under its new name (the SRC slot window was vacated
    // above, so the control reuses it).
    let raw = resp_one_shot(&resp, &[b"RENAME", CONTROL.as_bytes(), SRC.as_bytes()]).await;
    assert_eq!(strip_auth(&raw), b"+OK\r\n", "ledger-less rename");
    let reply = xtrim_reply(&resp, SRC, &[b"MINID", b"=", b"3-1"]).await;
    assert_eq!(reply, b":2\r\n", "no guard, plain trim of both entries");
}
