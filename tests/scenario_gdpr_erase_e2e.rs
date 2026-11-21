//! Multi-store scenario #40 -- a GDPR erasure request folded across
//! every store a user touches: MySQL pseudonymizes the account row
//! (UPDATE, the row stays for FK integrity), the ES face erases the
//! search doc (`deleted` then idempotent `not_found`), and the RESP
//! planes drop the session string, the JSON profile (`JSON.DEL :1`
//! then `:0`) and the prefs hash (`DEL :1` then `:0`). After the
//! fold, every read plane answers "gone" for user 1 while user 2 is
//! untouched. One REAL node, MySQL + ES fronts.

mod common;
mod scenario_common;

use common::mysql::{col, connect_root, ddl, ints, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, http, json_of, r, rb, rf, FrontSpec};

#[tokio::test]
async fn gdpr_erasure_folds_user_out_of_every_store() {
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, a) = front_world("gdpr40", &spec).await;
    let (es, resp) = (front.node.es.clone(), a.clone());
    let mut c = connect_root(&front.node).await;

    // a) Two users: SQL accounts, ES search docs (index created with
    //    its mapping first -- bulk never auto-creates), RESP session
    //    strings + JSON profiles + prefs hashes.
    ddl(
        &mut c,
        "CREATE TABLE users (id BIGINT PRIMARY KEY, email VARCHAR(64) NOT NULL, \
         note VARCHAR(64) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO users (id, email, note) VALUES (1, 'ann@x.io', 'vip')").await;
    run(&mut c, "INSERT INTO users (id, email, note) VALUES (2, 'bob@x.io', 'trial')").await;
    let mappings = r#"{"mappings":{"properties":{
        "email":{"type":"keyword"},"note":{"type":"keyword"}}}}"#;
    let (status, body) = http(&es, "PUT", "/users", Some(mappings)).await;
    assert_eq!(status, 200, "{body}");
    let bulk = concat!(
        r#"{"index":{"_index":"users","_id":"u1"}}"#,
        "\n",
        r#"{"email":"ann@x.io","note":"vip"}"#,
        "\n",
        r#"{"index":{"_index":"users","_id":"u2"}}"#,
        "\n",
        r#"{"email":"bob@x.io","note":"trial"}"#,
        "\n",
    );
    let (status, body) = http(&es, "POST", "/_bulk", Some(bulk)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["errors"], false, "{body}");

    ok_str(&r(&resp, &[b"SET", b"gdpr:u1:session", b"abc123"]).await);
    ok_str(&r(&resp, &[b"SET", b"gdpr:u2:session", b"zzz999"]).await);
    ok_str(
        &r(
            &resp,
            &[
                b"JSON.SET",
                b"gdpr:u1:profile",
                b"$",
                br#"{"name":"Ann","tags":["vip","eu"]}"#,
            ],
        )
        .await,
    );
    assert_eq!(
        int_line(&r(&resp, &[b"HSET", b"gdpr:u1:prefs", b"lang", b"de", b"dark", b"1"]).await),
        2
    );

    // b) The erasure request for user 1: SQL is pseudonymized (the row
    //    survives for referential integrity), everything else is gone.
    run(&mut c, "UPDATE users SET email = 'erased@invalid', note = '' WHERE id = 1").await;
    let mails = col(&mut c, "SELECT email FROM users WHERE id = 1").await;
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0], MVal::Bytes(b"erased@invalid".to_vec()), "pseudonymized row");

    // c) ES doc erased: `deleted`, and the replay is `not_found`
    //    WITHOUT a 404 (idempotent).
    let (status, body) = http(&es, "DELETE", "/users/_doc/u1", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["result"], "deleted", "{body}");
    let (status, body) = http(&es, "DELETE", "/users/_doc/u1", None).await;
    assert_ne!(status, 404, "{status} {body}");
    assert_eq!(json_of(&body)["result"], "not_found", "{body}");

    // d) RESP planes: session DEL is :1 then :0, JSON.DEL the profile
    //    is :1 then :0, the prefs hash DEL is :1 then :0.
    assert_eq!(int_line(&r(&resp, &[b"DEL", b"gdpr:u1:session"]).await), 1);
    assert_eq!(int_line(&r(&resp, &[b"DEL", b"gdpr:u1:session"]).await), 0);
    assert_eq!(int_line(&r(&resp, &[b"JSON.DEL", b"gdpr:u1:profile"]).await), 1);
    assert_eq!(int_line(&r(&resp, &[b"JSON.DEL", b"gdpr:u1:profile"]).await), 0);
    assert_eq!(int_line(&r(&resp, &[b"DEL", b"gdpr:u1:prefs"]).await), 1);
    assert_eq!(int_line(&r(&resp, &[b"DEL", b"gdpr:u1:prefs"]).await), 0);

    // e) Every plane answers gone for u1: GET nil, JSON.GET nil, the
    //    search finds nothing, and the index shrank to one doc.
    assert_eq!(rb(&resp, &[b"GET", b"gdpr:u1:session"]).await, "", "session gone");
    assert_eq!(rb(&resp, &[b"JSON.GET", b"gdpr:u1:profile"]).await, "", "profile gone");
    assert_eq!(int_line(&r(&resp, &[b"EXISTS", b"gdpr:u1:prefs"]).await), 0);
    let (status, body) = http(
        &es,
        "POST",
        "/users/_search",
        Some(r#"{"query":{"term":{"email":"ann@x.io"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["hits"]["total"]["value"], 0, "{body}");
    let (_status, body) = http(&es, "POST", "/users/_count", Some("{}")).await;
    assert_eq!(json_of(&body)["count"], 1, "{body}");

    // f) User 2 is untouched everywhere (the fold is surgical).
    assert_eq!(rb(&resp, &[b"GET", b"gdpr:u2:session"]).await, "zzz999");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM users WHERE email = 'bob@x.io'").await, vec![1]);
    let full = rf(&resp, &[b"KEYS", b"gdpr:*"]).await;
    assert!(full.contains("u2"), "u2 keys survive: {full:?}");
    assert!(!full.contains("u1"), "no u1 keys left: {full:?}");
}

fn ok_str(reply: &[u8]) {
    assert!(reply.starts_with(b"+OK"), "{reply:?}");
}

fn int_line(reply: &[u8]) -> i64 {
    let s = String::from_utf8_lossy(reply);
    s.trim().trim_start_matches(':').parse().expect("int line")
}
