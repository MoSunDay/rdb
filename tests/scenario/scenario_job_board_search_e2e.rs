//! Multi-store scenario #148 -- a job board where the same eight postings
//! live as HASHes (`job:<n>`), as JSON documents of the FT index `jobs148`
//! (STOPWORDS the/for, K1/B tuned), as ES docs of a mirror index and as
//! SQL rows: a salary range, a title phrase, a prefix, skill unions /
//! differences and a python+remote conjunction answer the SAME posting ids
//! on every face (ES `match` AND / `term`, a prepared `BETWEEN`); `* SORTBY
//! salary DESC LIMIT 0 2` is re-derived and cross-checked on the HASH, `RETURN`
//! projects in list order and `HIGHLIGHT` wraps.
//! HASH + FT (CREATE/ADD/SEARCH) + ES + MySQL. One REAL node, fronts mysql + es (RESP always).
//! Deviation: FT.CREATE takes options BEFORE `SCHEMA`, knows only TEXT | KEYWORD | NUMERIC | VECTOR
//! (no `ON HASH PREFIX 1` / TAG; HASH keys are NOT auto-indexed, JSON rides FT.ADD): src/search/ft_cmd.rs:69-124, :150-153.
//! An ES index and an FT index share one key namespace, so `PUT /jobs148` answers 409 and the ES mirror is `jobs148es`: src/es/write.rs:107-116.
use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, one as sql_one, run, s};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{FrontSpec, bulk_at, front_world, http, int_of, json_of, r};
use serde_json::json;
use std::time::Duration;

const IDX: &str = "jobs148";
/// `(title, company, salary, skills, remote)` of the eight postings;
/// job 3 carries the stopword `for` in its title.
const JOBS: [(&str, &str, i64, &[&str], &str); 8] = [
    ("Senior Data Engineer", "Nordwind Analytics", 128000, &["python", "sql", "rust"], "yes"),
    ("Data Engineer", "Hafenlogistik", 98000, &["python", "sql"], "no"),
    ("Software Engineer for the Java Stack", "Blue Harbor Systems", 101000, &["java", "sql"], "no"),
    ("Platform Engineer", "Kielworks", 118000, &["rust", "kubernetes"], "yes"),
    ("Analytics Engineer", "Tidepool Labs", 91000, &["python", "dbt"], "yes"),
    ("Backend Engineer", "Blue Harbor Systems", 125000, &["java", "go"], "no"),
    ("Staff Data Engineer", "Nordwind Analytics", 142000, &["python", "rust", "sql"], "yes"),
    ("Machine Learning Researcher", "Tidepool Labs", 133000, &["python", "rust"], "yes"),
];

/// Bytes of a `&str` command line.
fn argv<'a>(args: &[&'a str]) -> Vec<&'a [u8]> {
    args.iter().map(|t| t.as_bytes()).collect()
}

/// Full-drain reply text with a short quiet gap (the fixture's `rf`
/// parks 250ms per call).
async fn drain(a: &str, args: &[&str]) -> String {
    let raw = common::lite::cmd_full_reply_wait(a, common::TOKEN, &argv(args), 20, 10_000).await;
    common::lite::text(&raw)
}

/// One-shot reply text (status / error / integer).
async fn one(a: &str, args: &[&str]) -> String {
    common::lite::text(&r(a, &argv(args)).await)
}

/// One-shot integer reply.
async fn one_int(a: &str, args: &[&str]) -> i64 {
    int_of(&r(a, &argv(args)).await)
}

/// Doc id of posting `i`: the HASH key, the FT doc id AND the ES doc id.
fn key(i: usize) -> String {
    format!("job:{}", i + 1)
}

/// Posting index behind a doc id.
fn index_of(id: &str) -> usize {
    id.strip_prefix("job:")
        .and_then(|n| n.parse::<usize>().ok())
        .expect("job id")
        - 1
}

/// Indexed JSON of posting `i` (the FT document AND the ES source).
fn doc(i: usize) -> String {
    let (title, company, salary, skills, remote) = JOBS[i];
    json!({"title": title, "company": company, "salary": salary, "skills": skills, "remote": remote})
        .to_string()
}

/// `FT.SEARCH jobs148 <args...>` reply text.
async fn search(a: &str, args: &[&str]) -> String {
    let mut full = vec!["FT.SEARCH", IDX];
    full.extend_from_slice(args);
    drain(a, &full).await
}

/// The integer total of a FT.SEARCH reply: a FLAT array whose first
/// element is the `:total` header (page size unless SORTBY).
fn total_of(reply: &str) -> usize {
    reply
        .lines()
        .find_map(|l| l.strip_prefix(':').and_then(|t| t.parse().ok()))
        .unwrap_or_else(|| panic!("no total in {reply}"))
}

/// Hit count of a FT.SEARCH reply from its FLAT array header:
/// `*N` = total + (docid, content) pairs, and with SORTBY + LIMIT the
/// total is the full match count while the page is shorter.
fn hits(reply: &str) -> usize {
    let n = array_len(reply);
    assert!(n >= 1, "total always leads: {reply}");
    (n - 1) / 2
}

/// Doc id cells of every hit, in reply order (docid, content) * hits.
fn doc_ids(reply: &str) -> Vec<String> {
    (0..hits(reply)).map(|h| bulk_at(reply.as_bytes(), h * 2)).collect()
}

/// Sorted doc id SET of a reply: without SORTBY the hits come back in
/// relevance order, which the Rust-side re-derivations do not model.
fn sorted_ids(reply: &str) -> Vec<String> {
    let mut ids = doc_ids(reply);
    ids.sort();
    ids
}

/// Content cell of hit `h` (the stored JSON document).
fn content(reply: &str, h: usize) -> String {
    bulk_at(reply.as_bytes(), h * 2 + 1)
}

/// Sorted ids of the postings matching `pred` (set comparison).
fn pick(pred: impl Fn(usize) -> bool) -> Vec<String> {
    let mut ids: Vec<String> = (0..JOBS.len()).filter(|&i| pred(i)).map(key).collect();
    ids.sort();
    ids
}

/// Alphanumeric lowercase tokens (the indexer's Latin tokenizer).
fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// `words` appear adjacently in the title or the company (a phrase).
fn has_phrase(i: usize, words: &[&str]) -> bool {
    let n = words.len();
    let hit = |text: &str| tokens(text).windows(n).any(|w| w == words);
    hit(JOBS[i].0) || hit(JOBS[i].1)
}

/// Some title token starts with `stem` (the unscoped prefix clause).
fn has_prefix(i: usize, stem: &str) -> bool {
    [JOBS[i].0, JOBS[i].1]
        .iter()
        .any(|t| tokens(t).iter().any(|w| w.starts_with(stem)))
}

/// Some title / company token equals one of `words` (ES `match` OR).
fn has_any(i: usize, words: &[&str]) -> bool {
    [JOBS[i].0, JOBS[i].1].iter().any(|t| tokens(t).iter().any(|w| words.contains(&w.as_str())))
}

fn has_skill(i: usize, skill: &str) -> bool {
    JOBS[i].3.contains(&skill)
}

/// `* SORTBY salary DESC LIMIT 0 <n>` re-derived: value descending,
/// doc id ascending on ties.
fn top_salary(n: usize) -> Vec<String> {
    let mut idx: Vec<usize> = (0..JOBS.len()).collect();
    idx.sort_by(|&x, &y| JOBS[y].2.cmp(&JOBS[x].2).then_with(|| key(x).cmp(&key(y))));
    idx.into_iter().take(n).map(key).collect()
}

/// HIGHLIGHT re-derived: every title token equal to a query term is
/// wrapped, case preserved.
fn wrap(text: &str, terms: &[&str]) -> String {
    text.split_whitespace()
        .map(|w| {
            if terms.contains(&w.to_lowercase().as_str()) {
                format!("<b>{w}</b>")
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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

/// HGETALL reply as `(field, value)` pairs sorted by field.
fn hash_pairs(reply: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = (0..array_len(reply) / 2)
        .map(|k| (bulk_at(reply.as_bytes(), 2 * k), bulk_at(reply.as_bytes(), 2 * k + 1)))
        .collect();
    pairs.sort();
    pairs
}

/// Sorted `_id`s of an ES `_search` reply.
fn es_ids(body: &str) -> Vec<String> {
    let mut ids: Vec<String> = json_of(body)["hits"]["hits"]
        .as_array()
        .expect("hits array")
        .iter()
        .map(|h| h["_id"].as_str().expect("_id").to_string())
        .collect();
    ids.sort();
    ids
}

/// Sorted `_id`s of an ES `_search` POST against `path` (status 200).
async fn es_search(es: &str, path: &str, q: &str) -> Vec<String> {
    let (status, body) = http(es, "POST", path, Some(q)).await;
    assert_eq!(status, 200, "es {path}: {body}");
    es_ids(&body)
}

/// String cell of a SQL row (NULL is a failure: every column is NOT NULL).
fn cell_s(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => panic!("expected string cell, got {other:?}"),
    }
}

#[tokio::test]
async fn job_board_search_matches_sql_and_es() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        es: true,
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("jobs148", &spec).await;
    let es = front.node.es.clone();

    // a) The FT.CREATE surface: options BEFORE SCHEMA, no ON HASH
    //    PREFIX, no TAG, one index per name, unknown index refused.
    let e = one(&a, &["FT.CREATE", IDX, "ON", "HASH", "PREFIX", "1", "job:", "SCHEMA", "salary", "NUMERIC"]).await;
    assert_eq!(e, "-ERR unknown option", "ON HASH PREFIX 1");
    let e = one(&a, &["FT.CREATE", IDX, "SCHEMA", "skills", "TAG"]).await;
    assert_eq!(e, "-ERR unknown field type (use TEXT, KEYWORD, NUMERIC or VECTOR DIM n)", "TAG");
    let created = one(
        &a,
        &[
            "FT.CREATE", IDX, "STOPWORDS", "2", "the", "for", "K1", "1.2", "B", "0.75", "SCHEMA",
            "salary", "NUMERIC", "skills", "KEYWORD", "title", "TEXT", "company", "TEXT", "remote",
            "KEYWORD",
        ],
    )
    .await;
    assert!(created.starts_with("+OK"), "create {IDX}: {created}");
    let dup = one(&a, &["FT.CREATE", IDX, "SCHEMA", "salary", "NUMERIC"]).await;
    assert_eq!(dup, "-ERR index already exists", "duplicate");
    assert_eq!(one(&a, &["FT.SEARCH", "nope148", "*"]).await, "-ERR unknown index", "unknown index");

    // b) Hot block: 8 HASH writes + 8 FT.ADDs + 7 searches whose doc-id
    //    sets are re-derived Rust-side from the postings table.
    let t = scenario_common::perf::now();
    for (i, &(title, company, salary, skills, remote)) in JOBS.iter().enumerate() {
        let hset = one_int(
            &a,
            &[
                "HSET", &key(i), "title", title, "company", company, "salary", &salary.to_string(),
                "skills", &skills.join(" "), "remote", remote,
            ],
        )
        .await;
        assert_eq!(hset, 5, "hset {}", key(i));
        assert_eq!(one_int(&a, &["FT.ADD", IDX, &key(i), &doc(i)]).await, 1, "ft.add {}", key(i));
    }
    let range = search(&a, &["@salary:[90000 130000]"]).await;
    assert_eq!(sorted_ids(&range), pick(|i| (90000..=130000).contains(&JOBS[i].2)), "salary range");
    assert_eq!(total_of(&range), 6, "page total");
    let phrase = search(&a, &["\"data engineer\""]).await;
    assert_eq!(sorted_ids(&phrase), pick(|i| has_phrase(i, &["data", "engineer"])), "title phrase");
    assert_eq!(sorted_ids(&search(&a, &["engi*"]).await), pick(|i| has_prefix(i, "engi")), "prefix");
    let union = search(&a, &["@skills:python | @skills:rust"]).await;
    assert_eq!(sorted_ids(&union), pick(|i| has_skill(i, "python") || has_skill(i, "rust")), "skill union");
    let diff = search(&a, &["@skills:java -@skills:sql"]).await;
    assert_eq!(sorted_ids(&diff), pick(|i| has_skill(i, "java") && !has_skill(i, "sql")), "skill difference");
    let both = search(&a, &["@skills:python @remote:yes"]).await;
    assert_eq!(sorted_ids(&both), pick(|i| has_skill(i, "python") && JOBS[i].4 == "yes"), "python + remote");
    assert_eq!(sorted_ids(&search(&a, &["the"]).await), Vec::<String>::new(), "a stopword matches nothing");
    scenario_common::perf::within("jobs148/8-hset+8-add+7-search", t, Duration::from_secs(2));

    // c) SORTBY: total = full match count, the two best-paid postings
    //    re-derived, and job:7's HASH agreeing with its FT document.
    let top = search(&a, &["*", "SORTBY", "salary", "DESC", "LIMIT", "0", "2"]).await;
    assert_eq!(total_of(&top), JOBS.len(), "SORTBY total is the full match count");
    assert_eq!(doc_ids(&top), top_salary(2), "two best-paid postings");
    assert_eq!(json_of(&content(&top, 0)), json_of(&doc(6)), "hit content is the FT document");
    let hash = hash_pairs(&drain(&a, &["HGETALL", "job:7"]).await);
    let (_, company, salary, skills, remote) = JOBS[6];
    let mut want = vec![
        ("title".to_string(), JOBS[6].0.to_string()),
        ("company".to_string(), company.to_string()),
        ("salary".to_string(), salary.to_string()),
        ("skills".to_string(), skills.join(" ")),
        ("remote".to_string(), remote.to_string()),
    ];
    want.sort();
    assert_eq!(hash, want, "job:7 HASH carries the posting");

    // d) RETURN projects in list order; HIGHLIGHT TAGS wraps the matched
    //    title tokens of every phrase hit and leaves other fields alone.
    let ret = search(&a, &["@salary:[140000 150000]", "RETURN", "2", "title", "salary"]).await;
    assert_eq!(sorted_ids(&ret), vec![key(6)], "RETURN keeps the hit");
    assert_eq!(
        content(&ret, 0),
        format!("{{\"title\":\"{}\",\"salary\":{}}}", JOBS[6].0, JOBS[6].2),
        "RETURN order"
    );
    let hl = search(&a, &["\"data engineer\"", "HIGHLIGHT", "TAGS", "<b>", "</b>"]).await;
    assert_eq!(total_of(&hl), 3, "three phrase hits");
    for h in 0..hits(&hl) {
        let id = bulk_at(hl.as_bytes(), h * 2);
        let i = index_of(&id);
        assert_eq!(
            json_of(&content(&hl, h))["title"].as_str().expect("title"),
            wrap(JOBS[i].0, &["data", "engineer"]),
            "highlight {id}"
        );
        assert_eq!(json_of(&content(&hl, h))["company"], json!(JOBS[i].1), "other fields in {id}");
    }

    // e) ES: an ES index and an FT index share ONE key namespace, so
    //    `PUT /jobs148` is refused (409) and the ES mirror lives under
    //    `jobs148es`; its `match` AND equals the phrase set, the default
    //    OR is the superset re-derived Rust-side, `term remote` the
    //    remote set, and the ES query face also reads the FT index.
    let maps = concat!(
        r#"{"mappings":{"properties":{"title":{"type":"text"},"company":{"type":"keyword"},"#,
        r#""salary":{"type":"long"},"skills":{"type":"keyword"},"remote":{"type":"keyword"}}}}"#
    );
    let (status, body) = http(&es, "PUT", "/jobs148", Some(maps)).await;
    let err = &json_of(&body)["error"];
    assert_eq!(
        (status, err["type"].as_str(), err["reason"].as_str()),
        (409, Some("resource_already_exists_exception"), Some("index [jobs148] already exists")),
        "the FT index IS the ES index: {body}"
    );
    let (status, body) = http(&es, "PUT", "/jobs148es", Some(maps)).await;
    assert_eq!(status, 200, "create jobs148es: {body}");
    for i in 0..JOBS.len() {
        let (status, body) = http(&es, "PUT", &format!("/jobs148es/_doc/{}", key(i)), Some(&doc(i))).await;
        assert_eq!((status, json_of(&body)["result"].as_str()), (201, Some("created")), "index {}: {body}", key(i));
    }
    let (status, body) = http(&es, "POST", "/jobs148es/_refresh", None).await;
    assert_eq!(status, 200, "refresh: {body}");
    let phrase_set = sorted_ids(&phrase);
    let and_q = r#"{"query":{"match":{"title":{"query":"data engineer","operator":"and"}}},"size":20}"#;
    assert_eq!(es_search(&es, "/jobs148es/_search", and_q).await, phrase_set, "ES match AND == FT phrase");
    assert_eq!(es_search(&es, "/jobs148/_search", and_q).await, phrase_set, "the ES face reads the FT index");
    let or_set = pick(|i| has_any(i, &["data", "engineer"]));
    let or_q = r#"{"query":{"match":{"title":"data engineer"}},"size":20}"#;
    assert_eq!(es_search(&es, "/jobs148es/_search", or_q).await, or_set, "ES default OR = any term");
    assert!(or_set.len() > phrase_set.len(), "OR is the superset");
    let term_q = r#"{"query":{"term":{"remote":"yes"}},"size":20}"#;
    assert_eq!(es_search(&es, "/jobs148es/_search", term_q).await, pick(|i| JOBS[i].4 == "yes"), "ES term remote");

    // f) SQL: the same postings as rows; a prepared BETWEEN answers the
    //    FT salary range, job:7's row carries the HASH title.
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE jobs148 (id VARCHAR(16) PRIMARY KEY, title VARCHAR(64) NOT NULL, \
         company VARCHAR(32) NOT NULL, salary BIGINT NOT NULL, skills VARCHAR(48) NOT NULL, \
         remote VARCHAR(3) NOT NULL)",
    )
    .await;
    let mut values = String::new();
    for (i, &(title, company, salary, skills, remote)) in JOBS.iter().enumerate() {
        values.push_str(&format!(
            "{}('{}','{title}','{company}',{salary},'{}','{remote}')",
            if i > 0 { "," } else { "" },
            key(i),
            skills.join(" ")
        ));
    }
    run(&mut c, &format!("INSERT INTO jobs148 (id, title, company, salary, skills, remote) VALUES {values}")).await;
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM jobs148").await, vec![JOBS.len() as i64], "eight rows");
    let stmt = c
        .prep("SELECT id FROM jobs148 WHERE salary BETWEEN ? AND ? ORDER BY id")
        .await
        .expect("prepare between");
    let rs: Vec<mysql_async::Row> = c.exec(&stmt, (90000_i64, 130000_i64)).await.expect("exec between");
    let sql_ids: Vec<String> = rs.iter().map(|row| cell_s(&row.get::<MVal, _>(0).unwrap_or(MVal::NULL))).collect();
    let range_ids = sorted_ids(&range);
    assert_eq!(sql_ids, range_ids, "SQL BETWEEN == FT salary range");
    assert_eq!(sql_one(&mut c, "SELECT title FROM jobs148 WHERE id = 'job:7'").await, s(JOBS[6].0), "SQL row == HASH title");
    scenario_common::perf::scenario("jobs148", t0);
}
