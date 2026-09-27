//! Process-level wire tests for the FT.* search family and CONFIG:
//! every command travels as RESP2 bytes over a raw TcpStream to the
//! REAL spawned binary (single node, empty topology: writes are served
//! locally, no MOVED). Expected replies reuse the exact bytes pinned by
//! tests/search_e2e.rs; the point here is the wire path itself --
//! server-side name lowercasing plus reply frame encode/decode.
//! `cmd_one_shot` returns ONE reply (trailing CRLF stripped), so nested
//! array replies (FT.SEARCH with content, FT.INFO, CONFIG GET) go
//! through `cmd_full_reply` and are matched with `contains_bytes`.
//! The JSON/VectorSet wire siblings live in `wire_families_e2e.rs`.

mod common;

use common::lite::cmd_full_reply;
use common::{cmd_one_shot, contains_bytes, spawn_node, wait_resp_ready, TOKEN};

/// One fresh lone node per test: own tempdir (kept alive for the node's
/// lifetime), RAFT_BOOTSTRAP, no join -> empty topology, no MOVED.
async fn lone_node() -> (common::ProcNode, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut node = spawn_node(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    (node, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft_family_and_config_over_the_wire() {
    let (node, _dir) = lone_node().await;
    let a = node.resp.as_str();

    // Text index: create + two docs, BM25 ranking with content.
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[
                b"FT.CREATE",
                b"idx",
                b"SCHEMA",
                b"title",
                b"TEXT",
                b"body",
                b"TEXT"
            ]
        )
        .await,
        b"+OK"
    );
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[
                b"FT.ADD",
                b"idx",
                b"d1",
                b"{\"title\":\"redis quickstart\",\"body\":\"hello redis world\"}"
            ]
        )
        .await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[
                b"FT.ADD",
                b"idx",
                b"d2",
                b"{\"title\":\"full text\",\"body\":\"hello hello hello redis\"}"
            ]
        )
        .await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"FT.ADD", b"nope", b"x", b"{}"]).await,
        b"-ERR unknown index"
    );
    let r = cmd_full_reply(a, TOKEN, &[b"FT.SEARCH", b"idx", b"@body:hello"], 400).await;
    assert!(
        contains_bytes(&r, b"*5\r\n:2\r\n$2\r\nd2\r\n"),
        "rank1 d2: {r:?}"
    );
    assert!(contains_bytes(&r, b"$2\r\nd1\r\n"), "rank2 d1: {r:?}");
    let r = cmd_full_reply(a, TOKEN, &[b"FT.INFO", b"idx"], 400).await;
    assert!(contains_bytes(&r, b"num_docs"), "info: {r:?}");
    assert!(contains_bytes(&r, b":2\r\n"), "num_docs=2: {r:?}");

    // Vector index: brute-force pre-build, FT.BUILD, then probed search.
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[
                b"FT.CREATE",
                b"vidx",
                b"SCHEMA",
                b"t",
                b"TEXT",
                b"v",
                b"VECTOR",
                b"DIM",
                b"4"
            ]
        )
        .await,
        b"+OK"
    );
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[
                b"FT.ADD",
                b"vidx",
                b"a0",
                b"{\"t\":\"blue\",\"v\":[0.0,0.0,0.0,0.0]}"
            ]
        )
        .await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[
                b"FT.ADD",
                b"vidx",
                b"a1",
                b"{\"t\":\"blue\",\"v\":[0.1,0.0,0.0,0.0]}"
            ]
        )
        .await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[
                b"FT.ADD",
                b"vidx",
                b"a2",
                b"{\"t\":\"red\",\"v\":[0.0,0.2,0.0,0.0]}"
            ]
        )
        .await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(
            a,
            TOKEN,
            &[
                b"FT.ADD",
                b"vidx",
                b"a3",
                b"{\"t\":\"red\",\"v\":[0.0,0.0,0.3,0.0]}"
            ]
        )
        .await,
        b":1"
    );
    // Before FT.BUILD: exact scan; a0/a1 tie-break by docid.
    let r = cmd_full_reply(
        a,
        TOKEN,
        &[
            b"FT.SEARCH",
            b"vidx",
            b"*",
            b"KNN",
            b"2",
            b"v",
            b"VALUES",
            b"0.05",
            b"0",
            b"0",
            b"0",
        ],
        400,
    )
    .await;
    assert!(
        contains_bytes(&r, b":2\r\n$2\r\na0\r\n"),
        "knn pre-build: {r:?}"
    );
    assert!(
        contains_bytes(&r, b"$2\r\na1\r\n"),
        "knn pre-build a1: {r:?}"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"FT.BUILD", b"vidx", b"K", b"2", b"SEED", b"7"]).await,
        b"+OK"
    );
    let r = cmd_full_reply(a, TOKEN, &[b"FT.INFO", b"vidx"], 400).await;
    assert!(contains_bytes(&r, b"ann_built"), "info: {r:?}");
    assert!(
        contains_bytes(&r, b"ann_centroids\r\n:2\r\n"),
        "centroids: {r:?}"
    );
    let r = cmd_full_reply(
        a,
        TOKEN,
        &[
            b"FT.SEARCH",
            b"vidx",
            b"*",
            b"NOCONTENT",
            b"NPROBE",
            b"2",
            b"KNN",
            b"3",
            b"v",
            b"VALUES",
            b"0.05",
            b"0.1",
            b"0",
            b"0",
        ],
        400,
    )
    .await;
    assert!(contains_bytes(&r, b":3\r\n"), "knn post-build: {r:?}");
    for d in [b"$2\r\na0\r\n", b"$2\r\na1\r\n", b"$2\r\na2\r\n"] {
        assert!(contains_bytes(&r, d), "hit {d:?} in {r:?}");
    }

    // DEL shrinks postings; DROPINDEX frees the name.
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"FT.DEL", b"idx", b"d2"]).await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"FT.DEL", b"idx", b"d2"]).await,
        b":0"
    );
    let r = cmd_full_reply(a, TOKEN, &[b"FT.SEARCH", b"idx", b"@body:hello"], 400).await;
    assert!(contains_bytes(&r, b":1\r\n$2\r\nd1\r\n"), "post-del: {r:?}");
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"FT.DROPINDEX", b"idx"]).await,
        b":1"
    );
    assert_eq!(
        cmd_one_shot(a, TOKEN, &[b"FT.SEARCH", b"idx", b"*"]).await,
        b"-ERR unknown index"
    );

    // CONFIG: fixed pair regardless of arguments.
    let r = cmd_full_reply(
        a,
        TOKEN,
        &[b"CONFIG", b"GET", b"cluster-require-full-coverage"],
        400,
    )
    .await;
    assert_eq!(
        r,
        b"*2\r\n$29\r\ncluster-require-full-coverage\r\n$2\r\nno\r\n"
    );
}
