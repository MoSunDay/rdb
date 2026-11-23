//! Multi-store scenario #38 -- a storefront cart + discovery plane:
//! the cart lives as one JSON document (ARRAPPEND grows the items
//! array, STRAPPEND extends the gift note, OBJKEYS lists the root,
//! JSON.DEL removes a line item :1 then idempotently :0), checkout
//! writes the surviving lines into SQL, product search rides an
//! FT.CREATE TEXT+VECTOR schema (exact scan pre-build, FT.BUILD
//! re-seeds the ANN centroids, FT.INFO reports them, KNN recall
//! ranks the nearest products) and FT.DEL retires a delisted doc.
//! One REAL node, MySQL + RESP (JSON / FT / vectorset).

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{bulk_at, front_world, r, rf, FrontSpec};

#[tokio::test]
async fn cart_json_lifecycle_checkout_and_ft_vector_discovery() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("cart38", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE orders (id BIGINT PRIMARY KEY AUTO_INCREMENT, \
         sku VARCHAR(8) NOT NULL, qty BIGINT NOT NULL)",
    )
    .await;

    // a) The cart document: two line items, a buyer, a gift note.
    ok(&r(
        &a,
        &[
            b"JSON.SET",
            b"cart:9501",
            b"$",
            br#"{"buyer":"u1","note":"gif","items":[{"sku":"D","qty":1},{"sku":"G","qty":1}]}"#,
        ],
    )
    .await);

    // b) Array ops are int replies: append returns the NEW length,
    //    ARRLEN echoes it, STRAPPEND returns the new note length.
    assert_eq!(
        int(&r(
            &a,
            &[
                b"JSON.ARRAPPEND",
                b"cart:9501",
                b"$.items",
                br#"{"sku":"H","qty":2}"#,
            ],
        )
        .await),
        3,
        "three lines now"
    );
    assert_eq!(
        int(&r(&a, &[b"JSON.ARRLEN", b"cart:9501", b"$.items"]).await),
        3
    );
    assert_eq!(
        int(&r(&a, &[b"JSON.STRAPPEND", b"cart:9501", b"$.note", b"\"t\""]).await),
        4,
        "note is now gift"
    );

    // c) OBJKEYS lists the root members (array reply -> full drain).
    let roots = rf(&a, &[b"JSON.OBJKEYS", b"cart:9501", b"$"]).await;
    for member in ["buyer", "items", "note"] {
        assert!(roots.contains(member), "root has {member}: {roots:?}");
    }

    // d) The shopper drops the G line: JSON.DEL removes exactly that
    //    path (:1) and a replayed delete is :0.
    // (an array index re-addresses after each delete, so the
    // idempotent-:0 replay is pinned on an absent OBJECT path)
    assert_eq!(
        int(&r(&a, &[b"JSON.DEL", b"cart:9501", b"$.items[1]"]).await),
        1
    );
    assert_eq!(
        int(&r(&a, &[b"JSON.DEL", b"cart:9501", b"$.coupon"]).await),
        0
    );
    assert_eq!(
        int(&r(&a, &[b"JSON.ARRLEN", b"cart:9501", b"$.items"]).await),
        2
    );

    // e) Checkout: the surviving D x1 and H x2 land in SQL.
    run(
        &mut c,
        "INSERT INTO orders (sku, qty) VALUES ('D', 1), ('H', 2)",
    )
    .await;
    assert_eq!(ints(&mut c, "SELECT SUM(qty) FROM orders").await, vec![3]);
    let skus = ints(&mut c, "SELECT COUNT(*) FROM orders").await;
    assert_eq!(skus, vec![2], "two order lines");

    // f) Discovery index: TEXT + VECTOR(DIM 4) schema; before any
    //    build the search is an exact scan over postings.
    ok(&r(
        &a,
        &[
            b"FT.CREATE",
            b"pidx",
            b"SCHEMA",
            b"title",
            b"TEXT",
            b"v",
            b"VECTOR",
            b"DIM",
            b"4",
        ],
    )
    .await);
    for (id, title, v) in [
        ("p1", "cordless drill", "0.1,0,0,0"),
        ("p2", "drill bit set", "0.1,0.05,0,0"),
        ("p3", "garden hose", "0,0.2,0,0"),
    ] {
        let json = format!("{{\"title\":\"{title}\",\"v\":[{v}]}}");
        assert_eq!(
            int(&r(&a, &[b"FT.ADD", b"pidx", id.as_bytes(), json.as_bytes()]).await),
            1,
            "add {id}"
        );
    }
    let exact = rf(&a, &[b"FT.SEARCH", b"pidx", b"@title:drill"]).await;
    assert!(
        exact.contains("p1") && exact.contains("p2") && !exact.contains("p3"),
        "{exact:?}"
    );

    // g) FT.BUILD re-seeds the ANN layer (+OK); FT.INFO reports the
    //    built centroids; a KNN probe recalls all three docs.
    ok(&r(&a, &[b"FT.BUILD", b"pidx", b"K", b"2", b"SEED", b"7"]).await);
    let info = rf(&a, &[b"FT.INFO", b"pidx"]).await;
    assert!(info.contains("ann_built"), "{info:?}");
    assert!(info.contains("ann_centroids\r\n:2\r\n"), "{info:?}");
    let knn = rf(
        &a,
        &[
            b"FT.SEARCH",
            b"pidx",
            b"*",
            b"NOCONTENT",
            b"NPROBE",
            b"2",
            b"KNN",
            b"3",
            b"v",
            b"VALUES",
            b"0.12",
            b"0",
            b"0",
            b"0",
        ],
    )
    .await;
    for doc in ["p1", "p2", "p3"] {
        assert!(knn.contains(doc), "knn recalls {doc}: {knn:?}");
    }

    // h) FT.DEL retires the delisted hose (:1 then :0) and the text
    //    search stops matching it.
    assert_eq!(int(&r(&a, &[b"FT.DEL", b"pidx", b"p3"]).await), 1);
    assert_eq!(int(&r(&a, &[b"FT.DEL", b"pidx", b"p3"]).await), 0);
    let after = rf(&a, &[b"FT.SEARCH", b"pidx", b"@title:hose"]).await;
    assert!(!after.contains("p3"), "{after:?}");

    // i) Personal recall on the vectorset plane: [1,0] taste ranks
    //    drill first (this is the same index the cart hint uses).
    assert_eq!(
        int(&r(
            &a,
            &[b"VADD", b"reco:emb", b"VALUES", b"2", b"1", b"1", b"0"]
        )
        .await),
        1
    );
    assert_eq!(
        int(&r(
            &a,
            &[b"VADD", b"reco:emb", b"VALUES", b"2", b"2", b"0", b"1"]
        )
        .await),
        1
    );
    assert_eq!(
        int(&r(
            &a,
            &[b"VADD", b"reco:emb", b"VALUES", b"2", b"3", b"1", b"1"]
        )
        .await),
        1
    );
    let recall = rf(
        &a,
        &[b"VSIM", b"reco:emb", b"COUNT", b"3", b"VALUES", b"1", b"0"],
    )
    .await;
    assert_eq!(
        bulk_at(recall.as_bytes(), 0),
        "1",
        "drill nearest: {recall:?}"
    );
}

fn ok(reply: &[u8]) {
    assert!(reply.starts_with(b"+OK"), "{reply:?}");
}

fn int(reply: &[u8]) -> i64 {
    let s = String::from_utf8_lossy(reply);
    s.trim().trim_start_matches(':').parse().expect("int line")
}
