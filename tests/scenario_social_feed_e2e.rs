//! Multi-store scenario #3 -- a social feed: follow graphs in Sets
//! (SINTER common follows, set_e2e), a push-model inbox in Lists with
//! an LTRIM cap (list_e2e), a hot-post leaderboard in ZSETs (zset_e2e),
//! and full-text post search through FT.CREATE/FT.ADD/FT.SEARCH with
//! Chinese segmentation (search_e2e -- jieba on both index and query
//! side). One REAL single-node bootstrap, RESP only.

mod common;
mod scenario_common;

use scenario_common::{bulk_at, front_world, int_of, ok, r, rf, rb, FrontSpec};

#[tokio::test]
async fn social_feed_graph_inbox_leaderboard_search() {
    let (_front, a) = front_world("feed3", &FrontSpec::default()).await;

    // a) Follow graph: Sets per user. alice follows {bob, carol, dave},
    //    bob follows {alice, carol, dave} -> common follows {carol, dave};
    //    carol's fan set has exactly alice+bob.
    assert_eq!(int_of(&r(&a, &[b"SADD", b"{sf}follows:alice", b"bob", b"carol", b"dave"]).await), 3);
    assert_eq!(int_of(&r(&a, &[b"SADD", b"{sf}follows:bob", b"alice", b"carol", b"dave"]).await), 3);
    assert_eq!(int_of(&r(&a, &[b"SADD", b"follows:carol", b"alice"]).await), 1);
    let common = rf(&a, &[b"SINTER", b"{sf}follows:alice", b"{sf}follows:bob"]).await;
    assert!(common.contains("carol") && common.contains("dave") && !common.contains("alice"), "{common}");
    assert_eq!(int_of(&r(&a, &[b"SADD", b"fans:carol", b"alice", b"bob"]).await), 2);
    assert_eq!(int_of(&r(&a, &[b"SCARD", b"fans:carol"]).await), 2);

    // b) Push model: carol's three posts fan out to her fans' List
    //    inboxes; the inbox keeps only the latest five (LTRIM cap).
    for post in ["post:c1", "post:c2", "post:c3"] {
        int_of(&r(&a, &[b"LPUSH", b"feed:alice", post.as_bytes()]).await);
        int_of(&r(&a, &[b"LPUSH", b"feed:bob", post.as_bytes()]).await);
    }
    for noise in ["post:x1", "post:x2", "post:x3"] {
        int_of(&r(&a, &[b"LPUSH", b"feed:alice", noise.as_bytes()]).await);
    }
    ok(&r(&a, &[b"LTRIM", b"feed:alice", b"0", b"4"]).await, "cap inbox");
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"feed:alice"]).await), 5);
    let feed = rf(&a, &[b"LRANGE", b"feed:alice", b"0", b"-1"]).await;
    assert!(!feed.contains("post:c1"), "oldest real post evicted by cap: {feed}");
    assert!(feed.contains("post:x3") && feed.contains("post:c2"), "{feed}");

    // c) Hot board: likes bump the ZSET; the top range reads back in
    //    descending-score order.
    for (post, likes, want) in [("post:c1", "5", "5"), ("post:c2", "2", "2"), ("post:c3", "9", "9")] {
        let reply = r(&a, &[b"ZINCRBY", b"hot:posts", likes.as_bytes(), post.as_bytes()]).await;
        assert_eq!(bulk_at(&reply, 0), want, "{post}");
    }
    let top = rf(&a, &[b"ZREVRANGE", b"hot:posts", b"0", b"0", b"WITHSCORES"]).await;
    assert!(top.contains("post:c3") && top.contains("9"), "{top}");
    assert_eq!(
        rb(&a, &[b"ZSCORE", b"hot:posts", b"post:c1"]).await,
        "5"
    );

    // d) Post search: an index over title+body, three docs, a Chinese
    //    query term segmented the same on both sides.
    ok(
        &r(&a, &[b"FT.CREATE", b"idx:posts", b"SCHEMA", b"title", b"TEXT", b"body", b"TEXT"]).await,
        "ft.create",
    );
    let add = |doc: &str, json: &str| {
        let a = a.clone();
        let doc = doc.to_string();
        let json = json.to_string();
        async move {
            let d = doc.as_bytes();
            let j = json.as_bytes();
            r(&a, &[b"FT.ADD", b"idx:posts", d, j]).await
        }
    };
    assert_eq!(
        int_of(
            &add(
                "p:c1",
                r#"{"title":"露营攻略","body":"周末去山里露营看星星"}"#
            )
            .await
        ),
        1
    );
    assert_eq!(
        int_of(
            &add(
                "p:c2",
                r#"{"title":"咖啡地图","body":"城中新开的咖啡店"}"#
            )
            .await
        ),
        1
    );
    assert_eq!(
        int_of(
            &add("p:c3", r#"{"title":"咖啡露营","body":"露营咖啡壶"}"#).await
        ),
        1
    );
    let hits = rf(&a, &[b"FT.SEARCH", b"idx:posts", "@title:咖啡".as_bytes()]).await;
    assert!(hits.contains(":2\r\n"), "two coffee titles: {hits:?}");
    assert_eq!(bulk_at(hits.as_bytes(), 0), "p:c3", "tf=2 title ranks first: {hits:?}");
    let zh = rf(&a, &[b"FT.SEARCH", b"idx:posts", "@body:露营".as_bytes()]).await;
    assert!(zh.contains(":2\r\n") && zh.contains("p:c1"), "{zh:?}");

    // e) Cross-check: the leaderboard's membership equals the fan-out
    //    posts still visible plus evicted history (board keeps all).
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"hot:posts"]).await), 3);
}
