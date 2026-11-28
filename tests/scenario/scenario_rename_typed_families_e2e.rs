//! Multi-store scenario #163 -- a library re-codes its shelves: every record family
//! (hash, zset, set, list, JSON, vectorset, FT index, stream + consumer group/PEL)
//! moves from the old shelf code to the new one by RENAME, driven by the MySQL move
//! plan `catalog_moves` read in `old_name` order; the inventory register `shelf_items`
//! (JOIN + SUM over the moved kinds) must agree with the RESP cardinalities at the new
//! names; old names are gone, a TTL travels (index) or is replaced (string <-> hash),
//! RENAMENX refuses an occupied code. A second stream leg renames a PLAIN name
//! (`orders/q0 -> orders/q9`: the full names hash apart from each other and from
//! their parent, both live under the PARENT's slot) -- entries and the group record
//! travel, XLEN is conserved, EXISTS/TYPE see the stream at the new name only,
//! RENAMENX onto it answers 0.
//! RESP RENAME/RENAMENX/TYPE/TTL/PEXPIRE/PERSIST + HASH/ZSET/SET/LIST/JSON/VECTORSET/STREAM + FT + MySQL. One REAL node, fronts mysql (RESP always).
use crate::common;
use crate::scenario_common;
use common::mysql::{aff, connect_root, ddl, ints, rows, run};
use mysql_async::Value as MVal;
use scenario_common::{bulk_at, front_world, int_of, json_of, ok, r, rb, rt, stream_ids, FrontSpec};
use std::time::Duration;

/// Hash tag of every shelf key: RENAME needs both names in one slot.
const T: &str = "{r163}";
/// The lending-desk stream and its new name: both parents AND both full names
/// share this slot (the slot-aligned shape of the plan row; a PLAIN name whose
/// full name hashes apart from its parent is renamed in leg g).
const STREAM_OLD: &str = "desk61/q5484";
const STREAM_NEW: &str = "archive1189/q16945";
const SLOT: u16 = 6208;
const NO_KEY: &str = "-ERR no such key";
const CROSSSLOT: &str = "-ERR CROSSSLOT Keys in request don't hash to the same slot";
const WRONGTYPE: &str = "-WRONGTYPE Operation against a key holding the wrong kind of value";
const DOC: &str = r#"{"title":"atlas","tags":["maps","charts"],"fee":1200}"#;
/// Shelf embeddings (dim 4); `e` duplicates `a` (a tie for VSIM).
const VECTORS: [(&str, [f64; 4]); 5] = [
    ("a", [1.0, 1.0, 1.0, 1.0]),
    ("b", [-1.0, -1.0, -1.0, -1.0]),
    ("c", [1.0, 0.0, 0.0, 0.0]),
    ("d", [0.0, 1.0, 0.0, 0.0]),
    ("e", [1.0, 1.0, 1.0, 1.0]),
];
/// The move plan in `old_name` order: (kind, old name, new name, items).
const PLAN: [(&str, &str, &str, i64); 8] = [
    ("stream", STREAM_OLD, STREAM_NEW, 3),
    ("json", "{r163}old-doc", "{r163}new-doc", 2),
    ("hash", "{r163}old-hash", "{r163}new-hash", 3),
    ("search", "{r163}old-idx", "{r163}new-idx", 2),
    ("list", "{r163}old-list", "{r163}new-list", 3),
    ("set", "{r163}old-set", "{r163}new-set", 3),
    ("vectorset", "{r163}old-vec", "{r163}new-vec", 5),
    ("zset", "{r163}old-zset", "{r163}new-zset", 3),
];

fn k(suffix: &str) -> String {
    format!("{T}{suffix}")
}

/// Slot of `key` as the router computes it: crc16 of the hash tag.
fn slot(key: &str) -> u16 {
    let tag = key
        .split_once('{')
        .and_then(|(_, rest)| rest.split_once('}'))
        .filter(|(tag, _)| !tag.is_empty())
        .map_or(key, |(tag, _)| tag);
    rdb::hash::crc16(tag.as_bytes()) % 16384
}

fn argv<'a>(args: &'a [&'a str]) -> Vec<&'a [u8]> {
    args.iter().map(|s| s.as_bytes()).collect()
}

/// Text of a one-shot reply (`+OK`, `:N`, `-ERR ...`, the `*N` header).
async fn text(a: &str, args: &[&str]) -> String {
    rt(a, &argv(args)).await
}

/// `TYPE` of `key` with the simple-string marker stripped.
async fn ty(a: &str, key: &str) -> String {
    text(a, &["TYPE", key]).await.trim_start_matches('+').to_string()
}

/// `:N` of a one-key command (HLEN, ZCARD, SCARD, LLEN, VCARD, XLEN, TTL, EXISTS).
async fn count(a: &str, cmd: &str, key: &str) -> i64 {
    int_of(&r(a, &[cmd.as_bytes(), key.as_bytes()]).await)
}

/// Full-drain reply text with a short quiet gap (the fixture's `rf`
/// parks 250ms per call).
async fn drain(a: &str, args: &[&str]) -> String {
    common::lite::text(&common::lite::cmd_full_reply_wait(a, common::TOKEN, &argv(args), 20, 10_000).await)
}

/// Bulk payloads of a flat array reply (`$N` headers skipped).
fn bulks(reply: &str) -> Vec<String> {
    let t: Vec<&str> = reply.split("\r\n").collect();
    (0..t.len().saturating_sub(1))
        .filter(|&i| t[i].strip_prefix('$').is_some_and(|n| n.parse::<usize>().is_ok()))
        .map(|i| t[i + 1].to_string())
        .collect()
}

/// Value that follows the field `name` in a flat XINFO/FT.INFO reply
/// (`:2`, `$1\r\n2` and a plain bulk all compare by their text).
fn field(reply: &str, name: &str) -> String {
    let t: Vec<&str> = reply.split("\r\n").collect();
    let i = t.iter().position(|x| *x == name).unwrap_or_else(|| panic!("no {name} in {reply}"));
    let next = t.get(i + 1).unwrap_or_else(|| panic!("no value after {name} in {reply}"));
    if next.starts_with('$') {
        return (*t.get(i + 2).unwrap_or_else(|| panic!("truncated {name} in {reply}"))).to_string();
    }
    next.trim_start_matches(':').to_string()
}

/// `(count, min, max, consumer, its pending)` of an XPENDING summary reply.
fn pel(reply: &str) -> (i64, String, String, String, i64) {
    let t: Vec<&str> = reply.split("\r\n").collect();
    let n = |i: usize| t[i].trim_start_matches(':').parse::<i64>().expect("int");
    (n(1), t[3].to_string(), t[5].to_string(), t[7].to_string(), n(8))
}

/// Cardinality of the moved `kind` at `key`, read through the type's own
/// command (the FT page of a match-all search carries `per = 2` per hit).
async fn card(a: &str, kind: &str, key: &str) -> i64 {
    match kind {
        "json" => int_of(&r(a, &[b"JSON.ARRLEN", key.as_bytes(), b"$.tags"]).await),
        "search" => {
            let head = text(a, &["FT.SEARCH", key, "*"]).await;
            (head.trim_start_matches('*').parse::<i64>().expect("array header") - 1) / 2
        }
        "hash" => count(a, "HLEN", key).await,
        "zset" => count(a, "ZCARD", key).await,
        "set" => count(a, "SCARD", key).await,
        "list" => count(a, "LLEN", key).await,
        "vectorset" => count(a, "VCARD", key).await,
        "stream" => count(a, "XLEN", key).await,
        other => panic!("unknown kind {other}"),
    }
}

/// VSIM WITHSCORES for `query` over VECTORS, recomputed: score (cos+1)/2
/// desc, element bytes asc on ties, spelled by Rust's shortest f64 roundtrip.
fn vsim(query: [f64; 4]) -> Vec<String> {
    let norm = |w: &[f64; 4]| w.iter().map(|x| x * x).sum::<f64>().sqrt();
    let mut hits: Vec<(String, f64)> = VECTORS
        .iter()
        .map(|(n, v)| {
            let dot: f64 = v.iter().zip(query).map(|(x, y)| x * y).sum();
            (n.to_string(), (dot / (norm(v) * norm(&query)) + 1.0) / 2.0)
        })
        .collect();
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    hits.into_iter().flat_map(|(n, s)| [n, format!("{s}")]).collect()
}

/// Poll XINFO GROUPS until the group's `committed-id` reached `want` (the
/// 200ms offset flusher persists acks).
async fn wait_committed(a: &str, stream: &str, want: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let g = drain(a, &["XINFO", "GROUPS", stream]).await;
        if field(&g, "committed-id") == want {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "committed-id never reached {want}: {g}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Text of a non-NULL SQL cell (ints arrive as bytes on the text protocol).
fn cell(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        other => panic!("unexpected SQL cell {other:?}"),
    }
}

#[tokio::test]
async fn rename_typed_families() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec { mysql: true, ..Default::default() };
    let (front, a) = front_world("rentyped", &spec).await;
    let (old_hash, new_hash) = (k("old-hash"), k("new-hash"));
    let (old_zset, new_zset) = (k("old-zset"), k("new-zset"));
    let (old_set, new_set) = (k("old-set"), k("new-set"));
    let (old_list, new_list) = (k("old-list"), k("new-list"));
    let (old_doc, new_doc) = (k("old-doc"), k("new-doc"));
    let (old_vec, new_vec) = (k("old-vec"), k("new-vec"));
    let (old_idx, new_idx) = (k("old-idx"), k("new-idx"));

    // a) Construction: every plan row is one slot; the stream's two parents
    //    and two full names share ONE slot (the aligned shape). Seeds.
    for (_, old, new, _) in PLAN {
        assert_eq!(slot(old), slot(new), "one slot for {old} -> {new}");
    }
    assert_eq!([STREAM_OLD, STREAM_NEW, "desk61", "archive1189"].map(slot), [SLOT; 4], "aligned stream names");
    assert_eq!(int_of(&r(&a, &[b"HSET", old_hash.as_bytes(), b"shelf", b"A7", b"seats", b"12", b"floor", b"3"]).await), 3, "hash seed");
    assert_eq!(int_of(&r(&a, &[b"ZADD", old_zset.as_bytes(), b"5", b"x", b"3", b"y", b"9", b"z"]).await), 3, "zset seed");
    assert_eq!(int_of(&r(&a, &[b"SADD", old_set.as_bytes(), b"x", b"y", b"z"]).await), 3, "set seed");
    assert_eq!(int_of(&r(&a, &[b"RPUSH", old_list.as_bytes(), b"a", b"b", b"c"]).await), 3, "list seed");
    ok(&r(&a, &[b"JSON.SET", old_doc.as_bytes(), b"$", DOC.as_bytes()]).await, "json seed");
    for (name, v) in VECTORS {
        let nums: Vec<String> = v.iter().map(f64::to_string).collect();
        let mut args: Vec<&[u8]> = vec![b"VADD", old_vec.as_bytes(), b"VALUES", b"4", name.as_bytes()];
        args.extend(nums.iter().map(|s| s.as_bytes()));
        assert_eq!(int_of(&r(&a, &args).await), 1, "vadd {name}");
    }
    ok(&r(&a, &[b"FT.CREATE", old_idx.as_bytes(), b"SCHEMA", b"title", b"TEXT"]).await, "ft.create");
    let titles = ["harbor atlas", "harbor ledger"];
    for (i, title) in titles.iter().enumerate() {
        let doc = format!("{{\"title\":\"{title}\"}}");
        let id = (i + 1).to_string();
        assert_eq!(int_of(&r(&a, &[b"FT.ADD", old_idx.as_bytes(), id.as_bytes(), doc.as_bytes()]).await), 1, "ft.add");
    }
    assert_eq!(int_of(&r(&a, &[b"PEXPIRE", old_idx.as_bytes(), b"700000"]).await), 1, "index TTL armed");
    assert!((601..=700).contains(&count(&a, "TTL", &old_idx).await), "index TTL in seconds");
    for (id, event) in [("1-1", "lend"), ("2-1", "return"), ("3-1", "lend")] {
        let got = rb(&a, &[b"XADD", STREAM_OLD.as_bytes(), id.as_bytes(), b"event", event.as_bytes(), b"copy", b"A7"]).await;
        assert_eq!(got, id, "xadd {id}");
    }
    ok(&r(&a, &[b"XGROUP", b"CREATE", STREAM_OLD.as_bytes(), b"desk", b"0-0"]).await, "xgroup create");
    let first = drain(&a, &["XREADGROUP", "GROUP", "desk", "c1", "COUNT", "2", "STREAMS", STREAM_OLD, ">"]).await;
    assert_eq!(stream_ids(&first), ["1-1", "2-1"], "c1's first deliveries: {first}");
    assert_eq!(pel(&drain(&a, &["XPENDING", STREAM_OLD, "desk"]).await), (2, "1-1".into(), "2-1".into(), "c1".into(), 2), "PEL before the move");
    ok(&r(&a, &[b"SET", new_hash.as_bytes(), b"stale", b"PX", b"700000"]).await, "stale destination");
    for (key, want) in [(old_hash.as_str(), "hash"), (&old_zset, "zset"), (&old_set, "set"), (&old_list, "list"), (&old_doc, "ReJSON-RL"), (&old_vec, "vectorset"), (&old_idx, "search"), (STREAM_OLD, "stream")] {
        assert_eq!(ty(&a, key).await, want, "TYPE {key}");
    }

    // b) The move plan is MySQL's: the mover reads the open rows in
    //    `old_name` order, renames each family and marks the row done.
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE catalog_moves (old_name VARCHAR(64) PRIMARY KEY, new_name VARCHAR(64) NOT NULL, kind VARCHAR(16) NOT NULL, done INT NOT NULL)").await;
    ddl(&mut c, "CREATE TABLE shelf_items (kind VARCHAR(16) PRIMARY KEY, items BIGINT NOT NULL)").await;
    let plan_sql: Vec<String> = PLAN.iter().map(|(kind, o, n, _)| format!("('{o}', '{n}', '{kind}', 0)")).collect();
    run(&mut c, &format!("INSERT INTO catalog_moves (old_name, new_name, kind, done) VALUES {}", plan_sql.join(", "))).await;
    let items_sql: Vec<String> = PLAN.iter().map(|(kind, _, _, items)| format!("('{kind}', {items})")).collect();
    run(&mut c, &format!("INSERT INTO shelf_items (kind, items) VALUES {}", items_sql.join(", "))).await;
    let total: i64 = PLAN.iter().map(|p| p.3).sum();
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM catalog_moves m JOIN shelf_items i ON i.kind = m.kind WHERE m.done = 0").await, vec![8], "open plan rows");
    assert_eq!(ints(&mut c, "SELECT SUM(i.items) FROM catalog_moves m JOIN shelf_items i ON i.kind = m.kind WHERE m.done = 0").await, vec![total], "items to move");
    let t = scenario_common::perf::now();
    let open = rows(&mut c, "SELECT m.old_name, m.new_name, m.kind FROM catalog_moves m WHERE m.done = 0 ORDER BY m.old_name").await;
    let mut expect: Vec<String> = PLAN.iter().map(|p| p.1.to_string()).collect();
    expect.sort();
    assert_eq!(open.iter().map(|r| cell(&r[0])).collect::<Vec<_>>(), expect, "plan read in old_name order");
    let before = int_of(&r(&a, &[b"DBSIZE"]).await);
    for row in &open {
        let (old, new, kind) = (cell(&row[0]), cell(&row[1]), cell(&row[2]));
        assert_eq!(text(&a, &["RENAME", &old, &new]).await, "+OK", "rename {kind} {old} -> {new}");
        assert_eq!(aff(&mut c, &format!("UPDATE catalog_moves SET done = 1 WHERE old_name = '{old}'")).await, 1, "plan row done");
    }
    let after = int_of(&r(&a, &[b"DBSIZE"]).await);
    scenario_common::perf::within("rentyped/8-renames+plan", t, Duration::from_secs(2));
    // a move, not a copy: the 8 old names + the stale string became the 8 new names
    assert_eq!((before, after), (9, 8), "DBSIZE across the move");
    for (_, old, _, _) in PLAN {
        assert_eq!(count(&a, "EXISTS", old).await, 0, "{old} is gone");
    }
    let moved = rows(&mut c, "SELECT m.kind, m.new_name, i.items FROM catalog_moves m JOIN shelf_items i ON i.kind = m.kind WHERE m.done = 1 ORDER BY m.old_name").await;
    assert_eq!(moved.len(), PLAN.len(), "every plan row is done");
    let mut moved_total = 0;
    for row in &moved {
        let (kind, new, items) = (cell(&row[0]), cell(&row[1]), cell(&row[2]).parse::<i64>().expect("items"));
        let got = card(&a, &kind, &new).await;
        assert_eq!(got, items, "{kind} cardinality at {new}");
        moved_total += got;
    }
    assert_eq!(moved_total, total, "RESP cardinalities sum == the register");
    assert_eq!(ints(&mut c, "SELECT SUM(items) FROM shelf_items").await, vec![total], "register sum == the Rust sum");

    // c) Every family is fully live at its new name: type-specific reads
    //    plus one post-move write per family.
    let raw: Vec<String> = bulks(&drain(&a, &["HGETALL", &new_hash]).await);
    let mut pairs: Vec<[String; 2]> = raw.chunks(2).map(|p| [p[0].clone(), p[1].clone()]).collect();
    pairs.sort();
    assert_eq!(pairs, [["floor", "3"], ["seats", "12"], ["shelf", "A7"]], "hash fields moved: {raw:?}");
    assert_eq!(int_of(&r(&a, &[b"HINCRBY", new_hash.as_bytes(), b"seats", b"1"]).await), 13, "hash write after the move");
    assert_eq!(bulks(&drain(&a, &["ZRANGE", &new_zset, "0", "-1", "WITHSCORES"]).await), ["y", "3", "x", "5", "z", "9"], "zset by score");
    assert_eq!(rb(&a, &[b"ZSCORE", new_zset.as_bytes(), b"y"]).await, "3", "zscore");
    assert_eq!(rb(&a, &[b"ZINCRBY", new_zset.as_bytes(), b"1", b"y"]).await, "4", "zincrby after the move");
    let mut members = bulks(&drain(&a, &["SMEMBERS", &new_set]).await);
    members.sort();
    assert_eq!(members, ["x", "y", "z"], "set members");
    assert_eq!(int_of(&r(&a, &[b"SREM", new_set.as_bytes(), b"x"]).await), 1, "srem after the move");
    assert_eq!(count(&a, "SCARD", &new_set).await, 2, "set shrank");
    assert_eq!(bulks(&drain(&a, &["LRANGE", &new_list, "0", "-1"]).await), ["a", "b", "c"], "list order kept");
    let (lpop, rpop) = (rb(&a, &[b"LPOP", new_list.as_bytes()]).await, rb(&a, &[b"RPOP", new_list.as_bytes()]).await);
    assert_eq!((lpop.as_str(), rpop.as_str()), ("a", "c"), "both list ends live");
    assert_eq!(count(&a, "LLEN", &new_list).await, 1, "list length");
    assert_eq!(json_of(&rb(&a, &[b"JSON.GET", new_doc.as_bytes()]).await), json_of(DOC), "JSON document moved");
    assert_eq!(rb(&a, &[b"JSON.NUMINCRBY", new_doc.as_bytes(), b"$.fee", b"5"]).await, "1205", "json write after the move");
    assert_eq!(json_of(&rb(&a, &[b"JSON.GET", new_doc.as_bytes()]).await)["fee"], 1205, "json write landed at the new name");
    assert_eq!((count(&a, "VCARD", &new_vec).await, count(&a, "VDIM", &new_vec).await), (5, 4), "vectorset cardinality/dim");
    let recall = drain(&a, &["VSIM", &new_vec, "WITHSCORES", "VALUES", "1", "1", "1", "1"]).await;
    assert_eq!(bulks(&recall), vsim([1.0; 4]), "VSIM order + scores recomputed: {recall}");
    assert_eq!(int_of(&r(&a, &[b"VREM", new_vec.as_bytes(), b"d"]).await), 1, "vrem after the move");
    assert_eq!(count(&a, "VCARD", &new_vec).await, 4, "vectorset shrank");
    assert_eq!(ty(&a, &new_idx).await, "search", "index type travelled");
    assert!((601..=700).contains(&count(&a, "TTL", &new_idx).await), "index TTL travelled");
    let hits = drain(&a, &["FT.SEARCH", &new_idx, "harbor", "WITHSCORES"]).await;
    assert_eq!((bulks(&hits)[0].as_str(), bulks(&hits)[3].as_str()), ("1", "2"), "both docs found under the new name: {hits}");
    let (s1, s2) = (bulk_at(hits.as_bytes(), 1), bulk_at(hits.as_bytes(), 4));
    assert!(s1 == s2 && s1.parse::<f64>().is_ok_and(|s| s > 0.0), "equal-length docs score alike: {s1} / {s2}");
    assert_eq!((json_of(&bulk_at(hits.as_bytes(), 2))["title"].as_str(), json_of(&bulk_at(hits.as_bytes(), 5))["title"].as_str()), (Some(titles[0]), Some(titles[1])), "FT payloads moved");
    assert_eq!(text(&a, &["FT.SEARCH", &old_idx, "harbor"]).await, "-ERR unknown index", "the old index name is unknown");
    let info = drain(&a, &["FT.INFO", &new_idx]).await;
    assert_eq!((field(&info, "index_name"), field(&info, "num_docs")), (new_idx.clone(), "2".into()), "FT.INFO at the new name: {info}");
    assert_eq!(int_of(&r(&a, &[b"PERSIST", new_idx.as_bytes()]).await), 1, "persist");
    assert_eq!(count(&a, "TTL", &new_idx).await, -1, "index now persistent");

    // d) The stream moved with its group and pending entries; the group
    //    record lives at the new name only (history reads are PEL-driven).
    assert_eq!((count(&a, "XLEN", STREAM_OLD).await, count(&a, "XLEN", STREAM_NEW).await, count(&a, "EXISTS", STREAM_NEW).await), (0, 3, 1), "stream entries moved");
    assert_eq!(ty(&a, STREAM_NEW).await, "stream", "stream type");
    let groups = drain(&a, &["XINFO", "GROUPS", STREAM_NEW]).await;
    assert!(groups.starts_with("*1\r\n*13\r\n$4\r\ndesk\r\n"), "one group at the new name: {groups}");
    assert_eq!(text(&a, &["XINFO", "GROUPS", STREAM_OLD]).await, "*0", "no group left at the old name");
    assert_eq!(pel(&drain(&a, &["XPENDING", STREAM_NEW, "desk"]).await), (2, "1-1".into(), "2-1".into(), "c1".into(), 2), "PEL moved with the stream");
    let history = drain(&a, &["XREADGROUP", "GROUP", "desk", "c1", "STREAMS", STREAM_NEW, "0-0"]).await;
    assert_eq!(stream_ids(&history), ["1-1", "2-1"], "c1's pending entries at the new name: {history}");
    assert_eq!(text(&a, &["XREADGROUP", "GROUP", "desk", "c2", "STREAMS", STREAM_NEW, "0-0"]).await, "*-1", "another consumer owns nothing");
    assert_eq!(int_of(&r(&a, &[b"XACK", STREAM_NEW.as_bytes(), b"desk", b"1-1"]).await), 1, "ack at the new name");
    assert_eq!(pel(&drain(&a, &["XPENDING", STREAM_NEW, "desk"]).await), (1, "2-1".into(), "2-1".into(), "c1".into(), 1), "PEL after the ack");
    wait_committed(&a, STREAM_NEW, "1-1").await;
    assert_eq!(rb(&a, &[b"XADD", STREAM_NEW.as_bytes(), b"4-1", b"event", b"return", b"copy", b"A7"]).await, "4-1", "xadd at the new name");
    let stream_info = drain(&a, &["XINFO", "STREAM", STREAM_NEW]).await;
    let (len, last, n_groups) = (field(&stream_info, "length"), field(&stream_info, "last-generated-id"), field(&stream_info, "groups"));
    assert_eq!((len.as_str(), last.as_str(), n_groups.as_str()), ("4", "4-1", "1"), "XINFO STREAM at the new name: {stream_info}");

    // e) TTL legs: the hash had replaced a TTL string wholesale (no TTL
    //    left, the string unreadable); a TTL string now replaces the hash
    //    and carries its own TTL.
    assert_eq!((ty(&a, &new_hash).await, count(&a, "TTL", &new_hash).await), ("hash".into(), -1), "hash over the TTL string");
    assert_eq!(text(&a, &["GET", &new_hash]).await, WRONGTYPE, "stale string gone");
    let tmp = k("tmp-str");
    ok(&r(&a, &[b"SET", tmp.as_bytes(), b"moved", b"PX", b"700000"]).await, "ttl string");
    assert_eq!(text(&a, &["RENAME", &tmp, &new_hash]).await, "+OK", "string over the hash");
    let ttl = count(&a, "PTTL", &new_hash).await;
    assert_eq!(ty(&a, &new_hash).await, "string", "string type");
    assert!(ttl > 600_000 && ttl <= 700_000, "string TTL carried in ms: {ttl}");
    assert_eq!((rb(&a, &[b"GET", new_hash.as_bytes()]).await, text(&a, &["HGETALL", &new_hash]).await), ("moved".into(), WRONGTYPE.into()), "hash family gone");
    assert_eq!(count(&a, "EXISTS", &tmp).await, 0, "string source gone");

    // f) RENAMENX + error pins: an occupied code is refused (nothing
    //    moves), a missing source is an error, a key renames onto itself.
    assert_eq!(int_of(&r(&a, &[b"RENAMENX", new_set.as_bytes(), new_list.as_bytes()]).await), 0, "occupied destination");
    assert_eq!((count(&a, "SCARD", &new_set).await, count(&a, "LLEN", &new_list).await), (2, 1), "nothing moved");
    let (spare, fresh) = (k("spare"), k("fresh"));
    ok(&r(&a, &[b"SET", spare.as_bytes(), b"reserve"]).await, "spare");
    assert_eq!(int_of(&r(&a, &[b"RENAMENX", spare.as_bytes(), fresh.as_bytes()]).await), 1, "free destination");
    assert_eq!((rb(&a, &[b"GET", fresh.as_bytes()]).await, count(&a, "EXISTS", &spare).await), ("reserve".into(), 0), "NX move");
    assert_eq!((text(&a, &["RENAME", "{r163}nope", "{r163}x"]).await, text(&a, &["RENAMENX", "{r163}nope", "{r163}x"]).await), (NO_KEY.into(), NO_KEY.into()), "missing source");
    assert_eq!((text(&a, &["RENAME", &new_list, &new_list]).await, text(&a, &["RENAME", "{r163}nope", "{r163}nope"]).await), ("+OK".into(), NO_KEY.into()), "same key");
    assert_ne!(slot("{a163}k"), slot("{b163}k"), "distinct slots by construction");
    assert_eq!(text(&a, &["RENAME", "{a163}k", "{b163}k"]).await, CROSSSLOT, "cross-slot");

    // g) A PLAIN stream name: `orders/q0 -> orders/q9` hash apart from each
    //    other AND from their parent, yet both live under the PARENT's slot,
    //    so RENAME moves the whole family there -- entries (XLEN/XRANGE
    //    conserved), the group record -- and EXISTS/TYPE resolve the new
    //    name while the old one is gone; RENAMENX onto it answers 0.
    let (plain_old, plain_new, plain_src) = ("orders/q0", "orders/q9", "orders/q1");
    let parent_slot = slot("orders");
    assert!([slot(plain_old), slot(plain_new), slot(plain_src)].iter().all(|s| *s != parent_slot) && slot(plain_old) != slot(plain_new), "plain names hash apart from the parent by construction");
    let entries = ["5-1", "event", "hold", "copy", "A7", "6-1", "event", "renew", "copy", "A7"];
    for (id, event) in [("5-1", "hold"), ("6-1", "renew")] {
        let got = rb(&a, &[b"XADD", plain_old.as_bytes(), id.as_bytes(), b"event", event.as_bytes(), b"copy", b"A7"]).await;
        assert_eq!(got, id, "xadd {id}");
    }
    ok(&r(&a, &[b"XGROUP", b"CREATE", plain_old.as_bytes(), b"g0", b"0-0"]).await, "xgroup create");
    assert_eq!(bulks(&drain(&a, &["XRANGE", plain_old, "-", "+"]).await), entries, "entries before the move");
    assert_eq!((count(&a, "XLEN", plain_old).await, count(&a, "EXISTS", plain_old).await, ty(&a, plain_old).await), (2, 1, "stream".into()), "the plain stream is visible before the move");
    assert_eq!(text(&a, &["RENAME", plain_old, plain_new]).await, "+OK", "plain stream rename");
    assert_eq!(bulks(&drain(&a, &["XRANGE", plain_new, "-", "+"]).await), entries, "entries after the move");
    assert_eq!((count(&a, "XLEN", plain_new).await, count(&a, "EXISTS", plain_old).await, count(&a, "EXISTS", plain_new).await, ty(&a, plain_new).await), (2, 0, 1, "stream".into()), "XLEN conserved, old name gone, new name present");
    assert_eq!(text(&a, &["XINFO", "STREAM", plain_old]).await, NO_KEY, "XINFO STREAM at the old name");
    let plain_groups = drain(&a, &["XINFO", "GROUPS", plain_new]).await;
    assert!(plain_groups.starts_with("*1\r\n*13\r\n$2\r\ng0\r\n"), "g0 travelled with the family: {plain_groups}");
    assert_eq!(text(&a, &["XINFO", "GROUPS", plain_old]).await, "*0", "no group left at the old name");
    assert_eq!(rb(&a, &[b"XADD", plain_src.as_bytes(), b"7-1", b"event", b"reserve"]).await, "7-1", "xadd the second plain stream");
    assert_eq!(int_of(&r(&a, &[b"RENAMENX", plain_src.as_bytes(), plain_new.as_bytes()]).await), 0, "occupied stream destination");
    assert_eq!((count(&a, "XLEN", plain_src).await, count(&a, "XLEN", plain_new).await), (1, 2), "nothing moved");
    assert_eq!(text(&a, &["RENAME", plain_old, plain_new]).await, NO_KEY, "the old name is gone for RENAME too");
    scenario_common::perf::scenario("rentyped", t0);
}
