//! Multi-store scenario #135 -- a CRM profile merge: JSON.FORGET retires
//! the duplicate profile's legacy-import block (JSON.DEL alias: :1, then
//! :0, missing key :0, static paths only); the profiles' look-alike
//! embeddings live in a vector set (VADD VALUES / FP16, VSETATTR); VSIM
//! WITHATTRIBS WITHSCORES ranks the duplicates and FT.SEARCH KNN (FP16
//! blob, city prefilter, before and after FT.BUILD) agrees with it, and
//! MySQL `profiles` mirrors the merge (legacy cleared, attrs from SQL).
//! RESP (JSON + vector set + FT) + MySQL. One REAL node, MySQL front.
//! Deviations: VADD has no inline attribute syntax -- extra tokens read
//! as vector values (src/command/vectorset_cmd.rs:80-120); VSIM on a
//! missing key is an error, not nil (vectorset_sim.rs:36-43); attribs
//! precede scores whatever the flag order (vectorset_sim.rs:135).

use crate::common;
use crate::scenario_common;
use common::mysql::{aff, connect_root, ddl, ints, triples};
use scenario_common::{front_world, int_of, r, rb, rt, FrontSpec};
use serde_json::Value as J;
use std::time::Duration;

const K: &[u8] = b"profiles:vec";
const IDX: &[u8] = b"profiles_idx";

/// Profile embeddings: sign flips of the all-ones query. k flips give
/// cos = (4 - 2k) / 4 and L2 = 2 * sqrt(k): equal norms, so the VSIM
/// (cosine) and KNN (L2) rankings agree; p3 / p6 tie at k = 2 and
/// break by element bytes asc.
const VEC: [[f32; 4]; 6] = [
    [1.0, 1.0, 1.0, 1.0],
    [-1.0, 1.0, 1.0, 1.0],
    [1.0, 1.0, -1.0, -1.0],
    [-1.0, -1.0, -1.0, 1.0],
    [-1.0, -1.0, -1.0, -1.0],
    [-1.0, -1.0, 1.0, 1.0],
];
const NAME: [&str; 6] = ["ana", "bruno", "carla", "daniel", "elena", "filip"];
const CITY: [&str; 6] = ["berlin", "lisbon", "berlin", "lisbon", "berlin", "lisbon"];

fn elem(i: usize) -> String {
    format!("p{}", i + 1)
}

fn sv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn key(i: usize) -> String {
    format!("profile:{}", i + 1)
}

fn flips(v: &[f32; 4]) -> usize {
    v.iter().filter(|x| **x < 0.0).count()
}

/// VSIM score `(cos + 1) / 2` as the server prints it (`{}`).
fn vsim_score(k: usize) -> String {
    format!("{}", 1.0 - k as f64 / 4.0)
}

/// KNN score `1 / (1 + L2)` as the server prints it (`{:.4}`).
fn knn_score(k: usize) -> String {
    format!("{:.4}", 1.0 / (1.0 + (4.0 * k as f64).sqrt()))
}

/// Expected neighbour order (score desc, element asc), independent of
/// the server's reply: (element, vsim score, knn score).
fn ranked() -> Vec<(String, String, String)> {
    let mut rows: Vec<(String, usize)> = (0..6).map(|i| (elem(i), flips(&VEC[i]))).collect();
    rows.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    rows.into_iter()
        .map(|(e, k)| (e, vsim_score(k), knn_score(k)))
        .collect()
}

/// IEEE binary16 of a finite normal f32 (round-to-nearest-even), as
/// the little-endian wire bytes.
fn f16_bytes(x: f32) -> [u8; 2] {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let e = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    assert!((1..0x1F).contains(&e), "test vectors are binary16 normals");
    let frac = bits & 0x7F_FFFF;
    let mut h = ((e as u32) << 10) | (frac >> 13);
    let rem = frac & 0x1FFF;
    if rem > 0x1000 || (rem == 0x1000 && h & 1 == 1) {
        h += 1;
    }
    (sign | h as u16).to_le_bytes()
}

fn f16_blob(v: &[f32; 4]) -> Vec<u8> {
    v.iter().flat_map(|x| f16_bytes(*x)).collect()
}

/// Full-drain reply text of one array command (short quiet gap).
async fn full(a: &str, args: &[&[u8]]) -> String {
    let raw = common::lite::cmd_full_reply_wait(a, common::TOKEN, args, 30, 10_000).await;
    let text = common::lite::text(&raw);
    assert!(!text.starts_with('<'), "{}: {text:?}", String::from_utf8_lossy(args[0]));
    text
}

/// Bulk payloads of a flat reply in order (`<nil>` for a null bulk);
/// array headers and integer lines are skipped.
fn bulks(reply: &str) -> Vec<String> {
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < toks.len() {
        if let Some(len) = toks[i].strip_prefix('$') {
            if len == "-1" {
                out.push("<nil>".to_string());
            } else {
                out.push(toks.get(i + 1).copied().unwrap_or("").to_string());
                i += 1;
            }
        }
        i += 1;
    }
    out
}

/// The `:N` integer that follows `label` in an FT.INFO reply.
fn int_after(reply: &str, label: &str) -> i64 {
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let at = toks
        .iter()
        .position(|t| *t == label)
        .unwrap_or_else(|| panic!("no {label}: {reply:?}"));
    int_of(toks[at + 1].as_bytes())
}

/// `VSIM <K> <tail..>` -> the flat bulk list.
async fn vsim(a: &str, tail: &[&[u8]]) -> Vec<String> {
    let mut args: Vec<&[u8]> = vec![b"VSIM", K];
    args.extend_from_slice(tail);
    bulks(&full(a, &args).await)
}

/// `FT.SEARCH <IDX> <query> <opts..>` -> (hit total, bulk list). The
/// reply is one array `*N` whose first element is `:total`, so N is
/// 1 + the number of bulk items that follow (NOCONTENT shapes only).
async fn search(a: &str, query: &str, opts: &[&[u8]]) -> (i64, Vec<String>) {
    let mut args: Vec<&[u8]> = vec![b"FT.SEARCH", IDX, query.as_bytes()];
    args.extend_from_slice(opts);
    let text = full(a, &args).await;
    let mut lines = text.split("\r\n");
    let head = lines.next().unwrap_or("");
    let total = int_of(lines.next().unwrap_or("").as_bytes());
    let items = bulks(&text);
    assert_eq!(head, format!("*{}", 1 + items.len()), "{text:?}");
    (total, items)
}

#[tokio::test]
async fn json_forget_vector_attrs_and_fp16_search() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("attrs135", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE profiles (user_id INT PRIMARY KEY, name VARCHAR(32), city VARCHAR(32), legacy VARCHAR(32))").await;
    for i in 0..6 {
        let doc = format!(
            "{{\"user_id\":{},\"name\":\"{}\",\"city\":\"{}\",\"vip\":{},\
             \"legacy\":{{\"source\":\"crm-old-{}\",\"tags\":[\"beta\"]}}}}",
            i + 1,
            NAME[i],
            CITY[i],
            i == 0,
            i + 1
        );
        assert_eq!(rt(&a, &[b"JSON.SET", key(i).as_bytes(), b"$", doc.as_bytes()]).await, "+OK");
        let sql = format!("INSERT INTO profiles VALUES ({}, '{}', '{}', 'crm-old-{}')", i + 1, NAME[i], CITY[i], i + 1);
        assert_eq!(aff(&mut c, &sql).await, 1, "{sql}");
    }

    // a) Profile 1 was a legacy-import duplicate: JSON.FORGET retires its
    //    legacy block (:1, then :0); a missing key is :0; wildcard paths
    //    are refused; the rest of the document is untouched.
    let (k1, k2) = (key(0), key(1));
    assert_eq!(rt(&a, &[b"JSON.FORGET", k1.as_bytes(), b"$.legacy"]).await, ":1");
    assert_eq!(rt(&a, &[b"JSON.FORGET", k1.as_bytes(), b"$.legacy"]).await, ":0");
    assert_eq!(rt(&a, &[b"JSON.FORGET", b"profile:99", b"$.legacy"]).await, ":0");
    assert_eq!(rt(&a, &[b"JSON.FORGET", k2.as_bytes(), b"$..legacy"]).await, "-ERR wrong static path");
    let doc: J = serde_json::from_str(&rb(&a, &[b"JSON.GET", k1.as_bytes()]).await).unwrap();
    let obj = doc.as_object().unwrap();
    assert!(obj.get("legacy").is_none(), "{doc}");
    assert_eq!(obj.len(), 4, "user_id / name / city / vip survive: {doc}");
    assert_eq!(obj["vip"], J::Bool(true));
    assert_eq!(obj["name"], "ana");
    assert_eq!(rb(&a, &[b"JSON.GET", k1.as_bytes(), b"$.legacy"]).await, "", "forgotten path -> nil");
    assert_eq!(rb(&a, &[b"JSON.GET", k2.as_bytes(), b"$.legacy.source"]).await, "\"crm-old-2\"");
    assert_eq!(rt(&a, &[b"JSON.FORGET", k2.as_bytes(), b"$.legacy.tags[0]"]).await, ":1");
    assert_eq!(rb(&a, &[b"JSON.GET", k2.as_bytes(), b"$.legacy.tags"]).await, "[]");
    assert_eq!(aff(&mut c, "UPDATE profiles SET legacy = NULL WHERE user_id = 1").await, 1);
    // JSON.DEL is the same handler (src/command/json_cmd.rs:223): the
    // alias retires bruno's whole legacy block, mirrored in SQL too.
    assert_eq!(rt(&a, &[b"JSON.DEL", k2.as_bytes(), b"$.legacy"]).await, ":1", "JSON.DEL alias");
    assert_eq!(rb(&a, &[b"JSON.GET", k2.as_bytes(), b"$.legacy"]).await, "", "retired by the alias");
    assert_eq!(aff(&mut c, "UPDATE profiles SET legacy = NULL WHERE user_id = 2").await, 1);

    // b) The six embeddings: p1..p5 as VALUES, p6 through the binary16
    //    blob; re-adding p1 keeps its slot (:0); the set has no inline
    //    attribute syntax and validates both blob spellings.
    for (i, v) in VEC.iter().enumerate().take(5) {
        let (e, nums) = (elem(i), v.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        let mut args: Vec<&[u8]> = vec![b"VADD", K, b"VALUES", b"4", e.as_bytes()];
        args.extend(nums.iter().map(|s| s.as_bytes()));
        assert_eq!(rt(&a, &args).await, ":1", "VADD {e}");
    }
    let blob6 = f16_blob(&VEC[5]);
    assert_eq!(blob6, vec![0x00, 0xBC, 0x00, 0xBC, 0x00, 0x3C, 0x00, 0x3C], "f16: -1 = 00bc, 1 = 003c (LE)");
    assert_eq!(rt(&a, &[b"VADD", K, b"FP16", b"4", b"p6", blob6.as_slice()]).await, ":1");
    assert_eq!(
        rt(&a, &[b"VADD", K, b"VALUES", b"4", b"p1", b"1", b"1", b"1", b"1"]).await,
        ":0",
        "re-add overwrites the vector, count unchanged"
    );
    assert_eq!(int_of(&r(&a, &[b"VCARD", K]).await), 6);
    assert_eq!(int_of(&r(&a, &[b"VDIM", K]).await), 4);
    assert_eq!(
        rt(&a, &[b"VADD", K, b"VALUES", b"4", b"p7", b"1", b"1", b"1", b"1", b"name=x"]).await,
        "-ERR invalid vector value",
        "no inline SETATTR: the extra token counts as a 5th value"
    );
    assert_eq!(rt(&a, &[b"VADD", K, b"FP16", b"4", b"p7", &blob6[..2]]).await, "-ERR invalid FP16 vector");
    assert_eq!(
        rt(&a, &[b"VADD", K, b"VALUES", b"8", b"p7", b"1", b"1", b"1", b"1", b"1", b"1", b"1", b"1"]).await,
        "-ERR dimension mismatch"
    );
    assert_eq!(int_of(&r(&a, &[b"VCARD", K]).await), 6, "refused adds leave no trace");

    // c) Attributes are stamped from the SQL mirror (name + city) for
    //    every profile except p4 (not yet enriched); unknown elements
    //    answer VSETATTR :0 / VGETATTR nil.
    let rows = triples(&mut c, "SELECT user_id, name, city FROM profiles ORDER BY user_id").await;
    assert_eq!(rows.len(), 6);
    for (id, name, city) in rows.iter().filter(|(id, _, _)| *id != 4) {
        let attr = format!("name={name};city={city}");
        let e = elem(*id as usize - 1);
        assert_eq!(rt(&a, &[b"VSETATTR", K, e.as_bytes(), attr.as_bytes()]).await, ":1", "{e}");
    }
    assert_eq!(rb(&a, &[b"VGETATTR", K, b"p2"]).await, "name=bruno;city=lisbon");
    assert_eq!(rt(&a, &[b"VSETATTR", K, b"p99", b"x"]).await, ":0");
    assert_eq!(rb(&a, &[b"VGETATTR", K, b"p99"]).await, "", "unknown element -> nil");
    let want = ranked();
    assert_eq!(
        want.iter().map(|w| w.0.as_str()).collect::<Vec<_>>(),
        sv(&["p1", "p2", "p3", "p6", "p4", "p5"]),
        "independent ranking"
    );

    // d) Duplicate lookup: VSIM ranks the merged profile first and its
    //    nearest look-alike p2 second; attribs precede scores whatever
    //    the flag order (p4 without one reads as nil); COUNT trims; the
    //    binary16 query blob gives the same ranking; a missing key is an
    //    error, not nil.
    let t = scenario_common::perf::now();
    let q = ["1", "1", "1", "1"];
    let mut plain: Vec<&[u8]> = vec![b"VALUES"];
    plain.extend(q.iter().map(|s| s.as_bytes()));
    let mut both: Vec<&[u8]> = vec![b"WITHSCORES", b"WITHATTRIBS"];
    both.extend_from_slice(&plain);
    let mut swapped: Vec<&[u8]> = vec![b"WITHATTRIBS", b"WITHSCORES"];
    swapped.extend_from_slice(&plain);
    let flat = vsim(&a, &both).await;
    let got: Vec<(String, String)> = flat.chunks(3).map(|c| (c[0].clone(), c[2].clone())).collect();
    let expect: Vec<(String, String)> = want.iter().map(|(e, s, _)| (e.clone(), s.clone())).collect();
    assert_eq!(got, expect, "VSIM order + (cos+1)/2 scores: {flat:?}");
    assert_eq!(flat[1], "name=ana;city=berlin", "attr before score: {flat:?}");
    assert_eq!(flat[4], "name=bruno;city=lisbon");
    assert_eq!(flat[13], "<nil>", "p4 has no attribute: {flat:?}");
    assert_eq!(vsim(&a, &swapped).await, flat, "flag order does not move the layout");
    let mut top: Vec<&[u8]> = vec![b"COUNT", b"3"];
    top.extend_from_slice(&plain);
    assert_eq!(vsim(&a, &top).await, sv(&["p1", "p2", "p3"]), "COUNT trims the ranking");
    let qb = f16_blob(&[1.0; 4]);
    let via_fp16 = vsim(&a, &[b"WITHSCORES", b"FP16", qb.as_slice()]).await;
    assert_eq!(via_fp16, got.iter().flat_map(|(e, s)| [e.clone(), s.clone()]).collect::<Vec<_>>());
    assert_eq!(vsim(&a, &plain).await, want.iter().map(|w| w.0.clone()).collect::<Vec<_>>());
    assert_eq!(rt(&a, &[b"VSIM", b"nope:such", b"VALUES", b"1", b"1", b"1", b"1"]).await, "-ERR vector set does not exist");
    assert_eq!(rt(&a, &[b"VSIM", K, b"COUNT", b"x", b"VALUES", b"1", b"1", b"1", b"1"]).await, "-ERR invalid COUNT");

    // e) The search page's index agrees: KNN over the binary16 blob
    //    ranks exactly like VSIM and WITHSCORES prints 1/(1+L2); the
    //    city prefilter narrows the candidates; a short blob is refused;
    //    FT.BUILD switches SPANN on (FT.INFO ann_built 0 -> 1, two
    //    centroids) without changing the answer.
    assert_eq!(
        rt(&a, &[b"FT.CREATE", IDX, b"SCHEMA", b"city", b"KEYWORD", b"name", b"TEXT", b"v", b"VECTOR", b"DIM", b"4"]).await,
        "+OK"
    );
    for (i, v) in VEC.iter().enumerate() {
        let nums: Vec<String> = v.iter().map(|x| x.to_string()).collect();
        let doc = format!("{{\"city\":\"{}\",\"name\":\"{}\",\"v\":[{}]}}", CITY[i], NAME[i], nums.join(","));
        assert_eq!(rt(&a, &[b"FT.ADD", IDX, elem(i).as_bytes(), doc.as_bytes()]).await, ":1");
    }
    let knn6: Vec<&[u8]> = vec![b"NOCONTENT", b"KNN", b"6", b"@v", b"FP16", qb.as_slice()];
    let (total, ids) = search(&a, "*", &knn6).await;
    assert_eq!(
        (total, ids.clone()),
        (6, want.iter().map(|w| w.0.clone()).collect::<Vec<String>>()),
        "KNN order == VSIM order"
    );
    let mut scored: Vec<&[u8]> = vec![b"NOCONTENT", b"WITHSCORES"];
    scored.extend_from_slice(&knn6[1..]);
    let (_, pairs) = search(&a, "*", &scored).await;
    let got: Vec<(String, String)> = pairs.chunks(2).map(|c| (c[0].clone(), c[1].clone())).collect();
    let expect: Vec<(String, String)> = want.iter().map(|(e, _, s)| (e.clone(), s.clone())).collect();
    assert_eq!(got, expect, "KNN scores 1/(1+L2): {pairs:?}");
    let (total, lisbon) = search(&a, "@city:lisbon", &[b"NOCONTENT", b"KNN", b"2", b"@v", b"VALUES", b"1", b"1", b"1", b"1"]).await;
    assert_eq!((total, lisbon), (2, sv(&["p2", "p6"])), "Lisbon prefilter, k = 2");
    let (total, berlin) = search(&a, "@city:berlin", &[b"NOCONTENT", b"KNN", b"10", b"@v", b"VALUES", b"1", b"1", b"1", b"1"]).await;
    assert_eq!((total, berlin), (3, sv(&["p1", "p3", "p5"])), "Berlin prefilter");
    assert_eq!(int_after(&full(&a, &[b"FT.INFO", IDX]).await, "ann_built"), 0);
    assert_eq!(rt(&a, &[b"FT.BUILD", IDX, b"K", b"2", b"SEED", b"7"]).await, "+OK");
    let info = full(&a, &[b"FT.INFO", IDX]).await;
    assert_eq!((int_after(&info, "ann_built"), int_after(&info, "ann_centroids")), (1, 2), "{info}");
    let (_, via_spann) = search(&a, "*", &knn6).await;
    assert_eq!(via_spann, ids, "SPANN path == brute-force scan");
    assert_eq!(rt(&a, &[b"FT.SEARCH", b"nope_idx", b"*"]).await, "-ERR unknown index");
    assert_eq!(rt(&a, &[b"FT.SEARCH", IDX, b"*", b"NOCONTENT", b"KNN", b"6", b"@v", b"FP16", &qb[..2]]).await, "-ERR invalid FP16 vector");
    scenario_common::perf::within("attrs135/vector-search", t, Duration::from_secs(2));

    // f) Cross-face: profiles still carrying a legacy source (JSON
    //    `$.legacy.source` present) == SQL `legacy IS NOT NULL`; the
    //    merged profile's attribute carries the merge mark on the
    //    vector face.
    let mut json_legacy = 0;
    for i in 0..6 {
        if !rb(&a, &[b"JSON.GET", key(i).as_bytes(), b"$.legacy.source"]).await.is_empty() {
            json_legacy += 1;
        }
    }
    assert_eq!(json_legacy, 4, "two profiles retired (FORGET + DEL alias)");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM profiles WHERE legacy IS NOT NULL").await, vec![json_legacy]);
    assert_eq!(rt(&a, &[b"VSETATTR", K, b"p1", b"name=ana;city=berlin;merged=1"]).await, ":1");
    assert_eq!(rb(&a, &[b"VGETATTR", K, b"p1"]).await, "name=ana;city=berlin;merged=1");
    let mut merged: Vec<&[u8]> = vec![b"WITHATTRIBS", b"COUNT", b"1"];
    merged.extend_from_slice(&plain);
    assert_eq!(vsim(&a, &merged).await, sv(&["p1", "name=ana;city=berlin;merged=1"]));
    scenario_common::perf::scenario("attrs135", t0);
}
