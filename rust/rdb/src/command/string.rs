//! String / basic commands (Go `internal/command/string.go` + `othes.go`).

use std::sync::Arc;

use crate::command::Ctx;
use crate::resp::codec::{
    append_array, append_bulk, append_bulk_string, append_error, append_int, append_null,
    append_string,
};
use crate::store;

/// `PING` -> `+PONG`.
pub async fn ping(ctx: &mut Ctx<'_>) {
    append_string(ctx.out, "PONG");
}

/// `QUIT`: reply `+PONG` then `+OK` and ask for the connection to close
/// (Go quitHandler writes PONG, OK then closes the conn).
pub async fn quit(ctx: &mut Ctx<'_>) {
    append_string(ctx.out, "PONG");
    append_string(ctx.out, "OK");
    ctx.close_conn = true;
}

/// `CONFIG ...`: all arguments ignored; always the same two bulk strings.
pub async fn config(ctx: &mut Ctx<'_>) {
    append_array(ctx.out, 2);
    append_bulk_string(ctx.out, "cluster-require-full-coverage");
    append_bulk_string(ctx.out, "no");
}

/// `GET key`: raw string first, then the enveloped STRING_TTL record
/// (written by EXPIRE) with lazy expiry; any store error replies null bulk.
pub async fn get(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        append_error(ctx.out, "ERR wrong number of arguments for get command");
        return;
    }
    if let Ok(Some(val)) = store::get(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0]) {
        append_bulk(ctx.out, &val);
        return;
    }
    match crate::ds::expire::read_enveloped(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0]) {
        Ok(Some((_, payload))) => append_bulk(ctx.out, &payload),
        _ => append_null(ctx.out),
    }
}

/// `SET key value`.
pub async fn set(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        append_error(ctx.out, "ERR wrong number of arguments for set command");
        return;
    }
    // Only owned data crosses into the 'static spawn_blocking closure:
    // take the arg buffers instead of cloning them.
    let key = std::mem::take(&mut ctx.args[0]);
    let val = std::mem::take(&mut ctx.args[1]);
    let res = store::set_async(
        Arc::clone(&ctx.shared.store),
        ctx.prefix_key.clone(),
        key,
        val,
    )
    .await;
    match res {
        Ok(()) => append_string(ctx.out, "OK"),
        Err(_) => append_error(ctx.out, "ERR: set key failed"),
    }
}

/// `DEL key`.
///
/// AGREED BUG FIX: Go discarded pebble's Del error and always replied `:1`.
/// Rust reports the truth: `:1` only when the key existed and was removed,
/// `:0` otherwise (missing key or store error).
pub async fn del(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        append_error(ctx.out, "ERR wrong number of arguments for del command");
        return;
    }
    let key = std::mem::take(&mut ctx.args[0]);
    let res = store::del_async(Arc::clone(&ctx.shared.store), ctx.prefix_key.clone(), key).await;
    append_int(ctx.out, i64::from(matches!(res, Ok(true))));
}

/// `MGET key [key ...]`: missing raw entries fall back to enveloped
/// STRING_TTL records (lazy-expired); absent keys render as null bulks.
pub async fn mget(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        append_error(
            ctx.out,
            "MGET command must have at least 1 argument: MGET <key1> [<key2> ...]",
        );
        return;
    }
    let data = store::mget(&ctx.shared.store, &ctx.prefix_key, &ctx.args);
    append_array(ctx.out, data.len());
    for (i, val) in data.iter().enumerate() {
        if !val.is_empty() {
            append_bulk(ctx.out, val);
            continue;
        }
        match crate::ds::expire::read_enveloped(&ctx.shared.store, &ctx.prefix_key, &ctx.args[i]) {
            Ok(Some((_, payload))) => append_bulk(ctx.out, &payload),
            _ => append_null(ctx.out),
        }
    }
}

/// `MSET key value [key value ...]`.
///
/// AGREED BUG FIX: Go wrote the arity error but then fell through into
/// MSet, which panics on an odd-length slice. Rust returns right after the
/// error instead.
pub async fn mset(ctx: &mut Ctx<'_>) {
    if !ctx.args.len().is_multiple_of(2) {
        append_error(
            ctx.out,
            &format!("ERR wrong number of arguments: {}", ctx.args.len()),
        );
        return;
    }
    let pairs = std::mem::take(&mut ctx.args);
    let res = store::mset_async(Arc::clone(&ctx.shared.store), ctx.prefix_key.clone(), pairs).await;
    match res {
        Ok(()) => append_string(ctx.out, "OK"),
        Err(_) => append_error(ctx.out, "ERR: set key failed"),
    }
}

#[cfg(test)]
pub(crate) static TEST_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::test_ctx;
    use crate::state::{testutil, Shared};

    const PREFIX: &[u8] = b"70/";

    /// `state::testutil::shared_with` wipes `/tmp/rdb-test-{pid}` wholesale,
    /// so every test that opens a store must hold the crate-wide lock for
    /// its whole lifetime. Guard is returned FIRST so it outlives the
    /// Shared (locals drop in reverse declaration order).
    fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
        let guard = TEST_STORE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut conf = testutil::test_config();
        conf.bind = bind.to_string();
        (guard, testutil::shared_with(conf))
    }

    fn call(shared: &Shared, handler: crate::command::Handler, args: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
        let mut ctx = test_ctx(shared, PREFIX.to_vec(), argv, &mut out);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(handler(&mut ctx));
        out
    }

    #[test]
    fn ping_quit_config_replies() {
        let (_guard, shared) = shared_for("127.0.0.1:40101");
        assert_eq!(call(&shared, |ctx| Box::pin(ping(ctx)), &[]), b"+PONG\r\n");

        let mut out = Vec::new();
        {
            let mut ctx = test_ctx(&shared, vec![], vec![], &mut out);
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("test runtime")
                .block_on(quit(&mut ctx));
            assert!(ctx.close_conn);
        }
        assert_eq!(out, b"+PONG\r\n+OK\r\n");

        // "cluster-require-full-coverage" is 29 bytes long.
        assert_eq!(
            call(&shared, |ctx| Box::pin(config(ctx)), &[b"ignored", b"args"]),
            b"*2\r\n$29\r\ncluster-require-full-coverage\r\n$2\r\nno\r\n"
        );
    }

    #[test]
    fn set_get_del_roundtrip_with_del_bug_fix() {
        let (_guard, shared) = shared_for("127.0.0.1:40102");
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &[b"k", b"v"]),
            b"+OK\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(get(ctx)), &[b"k"]),
            b"$1\r\nv\r\n"
        );
        assert_eq!(call(&shared, |ctx| Box::pin(del(ctx)), &[b"k"]), b":1\r\n");
        assert_eq!(call(&shared, |ctx| Box::pin(get(ctx)), &[b"k"]), b"$-1\r\n");
        // Bug fix: Go always answered 1; missing keys now answer 0.
        assert_eq!(call(&shared, |ctx| Box::pin(del(ctx)), &[b"k"]), b":0\r\n");
    }

    #[test]
    fn get_set_del_arity_errors() {
        let (_guard, shared) = shared_for("127.0.0.1:40103");
        assert_eq!(
            call(&shared, |ctx| Box::pin(get(ctx)), &[]),
            b"-ERR wrong number of arguments for get command\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(get(ctx)), &[b"a", b"b"]),
            b"-ERR wrong number of arguments for get command\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &[b"only-key"]),
            b"-ERR wrong number of arguments for set command\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(del(ctx)), &[]),
            b"-ERR wrong number of arguments for del command\r\n"
        );
    }

    #[test]
    fn mget_null_for_missing_and_empty() {
        let (_guard, shared) = shared_for("127.0.0.1:40104");
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &[b"a", b"1"]),
            b"+OK\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &[b"b", b"2"]),
            b"+OK\r\n"
        );
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(mget(ctx)),
                &[b"a", b"missing", b"b"]
            ),
            b"*3\r\n$1\r\n1\r\n$-1\r\n$1\r\n2\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(mget(ctx)), &[]),
            b"-MGET command must have at least 1 argument: MGET <key1> [<key2> ...]\r\n"
        );
    }

    #[test]
    fn mset_even_success_odd_error_returned() {
        let (_guard, shared) = shared_for("127.0.0.1:40105");
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(mset(ctx)),
                &[b"a", b"1", b"b", b"2"]
            ),
            b"+OK\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(mget(ctx)), &[b"a", b"b"]),
            b"*2\r\n$1\r\n1\r\n$1\r\n2\r\n"
        );
        // Bug fix: Go fell through into a panic after writing this error;
        // Rust returns cleanly with the error as the only reply.
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(mset(ctx)),
                &[b"a", b"1", b"dangling"]
            ),
            b"-ERR wrong number of arguments: 3\r\n"
        );
    }
}
