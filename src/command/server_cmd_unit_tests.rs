//! Server-surface command tests (COMMAND/INFO/DBSIZE/ECHO/SELECT/
//! FLUSHDB); harness shared with the string-command tests.

use super::*;
use crate::command::flushdb::flushdb;
use crate::command::hash_cmd;
use crate::command::keys;
use crate::command::keyspace_role::FLUSH_PAGE;
use crate::command::string::test_util::{call, shared_for};
use crate::ds::codec;

/// `$len\r\npayload\r\n` framing helper for expected bulk replies.
fn bulk(payload: &str) -> Vec<u8> {
    format!("${}\r\n{}\r\n", payload.len(), payload).into_bytes()
}

/// Subslice search (slices have no `contains` for subslices).
fn has(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn command_count_and_subcommand_errors() {
    let (_guard, shared) = shared_for("127.0.0.1:40110");
    let out = call(&shared, |ctx| Box::pin(command(ctx)), &[b"count"]);
    assert_eq!(
        out,
        format!(":{}\r\n", cmd_meta::COMMANDS.len()).into_bytes()
    );
    // COUNT takes no further arguments; unknown subcommands (and the
    // wrong-number-of-arguments case) share Redis' error text.
    let out = call(&shared, |ctx| Box::pin(command(ctx)), &[b"count", b"x"]);
    assert_eq!(
        out,
        b"-ERR Unknown subcommand or wrong number of arguments for 'count'. Try COMMAND HELP.\r\n"
    );
    let out = call(&shared, |ctx| Box::pin(command(ctx)), &[b"bogus"]);
    assert_eq!(
        out,
        b"-ERR Unknown subcommand or wrong number of arguments for 'bogus'. Try COMMAND HELP.\r\n"
    );
}

#[test]
fn command_top_level_framing_matches_table() {
    let (_guard, shared) = shared_for("127.0.0.1:40111");
    let out = call(&shared, |ctx| Box::pin(command(ctx)), &[]);
    let first = &cmd_meta::COMMANDS[0];
    let expect = format!(
        "*{}\r\n*6\r\n${}\r\n{}\r\n:{}\r\n*0\r\n:{}\r\n:{}\r\n:{}\r\n",
        cmd_meta::COMMANDS.len(),
        first.name.len(),
        first.name,
        first.arity,
        first.first_key,
        first.last_key,
        first.step
    );
    assert!(
        out.starts_with(expect.as_bytes()),
        "prefix mismatch: {:?}",
        out
    );
    // Exactly one 6-element nested array per table row.
    let nested = out.windows(4).filter(|w| *w == b"*6\r\n").count();
    assert_eq!(nested, cmd_meta::COMMANDS.len());
    // COMMAND INFO with no names = the full table again.
    let info = call(&shared, |ctx| Box::pin(command(ctx)), &[b"info"]);
    assert_eq!(info, out);
}

#[test]
fn command_info_known_and_unknown_names() {
    let (_guard, shared) = shared_for("127.0.0.1:40111");
    let out = call(
        &shared,
        |ctx| Box::pin(command(ctx)),
        &[b"info", b"get", b"nope"],
    );
    assert_eq!(
        out,
        b"*2\r\n*6\r\n$3\r\nget\r\n:2\r\n*0\r\n:1\r\n:1\r\n:1\r\n$-1\r\n".to_vec()
    );
    // Name matching is case-insensitive both ways.
    let out = call(&shared, |ctx| Box::pin(command(ctx)), &[b"INFO", b"GET"]);
    assert_eq!(
        out,
        b"*1\r\n*6\r\n$3\r\nget\r\n:2\r\n*0\r\n:1\r\n:1\r\n:1\r\n".to_vec()
    );
}

#[test]
fn command_getkeys_resolves_positions() {
    let (_guard, shared) = shared_for("127.0.0.1:40111");
    let getkeys = |argv: &[&[u8]]| -> Vec<u8> {
        let mut full = vec![&b"getkeys"[..]];
        full.extend_from_slice(argv);
        call(&shared, |ctx| Box::pin(command(ctx)), &full)
    };
    assert_eq!(getkeys(&[b"set", b"k", b"v"]), b"*1\r\n$1\r\nk\r\n");
    assert_eq!(
        getkeys(&[b"mset", b"a", b"1", b"b", b"2"]),
        b"*2\r\n$1\r\na\r\n$1\r\nb\r\n"
    );
    assert_eq!(
        getkeys(&[b"watch", b"k1", b"k2"]),
        b"*2\r\n$2\r\nk1\r\n$2\r\nk2\r\n"
    );
    // Keyless command, unknown command, position outside argv.
    let invalid = b"-ERR Invalid command specified\r\n";
    assert_eq!(getkeys(&[b"ping"]), invalid);
    assert_eq!(getkeys(&[b"nope", b"k"]), invalid);
    assert_eq!(getkeys(&[b"get"]), invalid);
    assert_eq!(
        getkeys(&[]),
        b"-ERR wrong number of arguments for 'command|getkeys' command\r\n"
    );
}

#[test]
fn command_docs_flat_pairs() {
    let (_guard, shared) = shared_for("127.0.0.1:40111");
    let out = call(&shared, |ctx| Box::pin(command(ctx)), &[b"docs", b"get"]);
    assert_eq!(
        out,
        b"*2\r\n$3\r\nget\r\n*4\r\n$7\r\nsummary\r\n$3\r\nget\r\n$5\r\nsince\r\n$5\r\n1.0.0\r\n"
    );
    // Unknown requested names are skipped; no names = every command.
    assert_eq!(
        call(&shared, |ctx| Box::pin(command(ctx)), &[b"docs", b"nope"]),
        b"*0\r\n"
    );
    let all = call(&shared, |ctx| Box::pin(command(ctx)), &[b"docs"]);
    assert_eq!(
        all.windows(4).filter(|w| *w == b"*4\r\n").count(),
        cmd_meta::COMMANDS.len()
    );
}

#[test]
fn echo_and_select_replies() {
    let (_guard, shared) = shared_for("127.0.0.1:40111");
    assert_eq!(
        call(&shared, |ctx| Box::pin(echo(ctx)), &[b"hello"]),
        b"$5\r\nhello\r\n"
    );
    let arity = b"-ERR wrong number of arguments for 'echo' command\r\n";
    assert_eq!(call(&shared, |ctx| Box::pin(echo(ctx)), &[]), arity);
    assert_eq!(
        call(&shared, |ctx| Box::pin(echo(ctx)), &[b"a", b"b"]),
        arity
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(select(ctx)), &[b"0"]),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(select(ctx)), &[b"1"]),
        b"-ERR SELECT is not allowed in cluster mode\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(select(ctx)), &[b"abc"]),
        b"-ERR value is not an integer or out of range\r\n"
    );
}

#[test]
fn dbsize_counts_live_roots_only() {
    let (_guard, shared) = shared_for("127.0.0.1:40112");
    assert_eq!(call(&shared, |ctx| Box::pin(dbsize(ctx)), &[]), b":0\r\n");
    call(&shared, |ctx| Box::pin(string::set(ctx)), &[b"k", b"v"]);
    call(
        &shared,
        |ctx| Box::pin(hash_cmd::hset(ctx)),
        &[b"h", b"f", b"v"],
    );
    assert_eq!(call(&shared, |ctx| Box::pin(dbsize(ctx)), &[]), b":2\r\n");
    // SET + EXPIRE: the TTL envelope stays ONE root; the expire-index
    // record the TTL path adds must not inflate the count.
    call(&shared, |ctx| Box::pin(string::set(ctx)), &[b"t", b"1"]);
    assert_eq!(
        call(&shared, |ctx| Box::pin(keys::expire(ctx)), &[b"t", b"100"]),
        b":1\r\n"
    );
    assert_eq!(call(&shared, |ctx| Box::pin(dbsize(ctx)), &[]), b":3\r\n");
    assert_eq!(
        call(&shared, |ctx| Box::pin(info(ctx)), &[b"keyspace"]),
        bulk("# Keyspace\r\ndb0:keys=3,expires=1\r\n\r\n")
    );
    // An already-due envelope is lazily expired: never counted.
    let ghost = codec::data_key(b"70/", codec::KIND_STRING_TTL, b"ghost");
    ops::batch_write(&shared.store, {
        let mut b = rocksdb::WriteBatch::default();
        b.put(&ghost, codec::encode_envelope(1, b"v"));
        b
    })
    .expect("seed due envelope");
    assert_eq!(call(&shared, |ctx| Box::pin(dbsize(ctx)), &[]), b":3\r\n");
}

#[test]
fn info_sections_filter_and_concatenate() {
    let (_guard, shared) = shared_for("127.0.0.1:40113");
    let server = call(&shared, |ctx| Box::pin(info(ctx)), &[b"server"]);
    assert!(has(&server, b"# Server\r\n"));
    assert!(has(&server, b"redis_version:7.4.0\r\n"));
    assert!(has(&server, b"rdb_implementation:RocksDB\r\n"));
    assert!(has(&server, b"tcp_port:40113\r\n"));
    assert!(has(&server, b"run_id:"));
    assert!(!has(&server, b"# Cluster\r\n"));
    assert!(!has(&server, b"# Keyspace\r\n"));
    // Default (no args): every section, fixed order.
    let all = call(&shared, |ctx| Box::pin(info(ctx)), &[]);
    let cluster_at = all.windows(11).position(|w| w == b"# Cluster\r\n").unwrap();
    let keyspace_at = all
        .windows(12)
        .position(|w| w == b"# Keyspace\r\n")
        .unwrap();
    assert!(cluster_at < keyspace_at);
    assert!(has(&all, b"cluster_enabled:1\r\ncluster_mode:rdb\r\n"));
    assert!(all.ends_with(b"\r\n"));
    // Unknown section names contribute nothing at all.
    assert_eq!(
        call(&shared, |ctx| Box::pin(info(ctx)), &[b"bogus"]),
        b"$0\r\n\r\n"
    );
}

#[test]
fn flushdb_wipes_user_keys_preserves_control_plane() {
    let (_guard, shared) = shared_for("127.0.0.1:40114");
    // User data in two slots: raw string, typed hash (+ field), TTL
    // envelope, and a page-straddling mass of raw keys (> FLUSH_PAGE).
    call(&shared, |ctx| Box::pin(string::set(ctx)), &[b"k", b"v"]);
    call(
        &shared,
        |ctx| Box::pin(hash_cmd::hset(ctx)),
        &[b"h", b"f", b"1"],
    );
    call(&shared, |ctx| Box::pin(keys::expire(ctx)), &[b"k", b"100"]);
    crate::store::set(&shared.store, b"12/", b"other", b"v").expect("cross-slot put");
    let mut mass = rocksdb::WriteBatch::default();
    for i in 0..(FLUSH_PAGE + 64) {
        mass.put(codec::string_key(b"70/", format!("m{i}").as_bytes()), b"v");
    }
    ops::batch_write(&shared.store, mass).expect("seed mass");
    // Control-plane records: an SQL segment meta (kind 0x23, no slot
    // prefix) and a raft-style key outside the slot layout entirely.
    let segment = {
        let mut k = vec![0x23];
        k.extend_from_slice(&1u32.to_be_bytes());
        k.extend_from_slice(&7u64.to_be_bytes());
        k
    };
    let raft_meta = b"raft-state/term".to_vec();
    ops::batch_write(&shared.store, {
        let mut b = rocksdb::WriteBatch::default();
        b.put(&segment, b"meta");
        b.put(&raft_meta, b"1");
        b
    })
    .expect("seed control-plane");
    let expect = format!(":{}\r\n", FLUSH_PAGE + 64 + 3);
    assert_eq!(
        call(&shared, |ctx| Box::pin(dbsize(ctx)), &[]),
        expect.as_bytes()
    );
    assert_eq!(call(&shared, |ctx| Box::pin(flushdb(ctx)), &[]), b"+OK\r\n");
    assert_eq!(call(&shared, |ctx| Box::pin(dbsize(ctx)), &[]), b":0\r\n");
    // Family members and expire-index records went with their roots.
    let field = codec::elem_key(b"70/", codec::KIND_HASH_FLD, b"h", b"f");
    assert_eq!(ops::get_physical(&shared.store, &field).unwrap(), None);
    assert_eq!(ops::get_physical(&shared.store, b"12/other").unwrap(), None);
    // The control-plane records survived the wipe.
    assert_eq!(
        ops::get_physical(&shared.store, &segment)
            .unwrap()
            .as_deref(),
        Some(b"meta".as_slice())
    );
    assert_eq!(
        ops::get_physical(&shared.store, &raft_meta)
            .unwrap()
            .as_deref(),
        Some(b"1".as_slice())
    );
    // ASYNC/SYNC are accepted and ignored; junk is a syntax error.
    assert_eq!(
        call(&shared, |ctx| Box::pin(flushdb(ctx)), &[b"async"]),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(flushdb(ctx)), &[b"sync"]),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(flushdb(ctx)), &[b"nope"]),
        b"-ERR syntax error\r\n"
    );
}
