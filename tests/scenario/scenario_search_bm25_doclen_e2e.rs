//! Multi-store scenario #161 -- a legal-discovery desk ranks its matters with the RESP
//! full-text engine (BM25 over title+summary, NUMERIC fee, KEYWORD status) and reconciles
//! the ranking with the MySQL matters register: same-tf documents score by length (m1
//! above m2, both equal to the recomputed BM25), a rarer term outscores a common one
//! at equal tf, implicit AND sums the term scores, `|` unions, a NUMERIC range adds the
//! CONSTANT 1.0, K1/B retune the index (B=0 erases the length penalty), LIMIT pages
//! while SORTBY counts the full match set, FT.INFO echoes the corpus stats, shortlist /
//! budget are derived on BOTH faces, and closing m1 (UPDATE + FT.DEL) re-ranks m2.
//! FT (CREATE/ADD/DEL/INFO/SEARCH) + MySQL. One REAL node, fronts mysql (RESP always).
//! Deviations: a FT.SEARCH reply is ONE flat array whose `:total` is the PAGE size of a plain
//! search (RediSearch: full match count; src/search/ft_search.rs:181), the full count only under
//! SORTBY (ft_search.rs:232-259); a NUMERIC range node scores the constant 1.0, not BM25 (eval_tree.rs:198-208).

use crate::common;
use crate::scenario_common;
use common::mysql::{aff, connect_root, ddl, ints, one as sql_one, run};
use mysql_async::Value as MVal;
use scenario_common::{bulk_at, front_world, int_of, json_of, r, FrontSpec};
use std::time::Duration;

const IDX: &str = "matters161";
/// The desk's B=0 twin over the same six documents.
const IDX2: &str = "matters161b";
/// `(title, summary, fee, status)` of the six matters. m1 and m2 carry
/// `contract` once in the title (same tf, same df) and m2's summary is
/// the filler that makes it the longer document; only m1 carries
/// `breach`. m3/m4 tie on the top fee; m5 sits below the range.
const MATTERS: [(&str, &str, i64, &str); 6] = [
    ("contract breach review", "vendor agreement audit for the harbor project", 4500, "open"),
    (
        "contract review",
        "memo about filing calendars and scheduling notes for the quarterly dossier of the harbor vendor desk with annexes and attachments",
        1200,
        "open",
    ),
    ("lease dispute", "tenant filed a notice", 9000, "open"),
    ("invoice appeal", "vendor asked for relief", 9000, "closed"),
    ("patent claim", "filing fee schedule", 500, "open"),
    ("staff handbook", "policy update", 5000, "closed"),
];
const SCHEMA: [&str; 9] = ["SCHEMA", "title", "TEXT", "summary", "TEXT", "fee", "NUMERIC", "status", "KEYWORD"];
/// `(k1, b)` of each index: the engine defaults and the desk's twin.
const TUNING: [(&str, f64, f64); 2] = [(IDX, 1.2, 0.75), (IDX2, 0.6, 0.0)];
const RANGE: &str = "@fee:[1000 5000]";

fn mid(i: usize) -> String {
    format!("m{}", i + 1)
}

/// Indexed JSON of matter `i` (the FT document of both indexes).
fn doc_json(i: usize) -> String {
    let (title, summary, fee, status) = MATTERS[i];
    format!(r#"{{"title":"{title}","summary":"{summary}","fee":{fee},"status":"{status}"}}"#)
}

/// Lowercase alphanumeric runs (the indexer's Latin tokenizer).
fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Text of TEXT field `field` (0 = title, 1 = summary) in matter `i`.
fn field_text(i: usize, field: usize) -> &'static str {
    if field == 0 { MATTERS[i].0 } else { MATTERS[i].1 }
}

/// tf of `term` in matter `i`'s field `field` (postings are per field).
fn tf(i: usize, field: usize, term: &str) -> u64 {
    tokens(field_text(i, field)).iter().filter(|t| t.as_str() == term).count() as u64
}

/// doclen = tokens of every TEXT field combined (ft_cmd.rs:272).
fn doclen(i: usize) -> u64 {
    (0..2).map(|f| tokens(field_text(i, f)).len() as u64).sum()
}

fn has(i: usize, term: &str) -> bool {
    (0..2).any(|f| tf(i, f, term) > 0)
}

/// idf = ln(1 + (n - df + 0.5) / (df + 0.5)) (bm25.rs:15-18).
fn idf(df: u64, n: u64) -> f64 {
    let (df, n) = (df.max(1) as f64, n.max(1) as f64);
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

/// One field's term contribution (bm25.rs:26-30), same expression order.
fn term_score(tf: u64, df: u64, doclen: u64, avgdl: f64, n: u64, k1: f64, b: f64) -> f64 {
    let tf = tf as f64;
    let norm = 1.0 - b + b * (doclen.max(1) as f64) / avgdl.max(1e-9);
    idf(df, n) * (tf * (k1 + 1.0)) / (tf + k1 * norm)
}

/// BM25 of `term` for matter `doc` over the `live` corpus: per-field df,
/// summed title-then-summary (eval_tree.rs:225-255); n = live docs,
/// avgdl = sum(doclen) / n (eval_tree.rs:50-57).
fn bm25(term: &str, doc: usize, live: &[usize], k1: f64, b: f64) -> f64 {
    let n = live.len() as u64;
    let avgdl = live.iter().map(|&i| doclen(i)).sum::<u64>() as f64 / n as f64;
    (0..2).fold(0.0, |acc, f| {
        let df = live.iter().filter(|&&i| tf(i, f, term) > 0).count() as u64;
        let tf = tf(doc, f, term);
        if df == 0 || tf == 0 {
            return acc;
        }
        acc + term_score(tf, df, doclen(doc), avgdl, n, k1, b)
    })
}

/// Score cell as the server prints it (ft_search.rs:126).
fn cell4(score: f64) -> String {
    format!("{score:.4}")
}

fn argv<'a>(args: &[&'a str]) -> Vec<&'a [u8]> {
    args.iter().map(|t| t.as_bytes()).collect()
}

/// One-shot reply text (status / error / integer).
async fn one(a: &str, args: &[&str]) -> String {
    common::lite::text(&r(a, &argv(args)).await)
}

/// Full-drain reply text with a short quiet gap (the fixture's `rf`
/// parks 250ms per call).
async fn drain(a: &str, args: &[&str]) -> String {
    let raw = common::lite::cmd_full_reply_wait(a, common::TOKEN, &argv(args), 20, 10_000).await;
    common::lite::text(&raw)
}

/// `FT.SEARCH <idx> <args...>` reply text.
async fn search(a: &str, idx: &str, args: &[&str]) -> String {
    let mut full = vec!["FT.SEARCH", idx];
    full.extend_from_slice(args);
    drain(a, &full).await
}

/// Element count of a reply's leading `*N` array header.
fn array_len(reply: &str) -> usize {
    reply
        .split("\r\n")
        .next()
        .and_then(|l| l.strip_prefix('*'))
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("no array header in {reply}"))
}

/// The `:total` header of a FT.SEARCH reply.
fn total_of(reply: &str) -> usize {
    reply
        .lines()
        .find_map(|l| l.strip_prefix(':').and_then(|t| t.parse().ok()))
        .unwrap_or_else(|| panic!("no total in {reply}"))
}

/// Bulks of hit `h` in a flat reply carrying `per` cells per hit.
fn hit(reply: &str, per: usize, h: usize) -> Vec<String> {
    (0..per).map(|k| bulk_at(reply.as_bytes(), per * h + k)).collect()
}

fn ids(reply: &str, per: usize) -> Vec<String> {
    (0..hits(reply, per)).map(|h| hit(reply, per, h)[0].clone()).collect()
}

/// Hits on the page: the array header counts the page, the `:total`
/// header may count the whole match set (SORTBY).
fn hits(reply: &str, per: usize) -> usize {
    (array_len(reply) - 1) / per
}

/// Doc ids of the matters satisfying `pred`, ascending.
fn pick(pred: impl Fn(usize) -> bool) -> Vec<String> {
    (0..MATTERS.len()).filter(|&i| pred(i)).map(mid).collect()
}

/// Numeric part of a doc id (`m3` -> 3).
fn number_of(id: &str) -> i64 {
    id.trim_start_matches('m').parse().expect("matter number")
}

/// `* SORTBY fee DESC` re-derived: fee descending, doc id ascending on ties.
fn top_fees(n: usize) -> Vec<String> {
    let mut idx: Vec<usize> = (0..MATTERS.len()).collect();
    idx.sort_by(|&x, &y| MATTERS[y].2.cmp(&MATTERS[x].2).then_with(|| mid(x).cmp(&mid(y))));
    idx.into_iter().take(n).map(mid).collect()
}

/// Text of a non-NULL SQL cell (ints arrive as bytes on the text protocol).
fn cell(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        other => panic!("unexpected SQL cell {other:?}"),
    }
}

/// The `:N` integer that follows `label` in an FT.INFO reply.
fn int_after(reply: &str, label: &str) -> i64 {
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let at = toks.iter().position(|t| *t == label).unwrap_or_else(|| panic!("no {label}: {reply}"));
    int_of(toks[at + 1].as_bytes())
}

#[tokio::test]
async fn search_bm25_doclen() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("bm25doclen", &spec).await;

    // a) Two indexes over the SAME six documents: the engine defaults
    //    (K1 1.2 / B 0.75) and the desk's `K1 0.6 B 0` twin.
    for (idx, _, _) in TUNING {
        let mut create = vec!["FT.CREATE", idx];
        if idx == IDX2 {
            create.extend_from_slice(&["K1", "0.6", "B", "0"]);
        }
        create.extend_from_slice(&SCHEMA);
        assert_eq!(one(&a, &create).await, "+OK", "create {idx}");
    }
    for i in 0..MATTERS.len() {
        for (idx, _, _) in TUNING {
            let added = int_of(&r(&a, &argv(&["FT.ADD", idx, &mid(i), &doc_json(i)])).await);
            assert_eq!(added, 1, "ft.add {} into {idx}", mid(i));
        }
    }
    let all: Vec<usize> = (0..MATTERS.len()).collect();

    // b) Hot block: the ranking round trips + the two register queries.
    let t = scenario_common::perf::now();
    let contract = search(&a, IDX, &["contract", "WITHSCORES"]).await;
    let breach = search(&a, IDX, &["breach", "WITHSCORES"]).await;
    let both = search(&a, IDX, &["contract breach", "WITHSCORES"]).await;
    let union = search(&a, IDX, &["contract | breach", "WITHSCORES"]).await;
    let range = search(&a, IDX, &[RANGE, "WITHSCORES"]).await;
    let ranged = search(&a, IDX, &[&format!("contract {RANGE}"), "WITHSCORES"]).await;
    let flat = search(&a, IDX2, &["contract", "WITHSCORES"]).await;
    let top = search(&a, IDX, &["*", "SORTBY", "fee", "DESC", "LIMIT", "0", "2"]).await;
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE matters (id INT PRIMARY KEY, code VARCHAR(16) NOT NULL, \
                 status VARCHAR(8) NOT NULL, fee BIGINT NOT NULL)").await;
    let mut values = String::new();
    for (i, (_, _, fee, status)) in MATTERS.iter().enumerate() {
        values.push_str(&format!("{}({}, '{}', '{status}', {fee})", if i > 0 { "," } else { "" }, i + 1, mid(i)));
    }
    run(&mut c, &format!("INSERT INTO matters (id, code, status, fee) VALUES {values}")).await;
    let open_ids = ints(&mut c, "SELECT id FROM matters WHERE status = 'open' ORDER BY id").await;
    let short: Vec<i64> = ids(&contract, 3)
        .iter()
        .map(|id| number_of(id))
        .filter(|n| open_ids.contains(n))
        .collect();
    let listed = short.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
    let sum = sql_one(&mut c, &format!("SELECT SUM(fee) FROM matters WHERE id IN ({listed})")).await;
    let keyword = search(&a, IDX, &["@status:open", "NOCONTENT"]).await;
    scenario_common::perf::within("bm25/8-search+2-sql", t, Duration::from_secs(2));

    // c) Length normalization: ONE flat array, m1 above m2, every score
    //    equal to the Rust BM25; the rarer term scores higher at equal tf.
    assert_eq!(array_len(&contract), 1 + 2 * 3, "flat WITHSCORES array: {contract}");
    assert_eq!(ids(&contract, 3), pick(|i| has(i, "contract")), "contract hits in rank order");
    let s1 = bm25("contract", 0, &all, 1.2, 0.75);
    let s2 = bm25("contract", 1, &all, 1.2, 0.75);
    assert_eq!(hit(&contract, 3, 0)[1], cell4(s1), "m1 score {contract}");
    assert_eq!(hit(&contract, 3, 1)[1], cell4(s2), "m2 score {contract}");
    assert_eq!(tf(0, 0, "contract"), tf(1, 0, "contract"), "same tf by construction");
    assert!(doclen(1) > 2 * doclen(0) && s1 > s2, "the longer same-tf doc ranks lower");
    assert_eq!(hit(&contract, 3, 0)[2], doc_json(0), "content is the stored document");
    let b1 = bm25("breach", 0, &all, 1.2, 0.75);
    assert_eq!(ids(&breach, 3), pick(|i| has(i, "breach")), "breach hits");
    assert_eq!(hit(&breach, 3, 0)[1], cell4(b1), "m1 breach score {breach}");
    assert!(b1 > s1, "rarer term (df 1 < df 2) scores higher at the same tf");

    // d) B=0 twin: the same-tf docs score IDENTICALLY (length-blind),
    //    and m1's score moved away from the default-constants value.
    assert_eq!(ids(&flat, 3), pick(|i| has(i, "contract")), "twin hits");
    let f1 = bm25("contract", 0, &all, 0.6, 0.0);
    assert_eq!(hit(&flat, 3, 0)[1], cell4(f1), "m1 under B=0 {flat}");
    assert_eq!(hit(&flat, 3, 1)[1], cell4(f1), "m2 under B=0 {flat}");
    assert_ne!(cell4(f1), cell4(s1), "K1/B retune moved m1's score");

    // e) Implicit AND sums the two term scores; `|` unions the postings.
    assert_eq!(ids(&both, 3), pick(|i| has(i, "contract") && has(i, "breach")), "AND hits");
    let and_score: f64 = hit(&both, 3, 0)[1].parse().unwrap();
    let (c1, b1_cell): (f64, f64) = (hit(&contract, 3, 0)[1].parse().unwrap(), hit(&breach, 3, 0)[1].parse().unwrap());
    assert!((and_score - c1 - b1_cell).abs() <= 0.0002, "AND = contract + breach ({and_score})");
    assert_eq!(hit(&both, 3, 0)[1], cell4(bm25("contract", 0, &all, 1.2, 0.75) + bm25("breach", 0, &all, 1.2, 0.75)), "AND score {both}");
    let either = pick(|i| has(i, "contract") || has(i, "breach"));
    assert_eq!((total_of(&union), ids(&union, 3)), (either.len(), either), "union = docs with either term");
    assert_eq!(hit(&union, 3, 0)[1], cell4(s1 + b1), "union sums the member scores {union}");

    // f) NUMERIC range: the constant 1.0000 alone, bm25 + 1.0 next to a term.
    assert_eq!(ids(&range, 3), pick(|i| (1000..=5000).contains(&MATTERS[i].2)), "range hits {range}");
    assert!(hit(&range, 3, 0)[1] == "1.0000" && hit(&range, 3, 2)[1] == "1.0000", "range scores are the constant: {range}");
    assert_eq!(ids(&ranged, 3), pick(|i| has(i, "contract") && (1000..=5000).contains(&MATTERS[i].2)), "term AND range {ranged}");
    assert_eq!(hit(&ranged, 3, 0)[1], cell4(s1 + 1.0), "m1 = bm25 + 1.0 {ranged}");
    assert_eq!(hit(&ranged, 3, 1)[1], cell4(s2 + 1.0), "m2 = bm25 + 1.0 {ranged}");

    // g) Paging: LIMIT pages by relevance (the total is the PAGE size --
    //    deviation), SORTBY counts the FULL match set with ties by doc id,
    //    NOCONTENT returns ids + scores without a json payload.
    let page1 = search(&a, IDX, &["contract", "LIMIT", "0", "1"]).await;
    assert_eq!((total_of(&page1), ids(&page1, 2)), (1, vec![mid(0)]), "LIMIT 0 1 pages by score: {page1}");
    let rest = search(&a, IDX, &["contract", "LIMIT", "1", "1"]).await;
    assert_eq!((total_of(&rest), ids(&rest, 2)), (1, vec![mid(1)]), "offset 1 is m2: {rest}");
    assert_eq!((total_of(&top), ids(&top, 2)), (MATTERS.len(), top_fees(2)), "SORTBY counts the full set {top}");
    let bare = search(&a, IDX, &["contract", "NOCONTENT", "WITHSCORES", "LIMIT", "0", "6"]).await;
    assert_eq!(array_len(&bare), 1 + 2 * 2, "NOCONTENT WITHSCORES cells: {bare}");
    assert_eq!(ids(&bare, 2), pick(|i| has(i, "contract")), "bare ids");
    assert_eq!(hit(&bare, 2, 0)[1], cell4(s1), "bare score");
    assert!(!bare.contains("\"title\""), "no json payload: {bare}");

    // h) FT.INFO echoes the corpus statistics and the tuning knobs.
    for (idx, k1, b) in TUNING {
        let info = drain(&a, &["FT.INFO", idx]).await;
        let sum_doclen: u64 = (0..MATTERS.len()).map(doclen).sum();
        assert_eq!(int_after(&info, "num_docs"), MATTERS.len() as i64, "{info}");
        assert_eq!(int_after(&info, "sum_doclen"), sum_doclen as i64, "{info}");
        assert_eq!(bulk_at(info.as_bytes(), 5), cell4(sum_doclen as f64 / MATTERS.len() as f64), "avg_doclen {info}");
        assert_eq!((bulk_at(info.as_bytes(), 7), bulk_at(info.as_bytes(), 14)), ("title".to_string(), "KEYWORD".to_string()), "schema echo {info}");
        assert_eq!((bulk_at(info.as_bytes(), 18), bulk_at(info.as_bytes(), 19), bulk_at(info.as_bytes(), 21)), ("k1".to_string(), format!("{k1}"), format!("{b}")), "k1/b echo {info}");
    }

    // i) The register: the SQL open list is the KEYWORD postings' set; the
    //    shortlist (FT contract hits AND SQL open) and its budget agree with
    //    the fees inside the FT payloads of the same hits.
    let expect_open: Vec<i64> = (0..MATTERS.len()).filter(|&i| MATTERS[i].3 == "open").map(|i| i as i64 + 1).collect();
    assert_eq!(open_ids, expect_open, "SQL open list");
    let mut kw = ids(&keyword, 1);
    kw.sort();
    assert_eq!(kw, pick(|i| MATTERS[i].3 == "open"), "KEYWORD @status:open == SQL open list {keyword}");
    let expect_short: Vec<i64> = (0..MATTERS.len()).filter(|&i| MATTERS[i].3 == "open" && has(i, "contract")).map(|i| i as i64 + 1).collect();
    assert_eq!(short, expect_short, "shortlist = FT contract hits AND SQL open");
    let payload_sum: i64 = (0..hits(&contract, 3))
        .map(|h| hit(&contract, 3, h))
        .filter(|cells| short.contains(&number_of(&cells[0])))
        .map(|cells| json_of(&cells[2])["fee"].as_i64().expect("payload fee"))
        .sum();
    let sql_sum: i64 = cell(&sum).parse().expect("SUM cell");
    let plan_sum: i64 = expect_short.iter().map(|n| MATTERS[*n as usize - 1].2).sum();
    assert_eq!((sql_sum, payload_sum), (plan_sum, plan_sum), "budget: SQL SUM == FT payload fees == plan");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM matters").await, vec![MATTERS.len() as i64], "six rows");

    // j) The desk closes m1: the register flips, the doc leaves the index,
    //    and m2 is re-ranked against the smaller corpus.
    assert_eq!(aff(&mut c, "UPDATE matters SET status = 'closed' WHERE id = 1").await, 1, "close m1");
    assert_eq!(one(&a, &["FT.DEL", IDX, &mid(0)]).await, ":1", "ft.del m1");
    let live: Vec<usize> = (1..MATTERS.len()).collect();
    let reranked = search(&a, IDX, &["contract", "WITHSCORES"]).await;
    let s2_new = bm25("contract", 1, &live, 1.2, 0.75);
    assert_eq!(ids(&reranked, 3), vec![mid(1)], "only m2 is left {reranked}");
    assert_eq!(hit(&reranked, 3, 0)[1], cell4(s2_new), "m2 re-ranked {reranked}");
    assert_ne!(cell4(s2_new), cell4(s2), "n / avgdl / df moved the score");
    assert_eq!(int_after(&drain(&a, &["FT.INFO", IDX]).await, "num_docs"), 5, "num_docs after the delete");
    assert_eq!(ints(&mut c, "SELECT id FROM matters WHERE status = 'open' ORDER BY id").await, expect_open[1..].to_vec(), "m1 left the open list");
    assert_eq!(one(&a, &["FT.DEL", IDX, &mid(0)]).await, ":0", "a second delete is a no-op");
    scenario_common::perf::scenario("bm25", t0);
}
