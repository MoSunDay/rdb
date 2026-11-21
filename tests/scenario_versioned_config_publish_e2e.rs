//! Multi-store scenario #57 -- versioned config publish: one release
//! lands as a Hash (HMSET, HMGET batch with a nil hole, HLEN sizes
//! it, HKEYS/HVALS project it), the version floats up with
//! HINCRBYFLOAT, HSETNX guards a one-shot approver field, HSCAN walks
//! the whole field set under a cursor, HRANDFIELD spot-checks a
//! field, and a MySQL release ledger mirrors the publish. One REAL
//! node, RESP + SQL.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{bulk_at, front_world, int_of, r, rb, rf, rt, FrontSpec};

#[tokio::test]
async fn config_release_hashes_scan_and_ledger() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("cfg57", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The release: one HMSET, three fields.
    scenario_common::ok(
        &r(&a, &[b"HMSET", b"{cf}app:1", b"env", b"prod", b"owner", b"sre", b"ver", b"3"]).await,
        "hmset release",
    );

    // b) Batch read: present fields answer bulks, the missing one a
    //    nil hole.
    let got = rf(&a, &[b"HMGET", b"{cf}app:1", b"env", b"ver", b"ghost"]).await;
    assert_eq!(bulk_at(got.as_bytes(), 0), "prod", "{got}");
    assert_eq!(bulk_at(got.as_bytes(), 1), "3", "{got}");
    assert_eq!(bulk_at(got.as_bytes(), 2), "", "nil hole");
    assert_eq!(int_of(&r(&a, &[b"HLEN", b"{cf}app:1"]).await), 3);

    // c) Projections: HKEYS names the fields, HVALS the values.
    let keys = rf(&a, &[b"HKEYS", b"{cf}app:1"]).await;
    for f in ["env", "owner", "ver"] {
        assert!(keys.contains(f), "{keys}");
    }
    let vals = rf(&a, &[b"HVALS", b"{cf}app:1"]).await;
    for v in ["prod", "sre", "3"] {
        assert!(vals.contains(v), "{vals}");
    }

    // d) The version floats: 3 + 1.0 -> 4 (trailing .0 trimmed).
    assert_eq!(rb(&a, &[b"HINCRBYFLOAT", b"{cf}app:1", b"ver", b"1.0"]).await, "4");

    // e) One-shot approver: HSETNX lands once, the retry is a no-op.
    assert_eq!(int_of(&r(&a, &[b"HSETNX", b"{cf}app:1", b"approved", b"by-oncall"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"HSETNX", b"{cf}app:1", b"approved", b"by-audit"]).await), 0);
    assert_eq!(rb(&a, &[b"HGET", b"{cf}app:1", b"approved"]).await, "by-oncall");

    // f) HSCAN walks the whole field set under the cursor (a full
    //    COUNT page ends at "0"); HRANDFIELD spots one field.
    let page = rf(&a, &[b"HSCAN", b"{cf}app:1", b"0", b"COUNT", b"100"]).await;
    for f in ["env", "owner", "ver", "approved"] {
        assert!(page.contains(f), "hscan sees {f}: {page}");
    }
    assert!(page.contains("0"), "full page closes the cursor");
    let spot = rb(&a, &[b"HRANDFIELD", b"{cf}app:1"]).await;
    assert!(["env", "owner", "ver", "approved"].contains(&spot.as_str()), "{spot}");
    assert_eq!(rt(&a, &[b"TYPE", b"{cf}app:1"]).await.trim_start_matches('+'), "hash");

    // g) The MySQL release ledger mirrors the publish.
    ddl(
        &mut c,
        "CREATE TABLE cfg_release (app VARCHAR(16) PRIMARY KEY, ver INT NOT NULL, approved_by VARCHAR(32))",
    )
    .await;
    run(&mut c, "INSERT INTO cfg_release (app, ver, approved_by) VALUES ('app', 4, 'oncall')").await;
    assert_eq!(ints(&mut c, "SELECT ver FROM cfg_release WHERE app = 'app'").await, vec![4]);
}
