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
