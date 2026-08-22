//! `RAFT` subcommands (Go `internal/command/raft.go`).

use crate::command::Ctx;
use crate::resp::codec::{
    append_array, append_bulk_string, append_error, append_null, append_string,
};
use crate::rtypes;
use crate::state;

/// `RAFT ...` dispatch; unknown subcommands fall back to help.
pub async fn handle(ctx: &mut Ctx<'_>) {
    let Some(first) = ctx.args.first() else {
        raft_help(ctx);
        return;
    };
    match first.as_slice() {
        b"help" | b"HELP" => raft_help(ctx),
        b"stats" | b"STATS" => raft_stats(ctx),
        b"leader" | b"LEADER" => raft_leader(ctx),
        b"NODES" | b"nodes" => raft_nodes(ctx),
        b"set" | b"SET" => raft_set(ctx).await,
        b"get" | b"GET" => raft_get_cmd(ctx),
        _ => raft_help(ctx),
    }
}

fn raft_help(ctx: &mut Ctx<'_>) {
    append_string(ctx.out, "raft [ help | stats | nodes | leader ]");
}

fn raft_stats(ctx: &mut Ctx<'_>) {
    let raft = ctx.shared.raft.read().unwrap();
    append_array(ctx.out, raft.stats.len());
    for (k, v) in &raft.stats {
        append_bulk_string(ctx.out, &format!("{k}: {v}"));
    }
}

fn raft_leader(ctx: &mut Ctx<'_>) {
    let raft = ctx.shared.raft.read().unwrap();
    append_string(ctx.out, &format!("raft addr: {}", raft.leader_addr));
}

fn raft_nodes(ctx: &mut Ctx<'_>) {
    let raft = ctx.shared.raft.read().unwrap();
    let latest = state::raft_stats_get(&raft, "latest_configuration");
    append_string(ctx.out, &format!("{}, nodes: {}", raft.node_desc, latest));
}

async fn raft_set(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        append_error(ctx.out, "ERR wrong number of arguments for set command");
        return;
    }
    let entry = rtypes::RaftLogEntryData {
        key: String::from_utf8_lossy(&ctx.args[1]).into_owned(),
        value: String::from_utf8_lossy(&ctx.args[2]).into_owned(),
    };
    // Go applies with a 5s timeout; the apply loop enforces it (the stub
    // applies synchronously). The write guard covers only the non-blocking
    // start and is DROPPED before the await so `shared.raft` stays
    // readable while the apply is in flight.
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
        Err(e) => append_error(ctx.out, &format!("internal error err: {e}")),
    }
}

fn raft_get_cmd(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        append_error(ctx.out, "ERR wrong number of arguments for get command");
        return;
    }
    let key = String::from_utf8_lossy(&ctx.args[1]);
    let raft = ctx.shared.raft.read().unwrap();
    let val = state::raft_get(&raft, &key);
    if val.is_empty() {
        append_null(ctx.out);
    } else {
        // BREAKING (approved): bulk string, not Go's WriteString simple
        // string -- a value containing CRLF would corrupt the RESP frame.
        append_bulk_string(ctx.out, &val);
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
    fn help_for_empty_unknown_and_uppercase() {
        let (_guard, shared) = shared_for("127.0.0.1:40301");
        let help = b"+raft [ help | stats | nodes | leader ]\r\n";
        assert_eq!(call(&shared, &[]), help);
        assert_eq!(call(&shared, &[b"HELP"]), help);
        assert_eq!(call(&shared, &[b"bogus"]), help);
    }

    #[test]
    fn stats_leader_nodes_replies() {
        let (_guard, shared) = shared_for("127.0.0.1:40302");
        let stats = call(&shared, &[b"stats"]);
        assert!(stats.starts_with(b"*9\r\n"), "nine stat rows expected");
        assert!(
            stats.windows(18).any(|w| w == b"$13\r\nstate: Leader"),
            "stats must contain the state row"
        );
        assert_eq!(
            call(&shared, &[b"leader"]),
            b"+raft addr: 127.0.0.1:22681\r\n"
        );
        // node_desc follows Go hashicorp Raft.String(): "<addr> [<State>]".
        assert_eq!(
            call(&shared, &[b"NODES"]),
            b"+127.0.0.1:22681 [Leader], nodes: \
              [{Suffrage:Voter ID:127.0.0.1:22681 Address:127.0.0.1:22681}]\r\n"
        );
    }

    #[test]
    fn set_get_roundtrip_bulk_reply() {
        let (_guard, shared) = shared_for("127.0.0.1:40303");
        assert_eq!(call(&shared, &[b"set", b"k", b"v"]), b"+OK\r\n");
        // BREAKING (approved): the value replies as a BULK string, not the
        // Go simple string (CRLF values would break RESP framing).
        assert_eq!(call(&shared, &[b"get", b"k"]), b"$1\r\nv\r\n");
        assert_eq!(call(&shared, &[b"GET", b"k"]), b"$1\r\nv\r\n");
        assert_eq!(call(&shared, &[b"get", b"missing"]), b"$-1\r\n");
        assert_eq!(
            call(&shared, &[b"set", b"only-key"]),
            b"-ERR wrong number of arguments for set command\r\n"
        );
        assert_eq!(
            call(&shared, &[b"get"]),
            b"-ERR wrong number of arguments for get command\r\n"
        );
    }

    #[test]
    fn get_value_containing_crlf_keeps_bulk_framing() {
        let (_guard, shared) = shared_for("127.0.0.1:40305");
        // "a\r\nb" is 4 bytes; the bulk header must frame the embedded CRLF.
        assert_eq!(call(&shared, &[b"set", b"k", b"a\r\nb"]), b"+OK\r\n");
        assert_eq!(call(&shared, &[b"get", b"k"]), b"$4\r\na\r\nb\r\n");
        // A trailing-CRLF value is likewise length-prefixed, not +line.
        assert_eq!(call(&shared, &[b"set", b"c", b"x\r\n"]), b"+OK\r\n");
        assert_eq!(call(&shared, &[b"get", b"c"]), b"$3\r\nx\r\n\r\n");
    }

    #[test]
    fn set_not_leader_error() {
        let (_guard, shared) = shared_for("127.0.0.1:40304");
        shared.raft.write().unwrap().is_leader = false;
        assert_eq!(
            call(&shared, &[b"set", b"k", b"v"]),
            b"-internal error err: not leader\r\n"
        );
    }
}
