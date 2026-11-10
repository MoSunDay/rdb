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
    let (shared, _dir) = shared_at("45007");
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

/// FT.SEARCH must answer with ONE flat array (`*N` header, integer
/// total, then per-hit docid/[score]/[content]). Regression: the
/// header was missing, so every element left the server as a
/// standalone RESP value and clients only ever rendered the total.
#[test]
fn ft_search_reply_is_a_flat_array() {
    let (shared, _dir) = shared_at("45009");
    ok(
        &call(
            &shared,
            "ft.create",
            &[
                b"idx", b"SCHEMA", b"body", b"TEXT", b"vec", b"VECTOR", b"DIM", b"2",
            ],
        ),
        "create",
    );
    assert_eq!(
        call(
            &shared,
            "ft.add",
            &[b"idx", b"d1", br#"{"body":"hello","vec":[1,0]}"#]
        ),
        b":1\r\n".to_vec(),
        "add d1"
    );
    assert_eq!(
        call(
            &shared,
            "ft.add",
            &[b"idx", b"d2", br#"{"body":"world","vec":[0,1]}"#]
        ),
        b":1\r\n".to_vec(),
        "add d2"
    );
    // Text search: *3 = total + (docid + content) per hit.
    let r = call(&shared, "ft.search", &[b"idx", b"@body:hello"]);
    assert_eq!(r.first(), Some(&b'*'), "array header: {r:?}");
    assert!(r.starts_with(b"*3\r\n:1\r\n"), "text shape: {r:?}");
    // WITHSCORES + NOCONTENT: *3 = total + docid + score per hit.
    let r = call(
        &shared,
        "ft.search",
        &[b"idx", b"*", b"WITHSCORES", b"NOCONTENT"],
    );
    assert!(r.starts_with(b"*5\r\n:2\r\n"), "knn/score shape: {r:?}");
}

/// FT.DROPINDEX is a plain ALIAS of FT.DROP (one shared handler): both
/// wipe the whole search family -- index meta, postings AND documents --
/// unlike RediSearch, where DROPINDEX keeps docs unless DD is passed.
/// No DD option exists here; the arity error even leaks the canonical
/// 'ft.drop' name.
#[test]
fn ft_dropindex_alias_drops_docs_like_ft_drop() {
    let (shared, _dir) = shared_at("45411");
    let create = |idx: &[u8]| {
        ok(
            &call(&shared, "ft.create", &[idx, b"SCHEMA", b"body", b"TEXT"]),
            "create",
        );
    };
    // No DD option: exactly one argument, whichever spelling is used.
    err_has(
        &call(&shared, "ft.dropindex", &[b"i1", b"DD"]),
        "wrong number of arguments for 'ft.drop' command",
        "dropindex DD",
    );
    // After either drop the index is gone AND the doc records died with
    // it: a re-created index starts empty (num_docs 0, old terms find
    // nothing) -- docs are NOT kept like RediSearch's DROPINDEX.
    let gone = |idx: &[u8]| {
        err_has(
            &call(&shared, "ft.search", &[idx, b"*"]),
            "unknown index",
            "post-drop",
        );
        assert_eq!(call(&shared, "exists", &[idx]), b":0\r\n".to_vec());
        create(idx);
        let r = call(&shared, "ft.search", &[idx, b"@body:hello"]);
        assert!(contains_bytes(&r, b":0\r\n"), "stale docs: {r:?}");
        let info = call(&shared, "ft.info", &[idx]);
        assert!(
            contains_bytes(&info, b"num_docs\r\n:0\r\n"),
            "info {info:?}"
        );
    };
    for (idx, cmd) in [(b"i1".as_slice(), "ft.dropindex"), (b"i2", "ft.drop")] {
        create(idx);
        assert_eq!(
            call(&shared, "ft.add", &[idx, b"d1", br#"{"body":"hello"}"#]),
            b":1\r\n".to_vec()
        );
        assert_eq!(
            call(&shared, "ft.add", &[idx, b"d2", br#"{"body":"world"}"#]),
            b":1\r\n".to_vec()
        );
        assert_eq!(call(&shared, cmd, &[idx]), b":1\r\n".to_vec(), "in '{cmd}'");
        gone(idx);
        err_has(
            &call(&shared, cmd, &[b"nope"]),
            "unknown index",
            "drop unknown",
        );
    }
}

/// Dialect v2 operators over the same corpus as the lifecycle test:
/// phrases (positions), prefix scans, `-`/NOT exclusion, `|` union and
/// the strict implicit AND (multi-term queries now INTERSECT, the
/// documented v1 contract; v1 code unioned them).
#[test]
fn ft_dialect_v2_operators() {
    let (shared, _dir) = shared_at("45021");
    ok(
        &call(
            &shared,
            "ft.create",
            &[b"idx", b"SCHEMA", b"title", b"TEXT", b"body", b"TEXT"],
        ),
        "create",
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

    let q = |query: &str| call(&shared, "ft.search", &[b"idx", query.as_bytes()]);

    // phrase: adjacency against the v2 positions.
    let r = q(r#""full text""#); // title phrase only d2 has
    assert!(contains_bytes(&r, b":1\r\n"), "phrase full text: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d2");
    let r = q(r#"@body:"redis world""#); // adjacent only in d1
    assert!(contains_bytes(&r, b":1\r\n"), "phrase redis world: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d1");
    let r = q(r#"@body:"hello redis""#); // adjacent in BOTH bodies
    assert!(contains_bytes(&r, b":2\r\n"), "phrase hello redis: {r:?}");
    // reversed order breaks the adjacency chain
    let r = q(r#"@body:"redis hello""#);
    assert!(contains_bytes(&r, b":0\r\n"), "reversed phrase: {r:?}");

    // CJK phrase: bigram ordinals chain (中文检索 -> 中文|文检|检索).
    let r = q(r#"@title:"中文检索""#);
    assert!(contains_bytes(&r, b":1\r\n"), "cjk phrase: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d3");

    // prefix: whole-query star AND scoped.
    let r = q("red*");
    assert!(contains_bytes(&r, b":2\r\n"), "prefix red*: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d1");
    assert_eq!(bulk_at(&r, 2), "d2");
    let r = q("@title:red*");
    assert!(contains_bytes(&r, b":1\r\n"), "scoped prefix: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d1");

    // exclusion: '-' and uppercase NOT (lowercase is a plain term).
    let r = q("@body:hello -@body:world");
    assert!(contains_bytes(&r, b":1\r\n"), "minus exclusion: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d2");
    let r = q("@body:hello NOT @body:world");
    assert!(contains_bytes(&r, b":1\r\n"), "NOT exclusion: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d2");

    // union: '|' between clauses.
    let r = q("@title:full | @title:quickstart");
    assert!(contains_bytes(&r, b":2\r\n"), "pipe union: {r:?}");

    // strict implicit AND: both terms must match (v1 unioned these).
    let r = q("hello quickstart");
    assert!(contains_bytes(&r, b":1\r\n"), "strict AND: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d1");
    // mixed CJK + latin AND across fields
    let r = q("@title:检索 @body:引擎");
    assert!(contains_bytes(&r, b":1\r\n"), "cjk AND: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d3");

    // star alone still matches everything.
    let r = q("*");
    assert!(contains_bytes(&r, b":3\r\n"), "star: {r:?}");
}

/// WP3 索引级停用词 + BM25 k1/b：索引侧过滤（位置紧凑）、查询侧过滤、
/// FT.INFO 回显与 FT.CREATE 选项校错。
#[test]
fn ft_stopwords_and_bm25_knobs() {
    let (shared, _dir) = shared_at("45031");
    ok(
        &call(
            &shared,
            "ft.create",
            &[
                b"si",
                b"STOPWORDS",
                b"2",
                b"the",
                b"of",
                b"K1",
                b"1.5",
                b"B",
                b"0.5",
                b"SCHEMA",
                b"body",
                b"TEXT",
            ],
        ),
        "create",
    );
    let add = |docid: &[u8], json: &[u8]| {
        assert_eq!(
            call(&shared, "ft.add", &[b"si", docid, json]),
            b":1\r\n".to_vec(),
            "add {docid:?}"
        );
    };
    add(b"d1", br#"{"body":"the quick fox of the bay"}"#);

    // 停用词：索引侧没建 posting，查询侧也短路，总数 0。
    let r = call(&shared, "ft.search", &[b"si", b"the"]);
    assert!(contains_bytes(&r, b":0\r\n"), "stopword term: {r:?}");
    // 普通词不受影响。
    let r = call(&shared, "ft.search", &[b"si", b"quick"]);
    assert!(contains_bytes(&r, b":1\r\n"), "plain term: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d1");

    // 位置紧凑：过滤 the 后 quick/fox 相邻（RediSearch 保留空洞时 d2 不会命中）。
    add(b"d2", br#"{"body":"the quick the fox"}"#);
    let r = call(&shared, "ft.search", &[b"si", br#""quick fox""#]);
    assert!(contains_bytes(&r, b":2\r\n"), "compact phrase: {r:?}");
    assert!(contains_bytes(&r, b"d2"), "d2 compact adjacency: {r:?}");
    // 短语里只剩 1 个实词时按单 term 短语退化：两篇都含 fox。
    let r = call(&shared, "ft.search", &[b"si", br#""the fox""#]);
    assert!(contains_bytes(&r, b":2\r\n"), "stopword-head phrase: {r:?}");

    // FT.INFO 回显 stopwords/k1/b。
    let info = call(&shared, "ft.info", &[b"si"]);
    for want in [
        &b"stopwords"[..],
        &b"k1"[..],
        &b"b"[..],
        &b"1.5"[..],
        &b"0.5"[..],
    ] {
        assert!(contains_bytes(&info, want), "info {want:?}: {info:?}");
    }

    // 选项校错：未知选项 / 词数不匹配 / 坏 K1。
    err_has(
        &call(
            &shared,
            "ft.create",
            &[b"x1", b"NOPE", b"SCHEMA", b"f", b"TEXT"],
        ),
        "unknown option",
        "unknown option",
    );
    err_has(
        &call(
            &shared,
            "ft.create",
            &[b"x2", b"STOPWORDS", b"2", b"only", b"SCHEMA", b"f", b"TEXT"],
        ),
        "STOPWORDS",
        "count mismatch",
    );
    err_has(
        &call(
            &shared,
            "ft.create",
            &[b"x3", b"K1", b"abc", b"SCHEMA", b"f", b"TEXT"],
        ),
        "invalid K1",
        "bad k1",
    );
}

/// WP4：`@f:[min max]` 数值范围 + SORTBY/RETURN。范围查询走共享
/// query_tree 的 NumRange（doc-values 列扫描，常量分 1.0）；SORTBY
/// 按 NUMERIC 列排全量命中（缺失值两个方向都最后），total = 全匹
/// 配数；RETURN 投影文档 JSON。
#[test]
fn ft_numeric_range_sortby_return() {
    let (shared, _dir) = shared_at("45025");
    ok(
        &call(
            &shared,
            "ft.create",
            &[b"nidx", b"SCHEMA", b"t", b"TEXT", b"price", b"NUMERIC"],
        ),
        "create",
    );
    let add = |docid: &[u8], json: &[u8]| {
        assert_eq!(
            call(&shared, "ft.add", &[b"nidx", docid, json]),
            b":1\r\n".to_vec(),
            "add {docid:?}"
        );
    };
    add(b"d1", br#"{"t":"cheap","price":5}"#);
    add(b"d2", br#"{"t":"mid","price":10}"#);
    add(b"d3", br#"{"t":"high","price":20}"#);
    add(b"d4", br#"{"t":"no price"}"#);

    let q = |args: &[&[u8]]| call(&shared, "ft.search", args);

    // 范围命中 10/20 两篇，同分按 docid 升序
    let r = q(&[b"nidx", b"@price:[10 20]"]);
    assert!(contains_bytes(&r, b":2\r\n"), "range: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d2");
    assert_eq!(bulk_at(&r, 2), "d3");
    // 开界：`*` 上界 + -inf 下界
    let r = q(&[b"nidx", b"@price:[-inf 10]"]);
    assert!(contains_bytes(&r, b":2\r\n"), "open bounds: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d1");
    // DESC 反转；缺失值两个方向都排最后
    let r = q(&[b"nidx", b"@price:[10 20]", b"SORTBY", b"price", b"DESC"]);
    assert_eq!(bulk_at(&r, 0), "d3", "desc: {r:?}");
    assert_eq!(bulk_at(&r, 2), "d2", "desc: {r:?}");
    let r = q(&[b"nidx", b"*", b"SORTBY", b"price"]);
    assert!(contains_bytes(&r, b":4\r\n"), "asc total: {r:?}");
    for (i, want) in ["d1", "d2", "d3", "d4"].iter().enumerate() {
        assert_eq!(&bulk_at(&r, i * 2), want, "asc order: {r:?}");
    }
    let r = q(&[b"nidx", b"*", b"SORTBY", b"price", b"DESC"]);
    for (i, want) in ["d3", "d2", "d1", "d4"].iter().enumerate() {
        assert_eq!(&bulk_at(&r, i * 2), want, "desc missing-last: {r:?}");
    }
    // WITHSCORES 与 SORTBY 共存：scan_all 常量分照报
    let r = q(&[b"nidx", b"*", b"WITHSCORES", b"SORTBY", b"price"]);
    assert_eq!(bulk_at(&r, 1), "1.0000", "withscores+sortby: {r:?}");
    // 分页：LIMIT 1 2 -> d2,d3；total 仍为全匹配数 4
    let r = q(&[b"nidx", b"*", b"SORTBY", b"price", b"LIMIT", b"1", b"2"]);
    assert!(contains_bytes(&r, b":4\r\n"), "limit total: {r:?}");
    assert_eq!(bulk_at(&r, 0), "d2");
    assert_eq!(bulk_at(&r, 2), "d3");

    // RETURN 投影：只留存在的键（顺序跟随列表）；RETURN 0 -> {}
    let r = q(&[b"nidx", b"@price:[10 20]", b"RETURN", b"1", b"t"]);
    assert_eq!(bulk_at(&r, 1), r#"{"t":"mid"}"#, "return 1: {r:?}");
    assert_eq!(bulk_at(&r, 3), r#"{"t":"high"}"#, "return 1: {r:?}");
    let r = q(&[b"nidx", b"@t:cheap", b"RETURN", b"2", b"price", b"nope"]);
    assert_eq!(bulk_at(&r, 1), r#"{"price":5}"#, "missing key skips: {r:?}");
    let r = q(&[b"nidx", b"@t:cheap", b"RETURN", b"0"]);
    assert_eq!(bulk_at(&r, 1), "{}", "return 0: {r:?}");

    // 校错：非 NUMERIC 排序键 / 与 KNN 互斥（两种出现顺序）/ 坏区间
    err_has(
        &q(&[b"nidx", b"*", b"SORTBY", b"t"]),
        "not NUMERIC",
        "sortby text field",
    );
    err_has(
        &q(&[
            b"nidx", b"*", b"SORTBY", b"price", b"KNN", b"1", b"@price", b"VALUES", b"1",
        ]),
        "cannot be used with KNN",
        "sortby+knn",
    );
    err_has(
        // VALUES 吞尾参，反向互斥用 FP16 形式才能让 SORTBY 被解析到
        &q(&[
            b"nidx", b"*", b"KNN", b"1", b"@price", b"FP16", b"x", b"SORTBY", b"price",
        ]),
        "cannot be used with KNN",
        "knn+sortby",
    );
    err_has(
        &q(&[b"nidx", b"@price:[10]"]),
        "bad numeric range",
        "one bound",
    );
}
