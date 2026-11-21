//! Multi-store scenario #36 -- a config-center key lifecycle on the
//! RESP keyspace plane: TTL/EXPIRE/PERSIST round trip, one key of every
//! TYPE, RENAME carrying the TTL along (same-slot hash-tagged pair), a
//! full SCAN cursor walk with MATCH + COUNT pagination and a TYPE
//! filter, KEYS agreeing with the walk, multi-key DEL vs EXISTS, and
//! lazy expiry purging a short-TTL key out of both GET and SCAN. One
//! REAL node, RESP only (the SCAN wire shape had no proc-level pin).

mod common;
mod scenario_common;

use std::collections::BTreeSet;

use scenario_common::{front_world, r, rb, rf, FrontSpec};

/// Parse a SCAN full reply (`*2 [cursor-bulk, keys-array]`) into its
/// cursor and keys.
fn parse_scan(full: &str) -> (String, Vec<String>) {
    let toks: Vec<&str> = full.split("\r\n").collect();
    assert_eq!(toks.first(), Some(&"*2"), "scan reply head: {full:?}");
    let clen: usize = toks[1].strip_prefix('$').unwrap().parse().unwrap();
    assert_eq!(clen, toks[2].len(), "cursor bulk length");
    let n: usize = toks[3]
        .strip_prefix('*')
        .unwrap_or_else(|| panic!("keys array head: {full:?}"))
        .parse()
        .unwrap();
    let keys = toks[4..]
        .chunks(2)
        .take(n)
        .map(|pair| {
            let len: usize = pair[0].strip_prefix('$').unwrap().parse().unwrap();
            assert_eq!(len, pair[1].len(), "key bulk length: {full:?}");
            pair[1].to_string()
        })
        .collect();
    (toks[2].to_string(), keys)
}

/// Walk `SCAN .. MATCH <pat> [COUNT n]` until the cursor closes;
/// returns every visited key and how many round trips it took.
async fn scan_all(a: &str, pat: &str, count: &str, extra: &[&[u8]]) -> (BTreeSet<String>, usize) {
    let mut seen = BTreeSet::new();
    let mut cursor = "0".to_string();
    let mut rounds = 0;
    loop {
        let mut args: Vec<&[u8]> = vec![
            b"SCAN",
            cursor.as_bytes(),
            b"MATCH",
            pat.as_bytes(),
            b"COUNT",
            count.as_bytes(),
        ];
        args.extend_from_slice(extra);
        let reply = rf(a, &args).await;
        let (next, keys) = parse_scan(&reply);
        seen.extend(keys);
        rounds += 1;
        assert!(rounds < 50, "scan cursor never closes");
        if next == "0" {
            return (seen, rounds);
        }
        cursor = next;
    }
}

#[tokio::test]
async fn config_key_lifecycle_ttl_types_rename_scan_and_lazy_expiry() {
    let (_front, a) = front_world("keys36", &FrontSpec::default()).await;

    // a) TTL round trip: bare key has no TTL, EXPIRE arms one (floored
    //    to whole seconds), PERSIST clears it back to -1.
    ok(&r(&a, &[b"SET", b"cfg:v1", b"on"]).await);
    assert_eq!(int(&r(&a, &[b"TTL", b"cfg:v1"]).await), -1, "no ttl yet");
    assert_eq!(int(&r(&a, &[b"EXPIRE", b"cfg:v1", b"60"]).await), 1);
    let ttl = int(&r(&a, &[b"TTL", b"cfg:v1"]).await);
    assert!(ttl == 59 || ttl == 60, "floored ttl: {ttl}");
    assert_eq!(rb(&a, &[b"GET", b"cfg:v1"]).await, "on", "still readable");
    assert_eq!(int(&r(&a, &[b"PERSIST", b"cfg:v1"]).await), 1);
    assert_eq!(int(&r(&a, &[b"TTL", b"cfg:v1"]).await), -1, "persisted");

    // b) One key of every TYPE (string/hash/list/set/zset/stream/
    //    ReJSON-RL/vectorset); a missing key types as none.
    ok(&r(&a, &[b"SET", b"kt:str", b"v"]).await);
    assert_eq!(int(&r(&a, &[b"HSET", b"kt:hash", b"f", b"v"]).await), 1);
    assert_eq!(int(&r(&a, &[b"LPUSH", b"kt:list", b"v"]).await), 1);
    assert_eq!(int(&r(&a, &[b"SADD", b"kt:set", b"m"]).await), 1);
    assert_eq!(int(&r(&a, &[b"ZADD", b"kt:zset", b"1", b"m"]).await), 1);
    // A stream key is a FULL topic name (parent/child; hash tags are
    // rejected and a bare name round-robins onto an implicit child
    // queue). QUIRK pinned: the stream meta lives under the PARENT
    // topic's slot, so TYPE (routed by the key's own slot) answers
    // none -- while SCAN, whose classifier reads the physical record,
    // still lists the stream key.
    assert!(
        r(&a, &[b"XADD", b"ktstream/q0", b"*", b"f", b"v"]).await.contains(&b'-'),
        "stream entry minted"
    );
    ok(&r(&a, &[b"JSON.SET", b"kt:json", b"$", b"{}"]).await);
    assert_eq!(int(&r(&a, &[b"VADD", b"kt:vec", b"VALUES", b"2", b"1", b"1", b"0"]).await), 1);
    for (key, want) in [
        ("kt:str", "string"),
        ("kt:hash", "hash"),
        ("kt:list", "list"),
        ("kt:set", "set"),
        ("kt:zset", "zset"),
        
        ("kt:json", "ReJSON-RL"),
        ("kt:vec", "vectorset"),
    ] {
        assert_eq!(word(&r(&a, &[b"TYPE", key.as_bytes()]).await), want, "TYPE {key}");
    }
    assert_eq!(word(&r(&a, &[b"TYPE", b"kt:missing"]).await), "none", "missing key");
    assert_eq!(word(&r(&a, &[b"TYPE", b"ktstream/q0"]).await), "none", "stream TYPE quirk");

    // c) RENAME moves value AND ttl together on a same-slot pair; the
    //    old name is gone and the new name keeps the clock.
    ok(&r(&a, &[b"SET", b"{rot}old", b"payload"]).await);
    assert_eq!(int(&r(&a, &[b"EXPIRE", b"{rot}old", b"120"]).await), 1);
    ok(&r(&a, &[b"RENAME", b"{rot}old", b"{rot}new"]).await);
    assert_eq!(int(&r(&a, &[b"EXISTS", b"{rot}old"]).await), 0, "old name gone");
    assert_eq!(rb(&a, &[b"GET", b"{rot}new"]).await, "payload");
    let moved = int(&r(&a, &[b"TTL", b"{rot}new"]).await);
    assert!(moved == 119 || moved == 120, "ttl travelled: {moved}");

    // d) Ten prefixed keys; SCAN MATCH paginates them with COUNT 3
    //    (several round trips), walks to cursor "0", and sees exactly
    //    the same set as KEYS.
    let mut expect = BTreeSet::new();
    for i in 1..=5 {
        for half in ["a", "b"] {
            let k = format!("kc:{half}{i}");
            ok(&r(&a, &[b"SET", k.as_bytes(), b"v"]).await);
            expect.insert(k);
        }
    }
    let (walked, rounds) = scan_all(&a, "kc:*", "3", &[]).await;
    assert_eq!(walked, expect, "scan walk sees exactly the kc keys");
    assert!(rounds >= 2, "COUNT 3 paginated the 10 keys: {rounds} rounds");
    let keys_reply = rf(&a, &[b"KEYS", b"kc:*"]).await;
    for k in &expect {
        assert!(keys_reply.contains(k.as_str()), "KEYS lists {k}: {keys_reply:?}");
    }

    // e) TYPE filter: only the zset key answers under MATCH kt:*; a
    //    stream-type scan sees the stream key TYPE itself cannot.
    let (zsets, _) = scan_all(&a, "kt:*", "10", &[b"TYPE", b"zset"]).await;
    assert_eq!(zsets, BTreeSet::from(["kt:zset".to_string()]), "type filter");
    let (streams, _) = scan_all(&a, "ktstream/*", "10", &[b"TYPE", b"stream"]).await;
    assert_eq!(streams, BTreeSet::from(["ktstream/q0".to_string()]), "scan sees streams");

    // f) Multi-key DEL/EXISTS are slot-checked like every multi-key
    //    verb (plain kc:a1 + kc:a2 would CROSSSLOT), so the victims
    //    share a hash tag; DEL counts real victims only (a missing
    //    name is not one) and EXISTS drops to zero for both.
    ok(&r(&a, &[b"SET", b"{kc}d1", b"v"]).await);
    ok(&r(&a, &[b"SET", b"{kc}d2", b"v"]).await);
    assert_eq!(int(&r(&a, &[b"DEL", b"{kc}d1", b"{kc}d2", b"{kc}zz"]).await), 2);
    assert_eq!(int(&r(&a, &[b"EXISTS", b"{kc}d1", b"{kc}d2"]).await), 0);
    let (after, _) = scan_all(&a, "kc:*", "10", &[]).await;
    assert_eq!(after.len(), 10, "ten kc keys remain");

    // g) Lazy expiry: a 1s key dies on its own clock; the read purges
    //    it so neither GET nor SCAN can see it afterwards.
    ok(&r(&a, &[b"SET", b"kc:eph", b"v", b"EX", b"1"]).await);
    assert_eq!(int(&r(&a, &[b"TTL", b"kc:eph"]).await), 1);
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    assert_eq!(rb(&a, &[b"GET", b"kc:eph"]).await, "", "expired read is nil");
    assert_eq!(int(&r(&a, &[b"EXISTS", b"kc:eph"]).await), 0);
    let (post, _) = scan_all(&a, "kc:eph", "10", &[]).await;
    assert!(post.is_empty(), "lazy purge removed it from scans: {post:?}");
}

fn ok(reply: &[u8]) {
    assert!(reply.starts_with(b"+OK"), "{reply:?}");
}

fn int(reply: &[u8]) -> i64 {
    let s = String::from_utf8_lossy(reply);
    s.trim().trim_start_matches(':').parse().expect("int line")
}

fn word(reply: &[u8]) -> String {
    let s = String::from_utf8_lossy(reply);
    s.trim().trim_start_matches('+').trim().to_string()
}
