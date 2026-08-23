//! Slot-migration cluster subcommands (the `redis-cli --cluster reshard`
//! compatibility layer): `CLUSTER KEYSLOT`, `CLUSTER GETKEYSINSLOT`,
//! `CLUSTER SETSLOT MIGRATING|IMPORTING|STABLE|NODE` and the single-shot
//! `ASKING` flag. Split from `cluster.rs` to keep both files inside the
//! per-file line budget.

use crate::command::Ctx;
use crate::resp::codec::{append_array, append_error, append_int, append_string};
use crate::rtypes;
use crate::state;
use crate::topology;
use crate::utils;

/// follows an `ASK` redirection). Whitelisted: no slot routing of its own.
pub async fn asking(ctx: &mut Ctx<'_>) {
    ctx.conn.asking = true;
    append_string(ctx.out, "OK");
}
/// `CLUSTER KEYSLOT key`: the hash-tag slot of `key` (`keyHashSlot`).
pub fn cluster_key_slot(ctx: &mut Ctx<'_>) {
    let Some(key) = ctx.args.get(1) else {
        append_error(
            ctx.out,
            "ERR wrong number of arguments for 'cluster|keyslot' command",
        );
        return;
    };
    let (slot, _) = crate::hash::slot_with_prefix(crate::hash::hash_tag(key));
    append_int(ctx.out, slot as i64);
}
/// `CLUSTER GETKEYSINSLOT slot count`: up to `count` user keys physically
/// stored in `slot` on this node (the source side of a slot migration).
/// Mirrors Redis: top-level data keys only (no hash fields / list elems),
/// raw strings included, expire-index records skipped.
pub fn cluster_get_keys_in_slot(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        append_error(
            ctx.out,
            "ERR wrong number of arguments for 'cluster|getkeysinslot' command",
        );
        return;
    }
    let parse_u16 = |b: &[u8]| {
        std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
    };
    let Some(slot) = parse_u16(&ctx.args[1]) else {
        append_error(ctx.out, "ERR Invalid slot");
        return;
    };
    let count = parse_u16(&ctx.args[2]).map(|c| c as usize).unwrap_or(0);
    let prefix = crate::store::rocksdb::slot_prefix(slot);
    let prefix_len = prefix.len();
    let mut user_keys: Vec<Vec<u8>> = Vec::new();
    for pk in crate::store::ops::prefix_keys_collect(&ctx.shared.store, &prefix, count) {
        let after = &pk[prefix_len..];
        match crate::ds::codec::classify(after) {
            crate::ds::codec::Classification::Raw => user_keys.push(after.to_vec()),
            crate::ds::codec::Classification::Typed(_) => {
                if let Some((_, key, rest)) = crate::ds::codec::decode_data_key(&pk, prefix_len) {
                    if rest.is_empty() {
                        user_keys.push(key);
                    }
                }
            }
        }
    }
    append_array(ctx.out, user_keys.len());
    for k in &user_keys {
        crate::resp::codec::append_bulk(ctx.out, k);
    }
}
/// NODE records the new owner: it updates the local topology `owner_map`
/// immediately and, when this node is the raft leader, appends the full
/// map to the raft key so every node picks it up on the 3s topology sync.
/// Non-leader NODE is optimistic `+OK` (accepted risk): the leader gets
/// the same command from the migration orchestration and replicates it.
pub async fn cluster_set_slot(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        append_error(
            ctx.out,
            "ERR wrong number of arguments for 'cluster|setslot' command",
        );
        return;
    }
    let slot: u16 = match std::str::from_utf8(&ctx.args[1])
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(s) => s,
        None => {
            append_error(ctx.out, "ERR Invalid slot");
            return;
        }
    };
    let sub = ctx.args[2].to_ascii_uppercase();
    match sub.as_slice() {
        b"MIGRATING" => {
            let Some(id) = ctx.args.get(3) else {
                append_error(
                    ctx.out,
                    "ERR wrong number of arguments for 'cluster|setslot' command",
                );
                return;
            };
            let Some(addr) = node_id_to_addr(ctx, id) else {
                append_error(
                    ctx.out,
                    &format!("ERR Unknown node {}", String::from_utf8_lossy(id)),
                );
                return;
            };
            ctx.shared.migrating.write().unwrap().insert(slot, addr);
            append_string(ctx.out, "OK");
        }
        b"IMPORTING" => {
            let Some(id) = ctx.args.get(3) else {
                append_error(
                    ctx.out,
                    "ERR wrong number of arguments for 'cluster|setslot' command",
                );
                return;
            };
            let Some(addr) = node_id_to_addr(ctx, id) else {
                append_error(
                    ctx.out,
                    &format!("ERR Unknown node {}", String::from_utf8_lossy(id)),
                );
                return;
            };
            ctx.shared.importing.write().unwrap().insert(slot, addr);
            append_string(ctx.out, "OK");
        }
        b"STABLE" => {
            ctx.shared.migrating.write().unwrap().remove(&slot);
            ctx.shared.importing.write().unwrap().remove(&slot);
            append_string(ctx.out, "OK");
        }
        b"NODE" => {
            let Some(id) = ctx.args.get(3) else {
                append_error(
                    ctx.out,
                    "ERR wrong number of arguments for 'cluster|setslot' command",
                );
                return;
            };
            let Some(addr) = node_id_to_addr(ctx, id) else {
                append_error(
                    ctx.out,
                    &format!("ERR Unknown node {}", String::from_utf8_lossy(id)),
                );
                return;
            };
            let entry = {
                let mut topo = ctx.shared.topology.write().unwrap();
                topo.owner_map.insert(slot, addr.clone());
                drop(topo);
                ctx.shared.migrating.write().unwrap().remove(&slot);
                ctx.shared.importing.write().unwrap().remove(&slot);
                let topo = ctx.shared.topology.read().unwrap();
                rtypes::RaftLogEntryData {
                    key: topology::OWNER_MAP_KEY.to_string(),
                    value: topology::owner_map_json(&topo.owner_map),
                }
            };
            // Best-effort raft replication; not-leader falls back to the
            // optimistic local update above (accepted risk).
            let started = {
                let mut raft = ctx.shared.raft.write().unwrap();
                state::raft_apply_start(&mut raft, &entry)
            };
            if let Ok(ticket) = started {
                let _ = state::raft_apply_await(ticket).await;
            }
            append_string(ctx.out, "OK");
        }
        _ => {
            append_error(
                ctx.out,
                "ERR Unknown subcommand or wrong number of arguments for 'cluster|setslot'.                  Try CLUSTER SETSLOT <slot> (MIGRATING|IMPORTING|STABLE|NODE <node-id>).",
            );
        }
    }
}
/// Resolve a 40-hex node id (`md5_with40(addr)`) to its RESP addr in the
/// stable topology; `None` when unknown.
fn node_id_to_addr(ctx: &Ctx<'_>, id: &[u8]) -> Option<String> {
    let id = String::from_utf8_lossy(id);
    let topo = ctx.shared.topology.read().unwrap();
    topo.stable_addrs
        .iter()
        .find(|a| utils::md5_with40(a) == id.as_ref())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::test_ctx;
    use crate::state::{testutil, Shared};

    const INSTANCES: &str = "127.0.0.1:32681,127.0.0.1:32683,127.0.0.1:32685";

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
            .block_on(crate::command::cluster::handle(&mut ctx));
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
    fn keyslot_computes_hash_tag_slot() {
        let (_guard, shared) = shared_for("127.0.0.1:40209");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        // Well-known Redis slot values.
        assert_eq!(call(&shared, &[b"keyslot", b"key"]), b":12539\r\n");
        assert_eq!(call(&shared, &[b"keyslot", b"foo"]), b":12182\r\n");
        // Hash tags: the slot comes from the tagged part only.
        let tagged = call(&shared, &[b"keyslot", b"{user1000}.following"]);
        let tagless = call(&shared, &[b"keyslot", b"user1000"]);
        assert_eq!(tagged, tagless);
    }

    #[test]
    fn getkeysinslot_lists_only_top_level_keys() {
        let (_guard, shared) = shared_for("127.0.0.1:40210");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        // Seed two strings and one hash in the SAME slot via `{b}` (3300),
        // plus one string in a different slot.
        seed(&shared, b"{b}alpha", b"1");
        seed(&shared, b"{b}beta", b"2");
        let mut out = Vec::new();
        let (_, prefix) = crate::hash::slot_with_prefix(crate::hash::hash_tag(b"{b}h"));
        let argv = vec![b"{b}h".to_vec(), b"field".to_vec(), b"v".to_vec()];
        let mut ctx = test_ctx(&shared, prefix, argv, &mut out);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(crate::command::hash_cmd::hset(&mut ctx));
        seed(&shared, b"other", b"9");

        let out = call(&shared, &[b"getkeysinslot", b"3300", b"0"]);
        let mut keys = decode_bulk_array(&out);
        keys.sort();
        assert_eq!(
            keys,
            vec![b"{b}alpha".to_vec(), b"{b}beta".to_vec(), b"{b}h".to_vec()]
        );
        // count limit respected: physical order starts with the hash data
        // key (`{b}h`), its field elem is filtered out, so 2 rows yield 1.
        let out = call(&shared, &[b"getkeysinslot", b"3300", b"2"]);
        assert_eq!(decode_bulk_array(&out), vec![b"{b}h".to_vec()]);
        // errors
        assert!(
            call(&shared, &[b"getkeysinslot", b"99999", b"0"]).starts_with(b"-ERR Invalid slot")
        );
        assert!(call(&shared, &[b"getkeysinslot", b"3300"]).starts_with(b"-ERR wrong number"));
    }

    #[test]
    fn setslot_migrating_importing_stable_tables() {
        let (_guard, shared) = shared_for("127.0.0.1:32681");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        let src_id = utils::md5_with40("127.0.0.1:32681");
        let dst_id = utils::md5_with40("127.0.0.1:32683");
        assert_eq!(
            call(
                &shared,
                &[b"setslot", b"100", b"MIGRATING", src_id.as_bytes()]
            ),
            b"+OK\r\n"
        );
        assert_eq!(
            shared.migrating.read().unwrap().get(&100),
            Some(&"127.0.0.1:32681".to_string())
        );
        assert_eq!(
            call(
                &shared,
                &[b"setslot", b"100", b"IMPORTING", dst_id.as_bytes()]
            ),
            b"+OK\r\n"
        );
        assert_eq!(
            shared.importing.read().unwrap().get(&100),
            Some(&"127.0.0.1:32683".to_string())
        );
        // Unknown node id is rejected.
        assert_eq!(
            call(&shared, &[b"setslot", b"100", b"IMPORTING", b"deadbeef"]),
            b"-ERR Unknown node deadbeef\r\n"
        );
        // STABLE clears both tables.
        assert_eq!(call(&shared, &[b"setslot", b"100", b"STABLE"]), b"+OK\r\n");
        assert!(shared.migrating.read().unwrap().is_empty());
        assert!(shared.importing.read().unwrap().is_empty());
    }

    #[test]
    fn setslot_node_updates_owner_map_and_raft_key() {
        let (_guard, shared) = shared_for("127.0.0.1:32681");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        let dst_id = utils::md5_with40("127.0.0.1:32683");
        assert_eq!(
            call(&shared, &[b"setslot", b"100", b"NODE", dst_id.as_bytes()]),
            b"+OK\r\n"
        );
        assert_eq!(
            shared.topology.read().unwrap().owner_map.get(&100),
            Some(&"127.0.0.1:32683".to_string())
        );
        // The stub raft applies synchronously into RaftState.kv.
        let raft = shared.raft.read().unwrap();
        assert_eq!(
            state::raft_get(&raft, topology::OWNER_MAP_KEY),
            "{\"100\":\"127.0.0.1:32683\"}"
        );
    }

    #[test]
    fn nodes_and_slots_attribute_migrated_slot() {
        let (_guard, shared) = shared_for("127.0.0.1:32681");
        *shared.topology.write().unwrap() = topology::refresh(INSTANCES);
        let dst_id = utils::md5_with40("127.0.0.1:32683");
        // Migrate the LAST slot of node0's band to node1.
        call(&shared, &[b"setslot", b"5461", b"NODE", dst_id.as_bytes()]);
        let nodes = String::from_utf8(bulk_payload(&call(&shared, &[b"nodes"]))).unwrap();
        let lines: Vec<&str> = nodes.lines().collect();
        assert!(lines[0].ends_with(" connected 0-5460"));
        assert!(lines[1].ends_with(" connected 5461-10922"));
        assert!(lines[2].ends_with(" connected 10923-16383"));
        // CLUSTER SLOTS: node1's two ranges (5461 and 5462-10922) merge
        // into one contiguous entry -> 3 entries total.
        let slots = call(&shared, &[b"slots"]);
        assert!(slots.starts_with(b"*3\r\n"));
        assert!(slots.windows(13).any(|w| w == b":5461\r\n:10922"));
    }

    #[test]
    fn asking_sets_single_shot_flag() {
        let (_guard, shared) = shared_for("127.0.0.1:40211");
        let mut out = Vec::new();
        let mut conn = crate::tx::session::ConnState::default();
        let argv = vec![b"asking".to_vec()];
        let mut ctx =
            crate::command::test_ctx_with_conn(&shared, vec![], argv, &mut out, &mut conn);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(asking(&mut ctx));
        assert_eq!(out, b"+OK\r\n");
        assert!(conn.asking);
    }

    fn decode_bulk_array(out: &[u8]) -> Vec<Vec<u8>> {
        let mut rest = out;
        assert!(rest.starts_with(b"*"));
        rest = &rest[1..];
        let n: usize = {
            let i = rest.iter().position(|&b| b == b'\r').unwrap();
            let v = std::str::from_utf8(&rest[..i]).unwrap().parse().unwrap();
            rest = &rest[i + 2..];
            v
        };
        let mut keys = Vec::new();
        for _ in 0..n {
            assert!(rest.starts_with(b"$"));
            rest = &rest[1..];
            let i = rest.iter().position(|&b| b == b'\r').unwrap();
            let len: usize = std::str::from_utf8(&rest[..i]).unwrap().parse().unwrap();
            rest = &rest[i + 2..];
            keys.push(rest[..len].to_vec());
            rest = &rest[len + 2..];
        }
        keys
    }

    /// Seed a string through the real SET handler (argv WITHOUT the
    /// command name, like `test_ctx` callers).
    fn seed(shared: &Shared, key: &[u8], val: &[u8]) {
        let (_, prefix) = crate::hash::slot_with_prefix(crate::hash::hash_tag(key));
        let mut out = Vec::new();
        let argv = vec![key.to_vec(), val.to_vec()];
        let mut ctx = test_ctx(shared, prefix, argv, &mut out);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(crate::command::string::set(&mut ctx));
    }
}
