//! `MIGRATE` subcommands (Go `internal/command/migrate.go`).
//!
//! Tasks are replicated through the raft key `migrate_task`.

use crate::command::Ctx;
use crate::resp::codec::{append_array, append_bulk_string, append_error, append_string};
use crate::rtypes;
use crate::state;

/// Raft key holding the current migrate task.
const TASK_KEY: &str = "migrate_task";

/// `MIGRATE ...` dispatch; lowercase-only entries, exactly as Go's map.
pub async fn handle(ctx: &mut Ctx<'_>) {
    let Some(first) = ctx.args.first() else {
        migrate_helper(ctx);
        return;
    };
    match first.as_slice() {
        b"help" => migrate_helper(ctx),
        b"task" => migrate_task(ctx).await,
        b"list" => migrate_list(ctx),
        _ => migrate_helper(ctx),
    }
}

/// Go quirk kept: the usage message is an ERROR reply.
fn migrate_helper(ctx: &mut Ctx<'_>) {
    append_error(ctx.out, "migrate [ list | task ]");
}

fn migrate_list(ctx: &mut Ctx<'_>) {
    let raft = ctx.shared.raft.read().unwrap();
    let val = state::raft_get(&raft, TASK_KEY);
    // Go strings.Split: an empty value yields one empty item.
    let items: Vec<&str> = val.split(',').collect();
    append_array(ctx.out, items.len());
    for item in items {
        append_bulk_string(ctx.out, &item.replace('_', " "));
    }
}

async fn migrate_task(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 4 {
        migrate_helper(ctx);
        return;
    }
    let value = format!(
        "{}_{}_{}",
        String::from_utf8_lossy(&ctx.args[1]),
        String::from_utf8_lossy(&ctx.args[2]),
        String::from_utf8_lossy(&ctx.args[3])
    );
    // Go reads the existing task and builds `tasks += "," + val`, but that
    // string is never used: the applied entry carries `val` alone, so the
    // effective semantics are OVERWRITE. Kept as overwrite (dead append
    // dropped).
    let entry = rtypes::RaftLogEntryData {
        key: TASK_KEY.to_string(),
        value,
    };
    // Go applies with a 5s timeout; the stub applies synchronously. The
    // write guard covers only the non-blocking start and is DROPPED
    // before the await (see raft_set).
    let started = {
        let mut raft = ctx.shared.raft.write().unwrap();
        state::raft_apply_start(&mut raft, &entry)
    };
    let result = match started {
        Ok(ticket) => state::raft_apply_await(ticket).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => append_string(ctx.out, "OK"),
        Err(_) => append_error(ctx.out, "Raft Apply failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::test_ctx;
    use crate::state::{testutil, Shared};

    /// Every store-opening test holds the crate-wide lock for its whole
    /// lifetime (see `string::tests`): `shared_with` wipes the shared
    /// `/tmp/rdb-test-{pid}` root. Guard returned FIRST so it outlives the
    /// Shared (locals drop in reverse declaration order).
    fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
        let guard = crate::command::string::TEST_STORE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut conf = testutil::test_config();
        conf.bind = bind.to_string();
        (guard, testutil::shared_with(conf))
    }

    fn call(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
        let mut ctx = test_ctx(shared, vec![], argv, &mut out);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(handle(&mut ctx));
        out
    }

    #[test]
    fn helper_is_an_error_reply() {
        let (_guard, shared) = shared_for("127.0.0.1:40401");
        let helper = b"-migrate [ list | task ]\r\n";
        assert_eq!(call(&shared, &[]), helper);
        assert_eq!(call(&shared, &[b"help"]), helper);
        assert_eq!(call(&shared, &[b"bogus"]), helper);
        // Go has no uppercase entries: "TASK" also falls to the helper.
        assert_eq!(call(&shared, &[b"TASK"]), helper);
    }

    #[test]
    fn task_then_list_replaces_underscores() {
        let (_guard, shared) = shared_for("127.0.0.1:40402");
        assert_eq!(call(&shared, &[b"task", b"a", b"b", b"c"]), b"+OK\r\n");
        assert_eq!(call(&shared, &[b"list"]), b"*1\r\n$5\r\na b c\r\n");
    }

    #[test]
    fn second_task_overwrites_first() {
        let (_guard, shared) = shared_for("127.0.0.1:40403");
        assert_eq!(call(&shared, &[b"task", b"a", b"b", b"c"]), b"+OK\r\n");
        assert_eq!(call(&shared, &[b"task", b"d", b"e", b"f"]), b"+OK\r\n");
        // Go's append is dead code; the applied value overwrites, so the
        // list still holds exactly one item.
        assert_eq!(call(&shared, &[b"list"]), b"*1\r\n$5\r\nd e f\r\n");
    }

    #[test]
    fn task_arity_falls_back_to_helper() {
        let (_guard, shared) = shared_for("127.0.0.1:40404");
        assert_eq!(
            call(&shared, &[b"task", b"a", b"b"]),
            b"-migrate [ list | task ]\r\n"
        );
    }

    #[test]
    fn list_without_task_is_one_empty_item() {
        let (_guard, shared) = shared_for("127.0.0.1:40405");
        // Go strings.Split("", ",") == [""]; the quirk is preserved.
        assert_eq!(call(&shared, &[b"list"]), b"*1\r\n$0\r\n\r\n");
    }
}
