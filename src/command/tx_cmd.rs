//! MULTI / EXEC / DISCARD / WATCH / UNWATCH handlers.
//!
//! The queueing interception itself lives in `resp::conn` (it needs the
//! routing state); these handlers only manage the session state and run
//! the EXEC engine.

use crate::command::keys_core;
use crate::command::Ctx;
use crate::resp::codec;
use crate::tx::exec;
use crate::tx::session::{MultiState, WatchEntry};
use crate::tx::watch;

pub async fn multi(ctx: &mut Ctx<'_>) {
    if !ctx.shared.conf.tx.enabled {
        codec::append_error(ctx.out, "ERR transactions are disabled");
        return;
    }
    if ctx.conn.multi.is_some() {
        // Redis marks the transaction dirty on nested MULTI.
        ctx.conn.mark_dirty();
        codec::append_error(ctx.out, "ERR MULTI calls can not be nested");
        return;
    }
    ctx.conn.multi = Some(MultiState::new());
    codec::append_string(ctx.out, "OK");
}

pub async fn exec(ctx: &mut Ctx<'_>) {
    if ctx.conn.multi.is_none() {
        codec::append_error(ctx.out, "ERR EXEC without MULTI");
        return;
    }
    exec::run(
        ctx.shared,
        &mut *ctx.conn,
        &mut *ctx.out,
        &mut ctx.close_conn,
    )
    .await;
}

pub async fn discard(ctx: &mut Ctx<'_>) {
    if ctx.conn.multi.is_none() {
        codec::append_error(ctx.out, "ERR DISCARD without MULTI");
        return;
    }
    ctx.conn.reset_multi();
    // DISCARD unwatches, like EXEC (Redis `discardCommand`).
    ctx.conn.clear_watches();
    codec::append_string(ctx.out, "OK");
}

pub async fn watch(ctx: &mut Ctx<'_>) {
    if ctx.conn.multi.is_some() {
        ctx.conn.mark_dirty();
        codec::append_error(ctx.out, "ERR WATCH inside MULTI is not allowed");
        return;
    }
    if ctx.args.is_empty() {
        codec::append_error(ctx.out, "ERR wrong number of arguments for 'watch' command");
        return;
    }
    for key in &ctx.args {
        // Hash under the key's own latch: a concurrent RMW on this key is
        // either fully before (same hash we see) or fully after (different
        // hash -> abort at EXEC); no torn middle states. A lite stream
        // name has TWO windows -- the routed slot (a plain key of that
        // name) and its PARENT slot, where the node-local X-* verbs store
        // the stream (batch-14 closure, form A) -- and both are watched,
        // so an XADD on the stream dirties the transaction like any other
        // write; EXEC re-hashes every entry under its own prefix.
        let _guards = keys_core::lock_key_latches(ctx.shared, &ctx.prefix_key, key).await;
        let mut prefixes = vec![ctx.prefix_key.clone()];
        prefixes.extend(crate::lite::model::stream_key_target(key));
        for prefix in prefixes {
            let hash = watch::value_hash(&ctx.shared.store, &prefix, key);
            ctx.conn.watches.push(WatchEntry {
                prefix,
                key: key.clone(),
                hash,
            });
        }
    }
    codec::append_string(ctx.out, "OK");
}

pub async fn unwatch(ctx: &mut Ctx<'_>) {
    ctx.conn.clear_watches();
    codec::append_string(ctx.out, "OK");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::test_ctx_with_conn;
    use crate::state::testutil;
    use crate::tx::session::ConnState;

    fn shared() -> crate::state::Shared {
        testutil::shared_with(crate::conf::Config::default())
    }

    #[tokio::test]
    async fn multi_open_close_cycle() {
        let sh = shared();
        let mut conn = ConnState::default();
        let mut out = Vec::new();
        {
            let mut ctx = test_ctx_with_conn(&sh, b"42/".to_vec(), vec![], &mut out, &mut conn);
            multi(&mut ctx).await;
        }
        assert_eq!(out, b"+OK\r\n".to_vec());
        assert!(conn.in_multi());
        // nested -> dirty + error
        out.clear();
        {
            let mut ctx = test_ctx_with_conn(&sh, b"42/".to_vec(), vec![], &mut out, &mut conn);
            multi(&mut ctx).await;
        }
        assert_eq!(out, b"-ERR MULTI calls can not be nested\r\n".to_vec());
        assert!(conn.is_dirty());
        // discard resets and unwatches
        conn.watches.push(WatchEntry {
            prefix: vec![],
            key: b"k".to_vec(),
            hash: 0,
        });
        out.clear();
        {
            let mut ctx = test_ctx_with_conn(&sh, vec![], vec![], &mut out, &mut conn);
            discard(&mut ctx).await;
        }
        assert_eq!(out, b"+OK\r\n".to_vec());
        assert!(!conn.in_multi());
        assert!(conn.watches.is_empty());
        // discard without multi
        out.clear();
        {
            let mut ctx = test_ctx_with_conn(&sh, vec![], vec![], &mut out, &mut conn);
            discard(&mut ctx).await;
        }
        assert_eq!(out, b"-ERR DISCARD without MULTI\r\n".to_vec());
    }

    #[tokio::test]
    async fn watch_records_hashes_and_unwatch_clears() {
        let sh = shared();
        let mut conn = ConnState::default();
        let mut out = Vec::new();
        {
            let mut ctx = test_ctx_with_conn(
                &sh,
                b"42/".to_vec(),
                vec![b"key1".to_vec(), b"key2".to_vec()],
                &mut out,
                &mut conn,
            );
            watch(&mut ctx).await;
        }
        assert_eq!(out, b"+OK\r\n".to_vec());
        assert_eq!(conn.watches.len(), 2);
        assert_eq!(conn.watches[0].key, b"key1".to_vec());
        // same empty-store hash for both absent keys
        assert_eq!(conn.watches[0].hash, conn.watches[1].hash);
        out.clear();
        {
            let mut ctx = test_ctx_with_conn(&sh, vec![], vec![], &mut out, &mut conn);
            unwatch(&mut ctx).await;
        }
        assert_eq!(out, b"+OK\r\n".to_vec());
        assert!(conn.watches.is_empty());
    }

    #[tokio::test]
    async fn watch_covers_the_stream_window() {
        // A lite stream name is stored under its PARENT slot, not under
        // the routed slot of the full name: WATCH records BOTH windows,
        // and an XADD-shaped write under the parent slot changes the
        // re-hash exactly like any other write (batch-14 closure).
        let sh = shared();
        let name = b"orders/q0".to_vec();
        let routed = crate::hash::slot_with_prefix(&name).1;
        let parent = crate::lite::model::stream_key_target(&name).expect("stream name");
        assert_ne!(routed, parent);
        let mut conn = ConnState::default();
        let mut out = Vec::new();
        {
            let mut ctx = test_ctx_with_conn(
                &sh,
                routed.clone(),
                vec![name.clone()],
                &mut out,
                &mut conn,
            );
            watch(&mut ctx).await;
        }
        assert_eq!(out, b"+OK\r\n".to_vec());
        let prefixes: Vec<&[u8]> = conn.watches.iter().map(|w| w.prefix.as_slice()).collect();
        assert!(prefixes.contains(&routed.as_slice()));
        assert!(prefixes.contains(&parent.as_slice()));
        // Every entry is clean before the write.
        for w in &conn.watches {
            assert_eq!(watch::value_hash(&sh.store, &w.prefix, &w.key), w.hash);
        }
        sh.store
            .db
            .put(crate::lite::model::meta_key(&parent, &name), b"m")
            .unwrap();
        let stream_entry = conn
            .watches
            .iter()
            .find(|w| w.prefix == parent)
            .expect("parent window watched");
        assert_ne!(
            watch::value_hash(&sh.store, &stream_entry.prefix, &stream_entry.key),
            stream_entry.hash,
            "an XADD under the parent slot dirties the transaction"
        );
        // An ordinary key write under the routed slot dirties it too.
        sh.store
            .db
            .put(crate::ds::codec::string_key(&routed, &name), b"x")
            .unwrap();
        let routed_entry = conn
            .watches
            .iter()
            .find(|w| w.prefix == routed)
            .expect("routed window watched");
        assert_ne!(
            watch::value_hash(&sh.store, &routed_entry.prefix, &routed_entry.key),
            routed_entry.hash
        );
    }

    #[tokio::test]
    async fn watch_inside_multi_is_dirty() {
        let sh = shared();
        let mut conn = ConnState {
            multi: Some(MultiState::new()),
            ..ConnState::default()
        };
        let mut out = Vec::new();
        {
            let mut ctx = test_ctx_with_conn(&sh, vec![], vec![b"k".to_vec()], &mut out, &mut conn);
            watch(&mut ctx).await;
        }
        assert_eq!(out, b"-ERR WATCH inside MULTI is not allowed\r\n".to_vec());
        assert!(conn.is_dirty());
    }
}
