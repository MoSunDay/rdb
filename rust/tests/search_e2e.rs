//! FT.* search-engine e2e: text index lifecycle (create/add/search/del/
//! drop) with BM25 ordering, doc replacement, the SPANN vector path
//! (brute force pre-build, trained + probed post-build, term prefilter)
//! and TTL family purge -- all through the real registry + RocksDB.

mod common;

use common::contains_bytes;
use common::lite::{call, shared_at};

fn ok(reply: &[u8], what: &str) {
    assert!(reply.starts_with(b"+OK"), "{what}: {reply:?}");
}

fn err_has(reply: &[u8], needle: &str, what: &str) {
    assert!(
        reply.starts_with(b"-") && contains_bytes(reply, needle.as_bytes()),
        "{what}: {reply:?}"
    );
}

/// N-th bulk payload of a reply, skipping the leading integer count
/// (FT.SEARCH: docid, [score], [content] per hit -- all bulk typed).
fn bulk_at(reply: &[u8], n: usize) -> String {
    let s = String::from_utf8_lossy(reply);
    let mut rest: &str = &s;
    if rest.starts_with(':') {
        rest = rest.split_once("\r\n").map(|(_, r)| r).unwrap_or("");
    }
    let mut items = 0usize;
    let mut it = rest.split("\r\n");
    while let Some(head) = it.next() {
        if let Some(len) = head.strip_prefix('$') {
            let len: usize = len.parse().unwrap();
            let body = it.next().unwrap_or("");
            if items == n {
                return String::from_utf8_lossy(&body.as_bytes()[..len.min(body.len())])
                    .into_owned();
            }
            items += 1;
        }
    }
    panic!("no bulk #{n} in {reply:?}");
}

#[test]
fn ft_text_lifecycle_and_ranking() {
    let (shared, _dir) = shared_at("45001");
    ok(
        &call(
            &shared,
            "ft.create",
            &[b"idx", b"SCHEMA", b"title", b"TEXT", b"body", b"TEXT"],
        ),
        "create",
    );
    // Duplicate name and bad type keyword are refused.
    err_has(
        &call(&shared, "ft.create", &[b"idx", b"SCHEMA", b"x", b"TEXT"]),
        "already exists",
        "dup create",
    );
    err_has(
        &call(&shared, "ft.create", &[b"i2", b"SCHEMA", b"x", b"blob"]),
        "unknown field type",
        "bad type",
    );

    let add = |docid: &[u8], json: &[u8]| {
        assert_eq!(
            call(&shared, "ft.add", &[b"idx", docid, json]),
            b":1\r\n".to_vec(),
            "add {docid:?}"
        );
    };
    add(
        b"d1",
        br#"{"title":"redis quickstart","body":"hello redis world"}"#,
    );
    add(
        b"d2",
        br#"{"title":"full text","body":"hello hello hello redis"}"#,
    );
    let zh = r#"{"title":"中文检索","body":"全文检索引擎"}"#;
    add(b"d3", zh.as_bytes());
    err_has(
        &call(&shared, "ft.add", &[b"nope", b"x", b"{}"]),
        "unknown index",
        "add unknown index",
    );

    // BM25: tf=3 beats tf=1, so d2 ranks above d1 for @body:hello.
    let r = call(&shared, "ft.search", &[b"idx", b"@body:hello"]);
    assert!(contains_bytes(&r, b":2\r\n"), "hello hits: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d2", "rank1 {r:?}");
    assert_eq!(bulk_at(&r, 2), "d1", "rank2 {r:?}");

    // Chinese: query term survives segmentation on both sides.
    let zh_q = "@title:检索";
    let r = call(&shared, "ft.search", &[b"idx", zh_q.as_bytes()]);
    assert!(contains_bytes(&r, b":1\r\n"), "zh hits: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d3");

    // WITHSCORES + NOCONTENT: score first, no JSON body afterwards.
    let r = call(
        &shared,
        "ft.search",
        &[b"idx", b"@body:hello", b"WITHSCORES", b"NOCONTENT"],
    );
    let s2: f64 = bulk_at(&r, 1).parse().unwrap();
    let s1: f64 = bulk_at(&r, 3).parse().unwrap();
    assert!(s2 > s1 && s1 > 0.0, "scores {s2} {s1} ({r:?})");
    assert!(!contains_bytes(&r, b"{"), "NOCONTENT leaked json: {r:?}");

    // LIMIT paging over match-all.
    let all = call(&shared, "ft.search", &[b"idx", b"*"]);
    assert!(contains_bytes(&all, b":3\r\n"), "match-all {all:?}");
    let page = call(&shared, "ft.search", &[b"idx", b"*", b"LIMIT", b"0", b"1"]);
    assert!(contains_bytes(&page, b":1\r\n"), "page {page:?}");
    err_has(
        &call(&shared, "ft.search", &[b"idx", b"*", b"LIMIT", b"x"]),
        "bad LIMIT",
        "bad limit",
    );

    // INFO: flat pairs, num_docs reflects adds.
    let info = call(&shared, "ft.info", &[b"idx"]);
    assert!(contains_bytes(&info, b"num_docs"), "info {info:?}");
    assert!(contains_bytes(&info, b":3\r\n"), "info num_docs {info:?}");

    // Replacement: re-adding d2 must retire its old postings.
    add(b"d2", br#"{"title":"full text","body":"goodbye"}"#);
    let r = call(&shared, "ft.search", &[b"idx", b"@body:hello"]);
    assert!(contains_bytes(&r, b":1\r\n"), "post-replace {r:?}");
    assert_eq!(bulk_at(&r, 0), "d1");
    let r = call(&shared, "ft.search", &[b"idx", b"@body:goodbye"]);
    assert!(contains_bytes(&r, b":1\r\n"), "new term {r:?}");

    // DEL: postings shrink, second delete is a no-op.
    assert_eq!(
        call(&shared, "ft.del", &[b"idx", b"d2"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "ft.del", &[b"idx", b"d2"]),
        b":0\r\n".to_vec()
    );
    let r = call(&shared, "ft.search", &[b"idx", b"@body:goodbye"]);
    assert!(contains_bytes(&r, b":0\r\n"), "post-del {r:?}");

    // DROP: whole family; the name becomes reusable.
    assert_eq!(call(&shared, "ft.drop", &[b"idx"]), b":1\r\n".to_vec());
    err_has(
        &call(&shared, "ft.search", &[b"idx", b"*"]),
        "unknown index",
        "search after drop",
    );
    ok(
        &call(&shared, "ft.create", &[b"idx", b"SCHEMA", b"body", b"TEXT"]),
        "recreate",
    );
    let r = call(&shared, "ft.search", &[b"idx", b"@body:hello"]);
    assert!(
        contains_bytes(&r, b":0\r\n"),
        "stale postings leaked: {r:?}"
    );
}

#[test]
fn ft_knn_bruteforce_build_and_prefilter() {
    let (shared, _dir) = shared_at("45002");
    ok(
        &call(
            &shared,
            "ft.create",
            &[
                b"vidx", b"SCHEMA", b"t", b"TEXT", b"v", b"VECTOR", b"DIM", b"4",
            ],
        ),
        "create",
    );
    let add = |docid: &[u8], t: &str, v: [f64; 4]| {
        let json = format!(r#"{{"t":"{t}","v":[{},{},{},{}]}}"#, v[0], v[1], v[2], v[3]);
        assert_eq!(
            call(&shared, "ft.add", &[b"vidx", docid, json.as_bytes()]),
            b":1\r\n".to_vec()
        );
    };
    add(b"a0", "blue", [0.0, 0.0, 0.0, 0.0]);
    add(b"a1", "blue", [0.1, 0.0, 0.0, 0.0]);
    add(b"a2", "red", [0.0, 0.2, 0.0, 0.0]);
    add(b"a3", "red", [0.0, 0.0, 0.3, 0.0]);
    for i in 0..4 {
        let d = format!("b{i}");
        add(d.as_bytes(), "far", [10.0, 10.0, 10.0, 10.0]);
    }

    // Before FT.BUILD there is no centroid table: exact scan. The query
    // is equidistant (l2=0.0025) from a1 and a0: docid-asc tie-break.
    let r = call(
        &shared,
        "ft.search",
        &[
            b"vidx", b"*", b"KNN", b"2", b"v", b"VALUES", b"0.05", b"0", b"0", b"0",
        ],
    );
    assert!(contains_bytes(&r, b":2\r\n"), "knn pre-build {r:?}");
    assert_eq!(bulk_at(&r, 0), "a0");
    assert_eq!(bulk_at(&r, 2), "a1");
    err_has(
        &call(&shared, "ft.search", &[b"vidx", b"*", b"KNN", b"2", b"v"]),
        "KNN needs FP16 or VALUES",
        "knn without values",
    );
    err_has(
        &call(
            &shared,
            "ft.search",
            &[
                b"vidx", b"*", b"KNN", b"2", b"w", b"VALUES", b"0", b"0", b"0", b"0",
            ],
        ),
        "unknown vector field",
        "wrong field",
    );

    // Train two centroids; INFO reports the table.
    ok(
        &call(&shared, "ft.build", &[b"vidx", b"K", b"2", b"SEED", b"7"]),
        "build",
    );
    let info = call(&shared, "ft.info", &[b"vidx"]);
    assert!(contains_bytes(&info, b"ann_built"), "info {info:?}");
    assert!(
        contains_bytes(&info, b"ann_centroids\r\n:2\r\n"),
        "centroids {info:?}"
    );

    // Probed search still lands on the near cluster (nprobe covers it).
    let r = call(
        &shared,
        "ft.search",
        &[
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
    );
    assert!(contains_bytes(&r, b":3\r\n"), "knn post-build {r:?}");
    for i in 0..3 {
        let d = bulk_at(&r, i as usize); // NOCONTENT: docids only
        assert!(d.starts_with('a'), "hit{i} = {d} ({r:?})");
    }
    // Parity: with nprobe covering every centroid the probe+rerank
    // path must reproduce the pre-build brute-force answer exactly
    // (same docids, same docid-asc tie-break) -- recall@k = 1.0.
    let r = call(
        &shared,
        "ft.search",
        &[
            b"vidx",
            b"*",
            b"NOCONTENT",
            b"NPROBE",
            b"2",
            b"KNN",
            b"2",
            b"v",
            b"VALUES",
            b"0.05",
            b"0",
            b"0",
            b"0",
        ],
    );
    assert_eq!(bulk_at(&r, 0), "a0", "parity hit0 {r:?}");
    assert_eq!(bulk_at(&r, 1), "a1", "parity hit1 {r:?}");
    // WITHSCORES: score is 1/(1+l2) of the SQ8-dequantized rerank, so it
    // sits near -- not at -- the exact 0.9975 of the pre-build scan.
    let r = call(
        &shared,
        "ft.search",
        &[
            b"vidx",
            b"*",
            b"WITHSCORES",
            b"NPROBE",
            b"2",
            b"KNN",
            b"2",
            b"v",
            b"VALUES",
            b"0.05",
            b"0",
            b"0",
            b"0",
        ],
    );
    let score: f64 = bulk_at(&r, 1).parse().unwrap();
    assert!((score - 0.9975).abs() < 0.05, "score {score} ({r:?})");

    // Term prefilter: only blue docs (a0,a1) are candidates.
    let r = call(
        &shared,
        "ft.search",
        &[
            b"vidx", b"@t:blue", b"KNN", b"2", b"v", b"VALUES", b"0", b"0", b"0", b"0",
        ],
    );
    assert!(contains_bytes(&r, b":2\r\n"), "prefilter {r:?}");
    assert_eq!(bulk_at(&r, 0), "a0");
    assert_eq!(bulk_at(&r, 2), "a1");
}

#[test]
fn ft_ttl_expires_whole_family() {
    let (shared, _dir) = shared_at("45003");
    ok(
        &call(&shared, "ft.create", &[b"idx", b"SCHEMA", b"body", b"TEXT"]),
        "create",
    );
    assert_eq!(
        call(
            &shared,
            "ft.add",
            &[b"idx", b"d1", br#"{"body":"hello world"}"#]
        ),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "expire", &[b"idx", b"1"]),
        b":1\r\n".to_vec(),
        "expire index key"
    );
    std::thread::sleep(std::time::Duration::from_millis(1100));
    // Resolving the expired index purges the family: the index is gone
    // and no stale postings survive a re-create under the same name.
    err_has(
        &call(&shared, "ft.search", &[b"idx", b"@body:hello"]),
        "unknown index",
        "expired",
    );
    ok(
        &call(&shared, "ft.create", &[b"idx", b"SCHEMA", b"body", b"TEXT"]),
        "recreate",
    );
    let r = call(&shared, "ft.search", &[b"idx", b"@body:hello"]);
    assert!(
        contains_bytes(&r, b":0\r\n"),
        "stale postings after TTL: {r:?}"
    );
}
