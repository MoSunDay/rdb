//! `CLUSTER` subcommands (Go `internal/command/cluster.go`).
//!
//! Topology state lives in `ctx.shared.topology` (Rust mirror of Go's
//! `conf.Content.ClusterReady` / `StableAddrs`, which the rcache sync loop
//! refreshes from the raft key `cluster_slots_stable_instances`).

use std::time::{SystemTime, UNIX_EPOCH};

use crate::command::Ctx;
use crate::resp::codec::{
    append_array, append_bulk_string, append_error, append_int, append_string,
};
use crate::rtypes;
use crate::state;
use crate::topology;
use crate::utils;

/// `CLUSTER ...` dispatch.
///
/// Gate first, exactly as Go: before the cluster is ready only `init`/`INIT`
/// may run; every other subcommand (including `help`) is rejected.
pub async fn handle(ctx: &mut Ctx<'_>) {
    let ready = ctx.shared.topology.read().unwrap().cluster_ready;
    if !ready {
        if let Some(first) = ctx.args.first() {
            if first.as_slice() != b"init" && first.as_slice() != b"INIT" {
                append_error(
                    ctx.out,
                    "cluster not ready, initialize the cluster with this command \
                     (cluster init [instanes01,instanes02,instance03])",
                );
                return;
            }
        }
    }
    let Some(first) = ctx.args.first() else {
        cluster_help(ctx);
        return;
    };
    match first.as_slice() {
        b"help" => cluster_help(ctx),
        b"INIT" | b"init" => cluster_init(ctx).await,
        b"info" | b"INFO" => cluster_info(ctx),
        b"nodes" | b"NODES" => cluster_nodes(ctx),
        b"slots" | b"SLOTS" => cluster_slots(ctx),
        b"test" => cluster_test(ctx),
        _ => cluster_help(ctx),
    }
}

fn cluster_help(ctx: &mut Ctx<'_>) {
    append_string(ctx.out, "cluster [ help | nodes | slots | test ]");
}

fn cluster_info(ctx: &mut Ctx<'_>) {
    let topo = ctx.shared.topology.read().unwrap();
    let ready = topo.cluster_ready;
    let size = topo.stable_addrs.len();
    let raft = ctx.shared.raft.read().unwrap();
    // Go concatenates the stat strings: epoch = stats["term"] + stats["commit_index"].
    let epoch = format!(
        "{}{}",
        state::raft_stats_get(&raft, "term"),
        state::raft_stats_get(&raft, "commit_index")
    );
    let body = format!(
        "cluster_state:{ready}\n\
         cluster_slots_assigned:16384\n\
         cluster_slots_ok:16384\n\
         cluster_slots_pfail:0\n\
         cluster_slots_fail:0\n\
         cluster_known_nodes:{size}\n\
         cluster_size:{size}\n\
         cluster_current_epoch:{epoch}\n\
         cluster_my_epoch:{epoch}\n\
         cluster_stats_messages_sent:0\n\
         cluster_stats_messages_received:0\n"
    );
    append_bulk_string(ctx.out, &body);
}

fn cluster_nodes(ctx: &mut Ctx<'_>) {
    let topo = ctx.shared.topology.read().unwrap();
    let addrs = topo.stable_addrs.clone();
    drop(topo);
    let node_slots = topology::parse_node_slots(&addrs);
    let mut response = String::new();
    for addr in &addrs {
        let port = addr.split(':').nth(1).unwrap_or("");
        let uuid = utils::md5_with40(addr);
        let flag = if *addr == ctx.shared.conf.bind {
            "myself,"
        } else {
            ""
        };
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let slots = node_slots.get(addr).map(String::as_str).unwrap_or("");
        response.push_str(&format!(
            "{uuid} {addr}@{port} {flag}master - 0 {timestamp_ms} 1 connected {slots}\r\n"
        ));
    }
    append_bulk_string(ctx.out, &response);
}

fn cluster_slots(ctx: &mut Ctx<'_>) {
    let topo = ctx.shared.topology.read().unwrap();
    let addrs = topo.stable_addrs.clone();
    drop(topo);
    let node_slots = topology::parse_node_slots(&addrs);
    append_array(ctx.out, addrs.len());
    for addr in &addrs {
        append_array(ctx.out, 3);
        let mut range = node_slots
            .get(addr)
            .map(String::as_str)
            .unwrap_or("")
            .split('-');
        let start = parse_or_zero(range.next());
        let end = parse_or_zero(range.next());
        let mut parts = addr.split(':');
        let ip = parts.next().unwrap_or("");
        let port = parse_or_zero(parts.next());
        append_int(ctx.out, start);
        append_int(ctx.out, end);
        append_array(ctx.out, 3);
        append_bulk_string(ctx.out, ip);
        append_int(ctx.out, port);
        append_bulk_string(ctx.out, &utils::md5_with40(addr));
    }
}

/// Go ignores the ParseInt error and uses the zero value on failure.
fn parse_or_zero(s: Option<&str>) -> i64 {
    s.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0)
}

/// Known quirk kept: always the same hardcoded MOVED reply.
fn cluster_test(ctx: &mut Ctx<'_>) {
    append_error(ctx.out, "MOVED 5465 127.0.0.1:32681");
}

async fn cluster_init(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        append_error(ctx.out, "cluster init [instances]");
        return;
    }
    // Go: snapshot the leader via LeaderWithID() and refuse when this node
    // is not the leader.
    {
        let raft = ctx.shared.raft.read().unwrap();
        if ctx.shared.conf.raft_tcp_address != raft.leader_addr {
            append_error(ctx.out, &format!("Leader addr: {}", raft.leader_addr));
            return;
        }
    }
    let entry = rtypes::RaftLogEntryData {
        key: "cluster_slots_stable_instances".to_string(),
        value: String::from_utf8_lossy(&ctx.args[1]).into_owned(),
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
        Ok(()) => append_string(ctx.out, "done"),
        Err(_) => append_error(ctx.out, "Raft Apply failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::test_ctx;
    use crate::state::{testutil, Shared};

    const INSTANCES: &str = "127.0.0.1:32681,127.0.0.1:32683,127.0.0.1:32685";

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

    /// Unwrap the payload of a single bulk frame.
    fn bulk_payload(out: &[u8]) -> Vec<u8> {
        let header_end = out.windows(2).position(|w| w == b"\r\n").unwrap();
        let len: usize = std::str::from_utf8(&out[1..header_end])
            .unwrap()
            .parse()
            .unwrap();
        out[header_end + 2..header_end + 2 + len].to_vec()
    }

    #[test]
    fn gate_blocks_non_init_until_ready() {
        let (_guard, shared) = shared_for("127.0.0.1:40201");
        let err = b"-cluster not ready, initialize the cluster with this command \
                    (cluster init [instanes01,instanes02,instance03])\r\n";
        assert_eq!(call(&shared, &[b"info"]), err);
        assert_eq!(call(&shared, &[b"NODES"]), err);
        // Empty args bypass the gate and reach help.
        assert_eq!(
            call(&shared, &[]),
            b"+cluster [ help | nodes | slots | test ]\r\n"
        );
        // Init arity error still passes the gate.
        assert_eq!(call(&shared, &[b"init"]), b"-cluster init [instances]\r\n");
    }

    #[test]
    fn init_applies_instances_and_replies_done() {
        let (_guard, shared) = shared_for("127.0.0.1:40202");
        assert_eq!(
            call(&shared, &[b"init", INSTANCES.as_bytes()]),
            b"+done\r\n"
        );
        let raft = shared.raft.read().unwrap();
        assert_eq!(
            state::raft_get(&raft, "cluster_slots_stable_instances"),
            INSTANCES
        );
    }

    #[test]
    fn init_refused_when_not_leader() {
        let (_guard, shared) = shared_for("127.0.0.1:40203");
        shared.raft.write().unwrap().leader_addr = "10.0.0.1:22681".to_string();
        assert_eq!(
            call(&shared, &[b"init", INSTANCES.as_bytes()]),
            b"-Leader addr: 10.0.0.1:22681\r\n"
        );
    }

    #[test]
    fn info_bulk_exact_after_init() {
        let (_guard, shared) = shared_for("127.0.0.1:40204");
        assert_eq!(
            call(&shared, &[b"init", INSTANCES.as_bytes()]),
            b"+done\r\n"
        );
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        // One apply: term "1" + commit_index "1" -> epoch "11".
        let body = concat!(
            "cluster_state:true\n",
            "cluster_slots_assigned:16384\n",
            "cluster_slots_ok:16384\n",
            "cluster_slots_pfail:0\n",
            "cluster_slots_fail:0\n",
            "cluster_known_nodes:3\n",
            "cluster_size:3\n",
            "cluster_current_epoch:11\n",
            "cluster_my_epoch:11\n",
            "cluster_stats_messages_sent:0\n",
            "cluster_stats_messages_received:0\n",
        );
        let mut expected = format!("${}\r\n", body.len()).into_bytes();
        expected.extend_from_slice(body.as_bytes());
        expected.extend_from_slice(b"\r\n");
        assert_eq!(call(&shared, &[b"info"]), expected);
        assert_eq!(
            call(&shared, &[b"bogus"]),
            b"+cluster [ help | nodes | slots | test ]\r\n"
        );
    }

    #[test]
    fn nodes_three_lines_uuids_myself_and_slot_ranges() {
        let (_guard, shared) = shared_for("127.0.0.1:32681");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        let payload = bulk_payload(&call(&shared, &[b"nodes"]));
        let text = String::from_utf8(payload).unwrap();
        let lines: Vec<&str> = text.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3);
        let uuid0 = utils::md5_with40("127.0.0.1:32681");
        assert!(lines[0].starts_with(&format!("{uuid0} 127.0.0.1:32681@32681 myself,master - 0 ")));
        assert!(lines[1].contains(" 127.0.0.1:32683@32683 master - 0 "));
        assert!(lines[2].contains(" 127.0.0.1:32685@32685 master - 0 "));
        assert!(lines[0].ends_with(" 1 connected 0-5461"));
        assert!(lines[1].ends_with(" 1 connected 5462-10922"));
        assert!(lines[2].ends_with(" 1 connected 10923-16383"));
    }

    #[test]
    fn slots_structure_byte_exact() {
        let (_guard, shared) = shared_for("127.0.0.1:40206");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        let mut expected = b"*3\r\n".to_vec();
        for (addr, range) in [
            ("127.0.0.1:32681", (0i64, 5461i64)),
            ("127.0.0.1:32683", (5462, 10922)),
            ("127.0.0.1:32685", (10923, 16383)),
        ] {
            let (ip, port) = addr.split_once(':').unwrap();
            let uuid = utils::md5_with40(addr);
            let node = format!(
                "*3\r\n:{}\r\n:{}\r\n*3\r\n${}\r\n{}\r\n:{}\r\n${}\r\n{}\r\n",
                range.0,
                range.1,
                ip.len(),
                ip,
                port,
                uuid.len(),
                uuid
            );
            expected.extend_from_slice(node.as_bytes());
        }
        assert_eq!(call(&shared, &[b"slots"]), expected);
    }

    #[test]
    fn slots_single_node_reports_full_zero_to_16383_range() {
        // Approved fix: a single-node cluster reports the full 0-16383
        // range (the Go display omitted slot 0 with "1-16383").
        let (_guard, shared) = shared_for("127.0.0.1:40208");
        *shared.topology.write().unwrap() = topology::refresh("127.0.0.1:32681");
        let uuid = utils::md5_with40("127.0.0.1:32681");
        let expected = format!(
            "*1\r\n*3\r\n:0\r\n:16383\r\n*3\r\n$9\r\n127.0.0.1\r\n:32681\r\n${}\r\n{}\r\n",
            uuid.len(),
            uuid
        );
        assert_eq!(call(&shared, &[b"slots"]), expected.into_bytes());
    }

    #[test]
    fn test_subcommand_always_moved() {
        let (_guard, shared) = shared_for("127.0.0.1:40207");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        assert_eq!(
            call(&shared, &[b"test"]),
            b"-MOVED 5465 127.0.0.1:32681\r\n"
        );
    }
}
