//! Multi-store scenario #152 -- feature-flag documents: the first writer
//! roots a flag with JSON.SET, a second writer's NX is refused, an updater's XX only
//! touches an existing flag, an overwrite keeps the TTL and a multi-path JSON.GET
//! answers a FLAT array; the registry is mirrored into MySQL so the rollout report
//! reads the same enabled/rollout_pct values the JSON documents hold.
//! One REAL node, RESP (JSON) + MySQL.
//! Deviation: JSON.GET's INDENT/NEWLINE/SPACE options are NOT implemented -- every
//! argument after the key is parsed as a path, so `JSON.GET k $ INDENT <tab>` answers
//! `-ERR wrong static path` (src/command/json_cmd.rs:184-192, src/command/json_path.rs:32-79);
//! a multi-path JSON.GET returns one flat array entry per path instead of Redis'
//! merged object (src/command/json_cmd.rs:204-216).

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, one, rows, run};
use scenario_common::{bulk_at, front_world, int_of, ok, r, rb, rt, FrontSpec};

/// The flag registry, Rust side: (name, enabled, rollout pct, owner).
const FLAGS: [(&str, bool, i64, &str); 3] = [
    ("checkout", true, 25, "payments"),
    ("dark-mode", false, 0, "web"),
    ("beta-api", true, 100, "platform"),
];

/// The XX update of `checkout`: rollout widened, flag switched off.
const CHECKOUT_V2: (bool, i64, &str) = (false, 50, "payments");

/// A flag document as the writers send it (compact JSON object).
fn doc(enabled: bool, pct: i64, owner: &str) -> String {
    format!(r#"{{"enabled":{enabled},"rollout_pct":{pct},"owner":"{owner}"}}"#)
}

/// JSON text of a string field (JSON.GET answers the serialized value,
/// so a string keeps its quotes).
fn json_str(s: &str) -> String {
    format!("\"{s}\"")
}

/// RESP key of one flag.
fn key(name: &str) -> String {
    format!("flag:{name}")
}

/// Final state of one flag: the XX update wins for `checkout`.
fn final_state(name: &str, enabled: bool, pct: i64) -> (bool, i64) {
    if name == "checkout" {
        (CHECKOUT_V2.0, CHECKOUT_V2.1)
    } else {
        (enabled, pct)
    }
}

/// SQL mirror rows the report must answer: one line per flag, ordered
/// by name, enabled rendered as 1/0.
fn mirror_rows() -> Vec<Vec<String>> {
    let mut lines: Vec<Vec<String>> = FLAGS
        .iter()
        .map(|(name, enabled, pct, _)| {
            let (enabled, pct) = final_state(name, *enabled, *pct);
            vec![
                (*name).to_string(),
                i64::from(enabled).to_string(),
                pct.to_string(),
            ]
        })
        .collect();
    lines.sort();
    lines
}

/// Full-drain reply (flat arrays need the quiet-gap reader).
async fn drained(addr: &str, args: &[&[u8]]) -> Vec<u8> {
    common::lite::cmd_full_reply_wait(addr, common::TOKEN, args, 25, 10_000).await
}

/// Text of one wire cell.
fn cell(v: &mysql_async::Value) -> String {
    match v {
        mysql_async::Value::NULL => "<NULL>".to_string(),
        mysql_async::Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
async fn flag_documents_create_only_and_update_only() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("flag152", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Hot block: the create-only / update-only discipline. The first
    //    writer roots the flag; NX refuses to clobber it (nil bulk) and
    //    the original value survives; NX on a MISSING key creates it;
    //    XX on a missing key answers nil and creates nothing; XX on the
    //    existing flag lands the update and the new value reads back.
    let t = scenario_common::perf::now();
    let checkout = key("checkout");
    let v1 = doc(FLAGS[0].1, FLAGS[0].2, FLAGS[0].3);
    ok(
        &r(
            &a,
            &[b"JSON.SET", checkout.as_bytes(), b"$", v1.as_bytes()],
        )
        .await,
        "first writer roots the flag",
    );
    let rogue = doc(false, 0, "rogue");
    assert_eq!(
        rt(
            &a,
            &[
                b"JSON.SET",
                checkout.as_bytes(),
                b"$",
                rogue.as_bytes(),
                b"NX"
            ]
        )
        .await,
        "$-1",
        "NX never clobbers an existing flag"
    );
    assert_eq!(
        rb(&a, &[b"JSON.GET", checkout.as_bytes(), b"$.owner"]).await,
        json_str(FLAGS[0].3),
        "the refused writer left the flag alone"
    );
    let dark = key("dark-mode");
    let v2 = doc(FLAGS[1].1, FLAGS[1].2, FLAGS[1].3);
    ok(
        &r(
            &a,
            &[b"JSON.SET", dark.as_bytes(), b"$", v2.as_bytes(), b"NX"],
        )
        .await,
        "NX creates a missing flag",
    );
    let ghost = key("ghost");
    let v3 = doc(true, 1, "nobody");
    assert_eq!(
        rt(
            &a,
            &[b"JSON.SET", ghost.as_bytes(), b"$", v3.as_bytes(), b"XX"],
        )
        .await,
        "$-1",
        "XX only touches an existing flag"
    );
    assert_eq!(
        rt(&a, &[b"JSON.GET", ghost.as_bytes()]).await,
        "$-1",
        "the XX miss created no key"
    );
    let beta = key("beta-api");
    let v4 = doc(FLAGS[2].1, FLAGS[2].2, FLAGS[2].3);
    ok(
        &r(&a, &[b"JSON.SET", beta.as_bytes(), b"$", v4.as_bytes()]).await,
        "third flag rooted",
    );
    let v5 = doc(CHECKOUT_V2.0, CHECKOUT_V2.1, CHECKOUT_V2.2);
    ok(
        &r(
            &a,
            &[
                b"JSON.SET",
                checkout.as_bytes(),
                b"$",
                v5.as_bytes(),
                b"XX"
            ]
        )
        .await,
        "XX updates the existing flag",
    );
    assert_eq!(
        rb(&a, &[b"JSON.GET", checkout.as_bytes(), b"$.rollout_pct"]).await,
        CHECKOUT_V2.1.to_string(),
        "the XX update reads back"
    );
    scenario_common::perf::within("flag152/nx-xx-writers", t, Duration::from_secs(2));

    // b) An overwrite keeps the TTL (json_cmd.rs:96-98: save_doc reuses
    //    the envelope's expiry); a flag that never got one answers -1.
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", checkout.as_bytes(), b"200"]).await),
        1,
        "expire the flag"
    );
    ok(
        &r(
            &a,
            &[
                b"JSON.SET",
                checkout.as_bytes(),
                b"$.enabled",
                b"true",
            ]
        )
        .await,
        "field-level overwrite",
    );
    let ttl = int_of(&r(&a, &[b"TTL", checkout.as_bytes()]).await);
    assert!(
        (190..=200).contains(&ttl),
        "TTL survives the overwrite: {ttl}"
    );
    assert_eq!(
        rb(&a, &[b"JSON.GET", checkout.as_bytes(), b"$.enabled"]).await,
        "true",
        "the overwrite landed"
    );
    assert_eq!(
        int_of(&r(&a, &[b"TTL", dark.as_bytes()]).await),
        -1,
        "a flag without an expiry answers -1"
    );
    // the mirror below expects the switched-off flag: put it back
    ok(
        &r(
            &a,
            &[b"JSON.SET", checkout.as_bytes(), b"$.enabled", b"false"],
        )
        .await,
        "flag switched off again",
    );

    // c) JSON.GET with several paths: a FLAT array, one entry per path
    //    (documented deviation -- Redis merges them into one object);
    //    a missing path leaves a nil bulk in its own slot.
    let flat = drained(
        &a,
        &[
            b"JSON.GET",
            checkout.as_bytes(),
            b"$.enabled",
            b"$.rollout_pct",
        ],
    )
    .await;
    assert!(
        flat.starts_with(b"*2\r\n"),
        "two paths -> two entries: {flat:?}"
    );
    assert_eq!(bulk_at(&flat, 0), "false", "entry 0 is $.enabled");
    assert_eq!(bulk_at(&flat, 1), "50", "entry 1 is $.rollout_pct");
    let holed = drained(
        &a,
        &[
            b"JSON.GET",
            checkout.as_bytes(),
            b"$.enabled",
            b"$.nope",
        ],
    )
    .await;
    assert!(holed.starts_with(b"*2\r\n"), "{holed:?}");
    assert_eq!(bulk_at(&holed, 0), "false", "the live path answers");
    assert_eq!(bulk_at(&holed, 1), "", "the missing path is a nil slot");
    assert!(
        common::lite::text(&holed).contains("$-1"),
        "the nil slot is a nil bulk: {holed:?}"
    );

    // d) INDENT is not implemented: it parses as an extra path argument.
    assert_eq!(
        rt(
            &a,
            &[
                b"JSON.GET",
                checkout.as_bytes(),
                b"$",
                b"INDENT",
                b"\t",
            ]
        )
        .await,
        "-ERR wrong static path",
        "INDENT is read as a path (deviation)"
    );
    assert_eq!(
        rb(&a, &[b"JSON.GET", checkout.as_bytes(), b"$"]).await,
        doc(CHECKOUT_V2.0, CHECKOUT_V2.1, CHECKOUT_V2.2),
        "the failed option read changed nothing"
    );

    // e) Introspection of the flag document.
    assert_eq!(
        rt(&a, &[b"JSON.TYPE", checkout.as_bytes(), b"$"]).await,
        "+object",
        "root type"
    );
    assert_eq!(
        rt(&a, &[b"JSON.TYPE", checkout.as_bytes(), b"$.enabled"]).await,
        "+boolean",
        "boolean field"
    );
    assert_eq!(
        rt(&a, &[b"JSON.TYPE", checkout.as_bytes(), b"$.rollout_pct"]).await,
        "+integer",
        "integer field"
    );
    assert_eq!(
        rt(&a, &[b"JSON.TYPE", checkout.as_bytes(), b"$.nope"]).await,
        "$-1",
        "missing path has no type"
    );
    assert_eq!(
        int_of(&r(&a, &[b"JSON.OBJLEN", checkout.as_bytes(), b"$"]).await),
        3,
        "three flag fields"
    );
    assert_eq!(
        int_of(&r(&a, &[b"JSON.STRLEN", checkout.as_bytes(), b"$.owner"]).await),
        CHECKOUT_V2.2.len() as i64,
        "owner string length"
    );

    // f) MySQL keeps the flag registry: one row per flag, and the report
    //    reads exactly the enabled/rollout_pct the JSON documents hold.
    ddl(
        &mut c,
        "CREATE TABLE flags (name VARCHAR(24) PRIMARY KEY, enabled TINYINT NOT NULL, \
         rollout_pct INT NOT NULL)",
    )
    .await;
    for (name, enabled, pct, _) in FLAGS {
        let (enabled, pct) = final_state(name, enabled, pct);
        run(
            &mut c,
            &format!(
                "INSERT INTO flags (name, enabled, rollout_pct) VALUES ('{name}', {}, {pct})",
                i64::from(enabled)
            ),
        )
        .await;
    }
    let got: Vec<Vec<String>> = rows(
        &mut c,
        "SELECT name, enabled, rollout_pct FROM flags ORDER BY name",
    )
    .await
    .iter()
    .map(|row| row.iter().map(cell).collect())
    .collect();
    assert_eq!(got, mirror_rows(), "the SQL mirror matches the flags");
    for (name, enabled, pct, _) in FLAGS {
        let (enabled, pct) = final_state(name, enabled, pct);
        let k = key(name);
        assert_eq!(
            rb(&a, &[b"JSON.GET", k.as_bytes(), b"$.enabled"]).await,
            enabled.to_string(),
            "{name}: JSON enabled"
        );
        assert_eq!(
            rb(&a, &[b"JSON.GET", k.as_bytes(), b"$.rollout_pct"]).await,
            pct.to_string(),
            "{name}: JSON rollout"
        );
    }
    assert_eq!(
        cell(&one(&mut c, "SELECT COUNT(*) FROM flags").await),
        FLAGS.len().to_string(),
        "one row per flag"
    );

    scenario_common::perf::scenario("flag152", t0);
}
