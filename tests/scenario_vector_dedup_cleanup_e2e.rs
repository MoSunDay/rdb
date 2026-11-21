//! Multi-store scenario #58 -- embedding dedup + cleanup: VADD seeds a
//! 2-D vector set (auto dims from VALUES), a duplicate VADD answers
//! :0 and keeps the count flat while OVERWRITING the vector and
//! PRESERVING the element's attribute (VSETATTR :1 / VGETATTR bulk,
//! nil when unset), VSIM ranks the nearest after the overwrite,
//! VREM retires an element (:1 then :0, the last one wipes the
//! family), and a MySQL embedding table mirrors the dedup cleanup.
//! One REAL node, RESP + SQL.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{bulk_at, front_world, int_of, r, rb, rf, FrontSpec};

#[tokio::test]
async fn embeddings_dedup_attr_and_retire() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("vd58", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Seed: three 2-D embeddings (drill [1,0], scarf [0,1], belt
    //    [1,1]); VADD answers :1 per NEW element.
    assert_eq!(int_of(&r(&a, &[b"VADD", b"{vd}emb", b"VALUES", b"2", b"a", b"1", b"0"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"VADD", b"{vd}emb", b"VALUES", b"2", b"b", b"0", b"1"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"VADD", b"{vd}emb", b"VALUES", b"2", b"c", b"1", b"1"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"VCARD", b"{vd}emb"]).await), 3);

    // b) Attributes ride the element: VSETATTR lands on a live
    //    element (:1), misses a missing one (:0); VGETATTR answers
    //    the bytes, nil when unset.
    assert_eq!(int_of(&r(&a, &[b"VSETATTR", b"{vd}emb", b"a", b"src=web"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"VSETATTR", b"{vd}emb", b"zz", b"src=x"]).await), 0);
    assert_eq!(rb(&a, &[b"VGETATTR", b"{vd}emb", b"a"]).await, "src=web");
    assert_eq!(rb(&a, &[b"VGETATTR", b"{vd}emb", b"b"]).await, "", "unset attr is nil");

    // c) Dedup: re-adding `a` (even with a different vector) answers
    //    :0, the count stays flat, and the attribute SURVIVES the
    //    overwrite.
    assert_eq!(int_of(&r(&a, &[b"VADD", b"{vd}emb", b"VALUES", b"2", b"a", b"0", b"1"]).await), 0);
    assert_eq!(int_of(&r(&a, &[b"VCARD", b"{vd}emb"]).await), 3, "dedup keeps the count");
    assert_eq!(rb(&a, &[b"VGETATTR", b"{vd}emb", b"a"]).await, "src=web", "attr survives");

    // d) The overwrite is real: query [0,1] now ranks `a` first
    //    (tied with b at 1.0; a rides ahead), c last.
    let near = rf(&a, &[b"VSIM", b"{vd}emb", b"COUNT", b"3", b"VALUES", b"0", b"1"]).await;
    assert_eq!(bulk_at(near.as_bytes(), 0), "a", "a moved to [0,1]: {near}");
    assert_eq!(bulk_at(near.as_bytes(), 2), "c", "diagonal last: {near}");

    // e) Cleanup: VREM retires c (:1), a second sweep misses (:0),
    //    the card drops; b leaves next, and the last element wipes
    //    the whole family.
    assert_eq!(int_of(&r(&a, &[b"VREM", b"{vd}emb", b"c"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"VREM", b"{vd}emb", b"c"]).await), 0);
    assert_eq!(int_of(&r(&a, &[b"VCARD", b"{vd}emb"]).await), 2);
    assert_eq!(int_of(&r(&a, &[b"VREM", b"{vd}emb", b"b"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"VREM", b"{vd}emb", b"a"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"VCARD", b"{vd}emb"]).await), 0, "family wiped");

    // f) The MySQL embedding table mirrors the dedup cleanup.
    ddl(
        &mut c,
        "CREATE TABLE emb (elem VARCHAR(8) PRIMARY KEY, src VARCHAR(16) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO emb (elem, src) VALUES ('a','web'), ('b','app'), ('c','app')").await;
    run(&mut c, "DELETE FROM emb WHERE elem IN ('b', 'c')").await;
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM emb").await, vec![1]);
}
