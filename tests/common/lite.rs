//! Shared harness for the Lite Mode e2e tests: a real store + the real
//! command registry, dispatched exactly like `resp::conn` (whitelisted
//! X-commands get no slot prefix).

use std::sync::{Arc, RwLock};

use rdb::{command, conf, hash, monitor, state, store, topology};

pub fn shared_at(tag: &str) -> (state::Shared, std::path::PathBuf) {
    let c = conf::Config {
        bind: format!("127.0.0.1:{tag}"),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: format!("127.0.0.1:{}", tag.parse::<u16>().unwrap() + 100),
        raft_token: "test-token".to_string(),
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("rdb-lite-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = store::data_path(dir.to_str().unwrap(), &c.bind);
    (open_shared(&c, &path), path)
}

/// Build Shared over an existing store path (also the "restart" entry).
pub fn open_shared(c: &conf::Config, path: &std::path::Path) -> state::Shared {
    let st = store::open(path.to_str().unwrap()).unwrap();
    state::Shared {
        mode: state::Mode::Normal,
        store: Arc::new(st),
        topology: Arc::new(RwLock::new(topology::empty())),
        raft: Arc::new(RwLock::new(state::stub_raft(c))),
        monitor: Arc::new(monitor::new_collector()),
        latch: rdb::ds::latch::Latch::new(),
        wait_hub: rdb::ds::wait::WaitHub::new(),
        lite: Arc::new(rdb::lite::new_runtime()),
        sql_ts: std::sync::Arc::new(rdb::sql::tx::Oracle::new()),
        migrating: std::sync::Arc::new(RwLock::new(std::collections::HashMap::new())),
        importing: std::sync::Arc::new(RwLock::new(std::collections::HashMap::new())),
        migrate_busy: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        conf: c.clone(),
    }
}

/// Registry dispatch mirroring `resp::conn` (whitelisted X-cmds: no slot prefix).
pub fn call(shared: &state::Shared, name: &str, args: &[&[u8]]) -> Vec<u8> {
    let handler = command::lookup(name).unwrap_or_else(|| panic!("'{name}' not registered"));
    let prefix_key = if rdb::router::is_whitelisted(name) {
        Vec::new()
    } else {
        hash::slot_with_prefix(hash::hash_tag(args.first().copied().unwrap_or_default())).1
    };
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    let mut ctx = command::Ctx {
        shared,
        prefix_key,
        args: argv,
        out: &mut out,
        close_conn: false,
        // Tests never drive MULTI state; a leaked default is fine (test-only).
        conn: Box::leak(Box::new(rdb::tx::session::ConnState::default())),
        wrote: false,
    };
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(handler(&mut ctx));
    out
}

pub fn text(reply: &[u8]) -> String {
    String::from_utf8_lossy(reply).into_owned()
}

/// `[id, consumer, idle, deliveries]` rows of an XPENDING range reply,
/// tokenized by `\r\n`: row[1]=id, row[3]=consumer, row[4]=":<idle-ms>",
/// row[5]=":<times-delivered>" (the idle value is wall-clock dependent,
/// so callers assert it loosely or skip it).
pub fn pel_rows(reply: &[u8]) -> Vec<Vec<String>> {
    text(reply)
        .split("*4\r\n")
        .skip(1)
        .map(|row| row.split("\r\n").map(str::to_string).collect())
        .collect()
}

/// One RESP array frame.
pub fn frame(args: &[&[u8]]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// Like `cmd_one_shot` but drains the whole reply (arrays-of-arrays are not
/// resolvable line-by-line): reads until the socket falls quiet for `quiet_ms`.
pub async fn cmd_full_reply(addr: &str, token: &str, args: &[&[u8]], quiet_ms: u64) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = match tokio::net::TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => return format!("<CONN-ERR {e}>").into_bytes(),
    };
    let mut buf = Vec::new();
    let auth = frame(&[b"AUTH", token.as_bytes()]);
    let cmd = frame(args);
    if sock.write_all(&auth).await.is_err() || sock.write_all(&cmd).await.is_err() {
        return b"<WRITE-ERR>".to_vec();
    }
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::time::timeout(
            std::time::Duration::from_millis(quiet_ms),
            sock.read(&mut chunk),
        )
        .await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) => break,
            Err(_) => break, // fell quiet: reply complete
        }
    }
    // Drop the leading AUTH reply line.
    if buf.starts_with(b"+OK\r\n") {
        buf[5..].to_vec()
    } else {
        buf
    }
}

// ---- proc-level boots (real binary; shared by the DLQ/sweep suites) ----

use std::path::{Path, PathBuf};

use super::{cmd_one_shot, spawn_node_yaml, wait_resp_ready, ProcNode, TOKEN};

/// Per-process root of the proc-level lite boots: `<tmp>/rdb-lite-boot-<pid>`
/// -- one per running test binary, so parallel cargo invocations never share
/// a tree while repeat runs (whose pid is often recycled) land on the exact
/// path an earlier crashed run left behind.
const BOOT_ROOT_PREFIX: &str = "rdb-lite-boot";
/// Marker stamped into a freshly wiped boot root: present = this process
/// already owns the tree, so later boots only claim their own tag below it.
const BOOT_ROOT_STAMP: &str = ".owned-by-this-process";

/// Opportunistic prune of sibling boot roots whose owning process is gone
/// (`/proc/<pid>` disappeared): hundreds of suite runs otherwise leave a
/// few hundred KB each in /tmp. Never touches a live process's tree (its
/// /proc entry exists) and silently no-ops where /proc is unavailable.
fn prune_dead_roots(mine: &Path) {
    let proc = std::path::Path::new("/proc");
    if !proc.join("self").exists() {
        return; // no /proc (non-Linux): leave siblings alone
    }
    let Some(parent) = mine.parent() else { return };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        let Some(pid) = name
            .strip_prefix(&format!("{BOOT_ROOT_PREFIX}-"))
            .and_then(|rest| rest.parse::<u32>().ok())
        else {
            continue; // not one of ours: never touch foreign temp dirs
        };
        if pid != std::process::id() && !proc.join(pid.to_string()).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Own the boot root exactly once per process, then hand out the caller's
/// tag dir, remove-then-created: (1) the first boot wipes the whole root
/// (stale node dirs a recycled pid inherited from a crashed run) under the
/// init mutex, so no sibling boot's dir can be created before the wipe;
/// (2) the tag dir itself is cleared, so a re-run always starts on an
/// empty store instead of colliding with leftover RocksDB data.
fn fresh_boot_dir(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("{BOOT_ROOT_PREFIX}-{}", std::process::id()));
    static INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _held = INIT.lock().unwrap_or_else(|e| e.into_inner());
    prune_dead_roots(&root);
    if !root.join(BOOT_ROOT_STAMP).exists() {
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create lite boot root");
        std::fs::write(root.join(BOOT_ROOT_STAMP), b"").expect("stamp lite boot root");
    }
    drop(_held);
    let dir = root.join(format!("{tag}-{}", nanos()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create lite boot dir");
    dir
}

/// Monotonic-enough uniqueness suffix (two boots may share a tag across
/// sequential tests of one binary; the wall clock keeps them apart).
fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Fresh single bootstrap node on a self-cleaning tempdir (see
/// [`fresh_boot_dir`]) -> (node, resp addr). The node must stay bound
/// (`_node`: Drop kills it).
pub async fn boot(tag: &str) -> (ProcNode, String) {
    boot_yaml(tag, "").await
}

/// [`boot`] plus verbatim top-level yaml lines in the node's conf.yaml
/// (e.g. `lite:\n  redelivery_idle_ms: 500\n` to arm the background
/// sweep in the spawned process).
pub async fn boot_yaml(tag: &str, extra_yaml: &str) -> (ProcNode, String) {
    let dir = fresh_boot_dir(tag);
    let mut node = spawn_node_yaml(&dir, 0, true, None, extra_yaml);
    wait_resp_ready(&mut node, 30).await;
    let resp = node.resp.clone();
    (node, resp)
}

/// One-shot command reply as text (single-line replies resolve line-wise).
pub async fn resp_text(a: &str, args: &[&[u8]]) -> String {
    text(&cmd_one_shot(a, TOKEN, args).await)
}

/// Full-drain command reply as text (arrays-of-arrays need the quiet-gap
/// reader, not the line reader).
pub async fn resp_full(a: &str, args: &[&[u8]]) -> String {
    text(&cmd_full_reply(a, TOKEN, args, 200).await)
}

/// (id, consumer, deliveries) of `XPENDING <s> <g> - + <n>` range rows,
/// sorted by id (the `>5` token filter drops the trailing CRLF artifact).
pub async fn pending_rows(a: &str, s: &[u8], g: &[u8], n: usize) -> Vec<(String, String, u64)> {
    let n_arg = n.to_string();
    let reply = cmd_full_reply(
        a,
        TOKEN,
        &[b"xpending", s, g, b"-", b"+", n_arg.as_bytes()],
        200,
    )
    .await;
    let mut rows: Vec<(String, String, u64)> = pel_rows(&reply)
        .into_iter()
        .filter(|r| r.len() > 5)
        .map(|r| {
            (
                r[1].clone(),
                r[3].clone(),
                r[5].trim_start_matches(':').parse::<u64>().unwrap_or(0),
            )
        })
        .collect();
    rows.sort();
    rows
}

// ---- in-process helpers shared by the claim-options / xinfo-full suites ----

use rdb::state::Shared;

/// XADD with an explicit id, asserting the echoed id reply (keeps every
/// later assertion byte-deterministic).
pub fn add(shared: &Shared, stream: &[u8], id: &str) {
    assert_eq!(
        call(shared, "xadd", &[stream, id.as_bytes(), b"f", b"v"]),
        format!("${}\r\n{id}\r\n", id.len()).into_bytes(),
        "xadd {id}"
    );
}

/// XGROUP CREATE ... MKSTREAM from 0-0, asserted OK.
pub fn mk_group(shared: &Shared, stream: &[u8], g: &[u8]) {
    assert_eq!(
        call(
            shared,
            "xgroup",
            &[b"create", stream, g, b"0-0", b"MKSTREAM"]
        ),
        b"+OK\r\n".to_vec(),
        "xgroup create {g:?}"
    );
}

/// One `>` delivery for `c`; asserts the reply mentions `last`.
pub fn deliver(shared: &Shared, stream: &[u8], g: &[u8], c: &[u8], last: &str) {
    let t = text(&call(
        shared,
        "xreadgroup",
        &[b"group", g, c, b"streams", stream, b">"],
    ));
    assert!(t.contains(last), "delivery missing {last}: {t}");
}

/// XPENDING range rows as `(id, consumer, idle-ms, deliveries)`, id
/// order (row[4]/row[5] are `:<int>` tokens; see [`pel_rows`]).
pub fn pending_rows4(
    shared: &Shared,
    stream: &[u8],
    g: &[u8],
    n: usize,
) -> Vec<(String, String, u64, u64)> {
    let n_arg = n.to_string();
    let mut rows: Vec<(String, String, u64, u64)> = pel_rows(&call(
        shared,
        "xpending",
        &[stream, g, b"-", b"+", n_arg.as_bytes()],
    ))
    .into_iter()
    .filter(|r| r.len() > 5)
    .map(|r| {
        (
            r[1].clone(),
            r[3].clone(),
            r[4].trim_start_matches(':').parse().unwrap_or(0),
            r[5].trim_start_matches(':').parse().unwrap_or(0),
        )
    })
    .collect();
    rows.sort();
    rows
}

/// XCLAIM reply text (trimmed of the trailing CRLF).
pub fn claim(shared: &Shared, stream: &[u8], args: &[&[u8]]) -> String {
    let mut argv = vec![stream];
    argv.extend(args.iter().copied());
    text(&call(shared, "xclaim", &argv)).trim_end().to_string()
}
