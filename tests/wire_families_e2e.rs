//! Process-level wire tests for the JSON and VectorSet command
//! families: every command travels as RESP2 bytes over a raw TcpStream
//! to the REAL spawned binary (single node, empty topology: writes are
//! served locally, no MOVED). Expected replies reuse the exact bytes
//! already pinned by the in-process suites (tests/json_e2e.rs,
//! vectorset_e2e.rs); the point here is the wire path itself --
//! server-side name lowercasing plus reply frame encode/decode.
//! `cmd_one_shot` returns ONE reply (trailing CRLF stripped), so array
//! replies (OBJKEYS / VSIM) go through `cmd_full_reply` and are matched
//! with `contains_bytes` (nested arrays are not line-resolvable).
//! The FT.*/CONFIG wire siblings live in `wire_ft_config_e2e.rs`.

mod common;

use common::lite::cmd_full_reply;
use common::{cmd_one_shot, spawn_node, wait_resp_ready, TOKEN};

/// One fresh lone node per test: own tempdir (kept alive for the node's
/// lifetime), RAFT_BOOTSTRAP, no join -> empty topology, no MOVED.
async fn lone_node() -> (common::ProcNode, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut node = spawn_node(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    (node, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_family_over_the_wire() {
    let (node, _dir) = lone_node().await;
    let a = node.resp.as_str();

    // SET/GET roundtrip: insertion order kept, byte-exact payload.
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[b"JSON.SET", b"jdoc", b".", b"{\"b\":1,\"a\":[true,null]}"]
        )
        .await,
        b"+OK"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.GET", b"jdoc"]).await,
        b"$23\r\n{\"b\":1,\"a\":[true,null]}"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.GET", b"jdoc", b".a[0]"]).await,
        b"$4\r\ntrue"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.GET", b"missing"]).await,
        b"$-1"
    );

    // TYPE at root and leaves.
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.TYPE", b"jdoc"]).await,
        b"+object"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.TYPE", b"jdoc", b".b"]).await,
        b"+integer"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.TYPE", b"jdoc", b".a[1]"]).await,
        b"+null"
    );

    // STRAPPEND / NUMINCRBY pinned by json_e2e.rs.
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[b"JSON.SET", b"k", b".", b"{\"s\":\"ab\",\"n\":1}"]
        )
        .await,
        b"+OK"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.STRAPPEND", b"k", b".s", b"\"cd\""]).await,
        b":4"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.GET", b"k", b".s"]).await,
        b"$6\r\n\"abcd\""
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.NUMINCRBY", b"k", b".n", b"2"]).await,
        b"$1\r\n3"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.NUMINCRBY", b"k", b".n", b"0.5"]).await,
        b"$3\r\n3.5"
    );

    // OBJKEYS is an array reply -> full drain; OBJLEN is an int line.
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[
                b"JSON.SET",
                b"o",
                b".",
                b"{\"o\":{\"b\":2,\"a\":1},\"keep\":9}"
            ]
        )
        .await,
        b"+OK"
    );
    let r = cmd_full_reply(a, TOKEN, &[b"JSON.OBJKEYS", b"o", b".o"], 400).await;
    assert_eq!(r, b"*2\r\n$1\r\nb\r\n$1\r\na\r\n", "objkeys");
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.OBJLEN", b"o", b".o"]).await,
        b":2"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.OBJLEN", b"o", b".o.b"]).await,
        b"-ERR wrong type of path value"
    );

    // Array lifecycle: ARRAPPEND/ARRLEN, then DEL (path + whole key).
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.SET", b"arr", b".", b"[]"]).await,
        b"+OK"
    );
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[b"JSON.ARRAPPEND", b"arr", b".", b"1", b"2", b"3"]
        )
        .await,
        b":3"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.ARRLEN", b"arr"]).await,
        b":3"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.DEL", b"o", b".o.b"]).await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"JSON.DEL", b"o", b".o.b"]).await,
        b":0"
    );
    assert_eq!(cmd_one_shot(a, TOKEN, &[b"JSON.DEL", b"o"]).await, b":1");
    assert_eq!(cmd_one_shot(a, TOKEN, &[b"JSON.GET", b"o"]).await, b"$-1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vectorset_family_over_the_wire() {
    let (node, _dir) = lone_node().await;
    let a = node.resp.as_str();

    // Seed a 2-D set: e1=[1,0] e2=[0,1] e3=[1,1] (vectorset_e2e.rs seed).
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[b"VADD", b"k", b"VALUES", b"2", b"e1", b"1", b"0"]
        )
        .await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[b"VADD", b"k", b"VALUES", b"2", b"e2", b"0", b"1"]
        )
        .await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[b"VADD", b"k", b"VALUES", b"2", b"e3", b"1", b"1"]
        )
        .await,
        b":1"
    );
    assert_eq!(cmd_one_shot(a, TOKEN, &[b"VCARD", b"k"]).await, b":3");
    assert_eq!(cmd_one_shot(a, TOKEN, &[b"VDIM", b"k"]).await, b":2");

    // Attributes: nil by default, set/clear roundtrip.
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"VGETATTR", b"k", b"e1"]).await,
        b"$-1"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"VSETATTR", b"k", b"e1", b"t=1"]).await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"VGETATTR", b"k", b"e1"]).await,
        b"$3\r\nt=1"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"VSETATTR", b"k", b"e1", b""]).await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"VGETATTR", b"k", b"e1"]).await,
        b"$-1"
    );

    // VSIM: array reply, exact deterministic ranking for [0,1].
    let r = cmd_full_reply(a, TOKEN, &[b"VSIM", b"k", b"VALUES", b"0", b"1"], 400).await;
    assert_eq!(
        r, b"*3\r\n$2\r\ne2\r\n$2\r\ne3\r\n$2\r\ne1\r\n",
        "vsim [0,1]"
    );

    // Re-adding an existing element replaces the vector (returns 0).
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[b"VADD", b"k", b"VALUES", b"2", b"e1", b"0", b"1"]
        )
        .await,
        b":0"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"VCARD", b"k"]).await,
        b":3",
        "replace keeps cardinality"
    );
    let r = cmd_full_reply(
        a,
        TOKEN,
        &[
            b"VSIM",
            b"k",
            b"COUNT",
            b"2",
            b"WITHSCORES",
            b"VALUES",
            b"0",
            b"1",
        ],
        400,
    )
    .await;
    assert_eq!(
        r, b"*4\r\n$2\r\ne1\r\n$1\r\n1\r\n$2\r\ne2\r\n$1\r\n1\r\n",
        "vsim scores"
    );

    // VREM shrinks the set; a second remove is a no-op.
    assert_eq!(cmd_one_shot(a, TOKEN, &[b"VREM", b"k", b"e3"]).await, b":1");
    assert_eq!(cmd_one_shot(a, TOKEN, &[b"VCARD", b"k"]).await, b":2");
    assert_eq!(cmd_one_shot(a, TOKEN, &[b"VREM", b"k", b"e3"]).await, b":0");
}
