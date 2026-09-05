//! Wire-level e2e tests for the server-surface commands (COMMAND / INFO /
//! DBSIZE / ECHO / SELECT / FLUSHDB / PING) over a real listener with raw
//! TcpStream RESP2 clients. Empty (all-local) topology: no MOVED allowed.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rdb::{command::cmd_meta, conf, monitor, resp, state, store, topology};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Documented `COMMAND` table size (see `src/command/cmd_meta.rs`).
const TABLE: usize = 188;
/// FLUSHDB chunk page (see `src/command/server_cmd.rs` `FLUSH_PAGE`).
const FLUSH_PAGE: usize = 1024;

fn test_config(port: u16) -> conf::Config {
    conf::Config {
        bind: format!("127.0.0.1:{port}"),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: format!("127.0.0.1:227{port}"),
        raft_token: "test-token".to_string(),
        ..Default::default()
    }
}

/// Mirror of `state::testutil::shared_with` (not visible to integration
/// tests); `tag` keeps the parallel tests' store dirs apart.
fn test_shared(conf: conf::Config, tag: &str) -> state::Shared {
    let dir = std::env::temp_dir().join(format!("rdb-surface-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = store::data_path(dir.to_str().unwrap(), &conf.bind);
    let st = store::open(path.to_str().unwrap()).unwrap();
    state::Shared {
        mode: state::Mode::Normal,
        store: Arc::new(st),
        topology: Arc::new(RwLock::new(topology::empty())),
        raft: Arc::new(RwLock::new(state::stub_raft(&conf))),
        monitor: Arc::new(monitor::new_collector()),
        latch: rdb::ds::latch::Latch::new(),
        wait_hub: rdb::ds::wait::WaitHub::new(),
        lite: Arc::new(rdb::lite::new_runtime()),
        sql_ts: Arc::new(rdb::sql::tx::Oracle::new()),
        migrating: Arc::new(RwLock::new(std::collections::HashMap::new())),
        importing: Arc::new(RwLock::new(std::collections::HashMap::new())),
        migrate_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        conf,
    }
}

fn resp_req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn arity(cmd: &str) -> Vec<u8> {
    format!("-ERR wrong number of arguments for '{cmd}' command\r\n").into_bytes()
}

fn unknown_sub(sub: &str) -> Vec<u8> {
    let t = "ERR Unknown subcommand or wrong number of arguments";
    format!("-{t} for '{sub}'. Try COMMAND HELP.\r\n").into_bytes()
}

#[derive(Debug)]
enum Frame {
    Bulk(Option<Vec<u8>>),
    Int(i64),
    Array(Vec<Frame>),
    /// `+`/`-` payloads are asserted as raw bytes elsewhere.
    Line,
}

impl Frame {
    fn bulk(&self) -> &[u8] {
        match self {
            Frame::Bulk(Some(b)) => b,
            other => panic!("expected bulk, got {other:?}"),
        }
    }

    fn arr(&self) -> &[Frame] {
        match self {
            Frame::Array(a) => a,
            other => panic!("expected array, got {other:?}"),
        }
    }

    fn parse(buf: &[u8]) -> Frame {
        let mut pos = 0;
        let f = Frame::walk(buf, &mut pos).expect("complete frame");
        assert_eq!(pos, buf.len(), "trailing bytes after frame");
        f
    }

    /// Recursive descent; `None` when the buffer holds only a prefix.
    fn walk(buf: &[u8], pos: &mut usize) -> Option<Frame> {
        let end = buf[*pos..].iter().position(|&b| b == b'\n')? + *pos;
        let line = &buf[*pos..end - 1];
        *pos = end + 1;
        let text = || std::str::from_utf8(&line[1..]).ok();
        match line.first()? {
            b'+' | b'-' => Some(Frame::Line),
            b':' => Some(Frame::Int(text()?.parse().ok()?)),
            b'$' => {
                let len: i64 = text()?.parse().ok()?;
                if len < 0 {
                    return Some(Frame::Bulk(None));
                }
                let len = len as usize;
                let end = pos.checked_add(len + 2).filter(|e| *e <= buf.len())?;
                let data = buf[*pos..*pos + len].to_vec();
                *pos = end;
                Some(Frame::Bulk(Some(data)))
            }
            b'*' => {
                let n: usize = text()?.parse().ok()?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(Frame::walk(buf, pos)?);
                }
                Some(Frame::Array(items))
            }
            other => panic!("unexpected frame line {other:?}"),
        }
    }
}

/// Buffered RESP client: send one request, get the next reply frame back
/// as RAW wire bytes (structure available via `Frame::parse`).
struct Wire {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl Wire {
    async fn connect(addr: std::net::SocketAddr, token: &str) -> Wire {
        let sock = tokio::time::timeout(TIMEOUT, TcpStream::connect(addr))
            .await
            .expect("connect timeout")
            .expect("connect");
        let mut w = Wire {
            sock,
            buf: Vec::new(),
        };
        let auth = resp_req(&[b"AUTH", token.as_bytes()]);
        assert_eq!(w.rpc(&auth).await, b"+OK\r\n");
        w
    }

    async fn rpc(&mut self, req: &[u8]) -> Vec<u8> {
        self.sock.write_all(req).await.expect("write");
        loop {
            let mut pos = 0;
            if Frame::walk(&self.buf, &mut pos).is_some() {
                return self.buf.drain(..pos).collect();
            }
            let mut chunk = [0u8; 4096];
            let n = tokio::time::timeout(TIMEOUT, self.sock.read(&mut chunk))
                .await
                .expect("read timeout")
                .expect("EOF while waiting for reply");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    async fn is(&mut self, parts: &[&[u8]], expect: &[u8]) {
        assert_eq!(self.rpc(&resp_req(parts)).await, expect, "{parts:?}");
    }

    async fn frame(&mut self, parts: &[&[u8]]) -> Frame {
        Frame::parse(&self.rpc(&resp_req(parts)).await)
    }
}

fn spawn(port: u16, tag: &str) -> std::net::SocketAddr {
    let shared = Arc::new(test_shared(test_config(port), tag));
    let listener = resp::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(resp::serve(listener, shared));
    addr
}

#[tokio::test]
async fn ping_echo_select_exact_wire_framing() {
    let mut w = Wire::connect(spawn(32770, "framing"), "test-token").await;

    w.is(&[b"PING"], b"+PONG\r\n").await;
    w.is(&[b"PING", b"hello"], b"$5\r\nhello\r\n").await;
    w.is(&[b"PING", b"a", b"b"], &arity("ping")).await;
    w.is(&[b"ECHO", b"x"], b"$1\r\nx\r\n").await;
    w.is(&[b"ECHO"], &arity("echo")).await;
    w.is(&[b"ECHO", b"a", b"b"], &arity("echo")).await;
    w.is(&[b"SELECT", b"0"], b"+OK\r\n").await;
    w.is(
        &[b"SELECT", b"1"],
        b"-ERR SELECT is not allowed in cluster mode\r\n",
    )
    .await;
    let not_int = b"-ERR value is not an integer or out of range\r\n";
    w.is(&[b"SELECT", b"abc"], not_int).await;
}

#[tokio::test]
async fn command_info_docs_getkeys_and_subcommand_errors() {
    let mut w = Wire::connect(spawn(32771, "command"), "test-token").await;

    // Sanity: the documented table size matches the compiled table.
    assert_eq!(cmd_meta::COMMANDS.len(), TABLE);

    // Bare COMMAND: one 6-element entry per table row ([0] bulk name,
    // [1] integer arity); the first row verbatim is keyless "asking".
    let bare = w.frame(&[b"COMMAND"]).await;
    let entries = bare.arr();
    assert_eq!(entries.len(), TABLE, "COMMAND must list every command");
    for e in entries {
        let fields = e.arr();
        assert_eq!(fields.len(), 6, "each entry is a 6-element array");
        assert!(matches!(fields[0], Frame::Bulk(Some(_))), "name is bulk");
        assert!(matches!(fields[1], Frame::Int(_)), "arity is integer");
    }
    assert_eq!(entries[0].arr()[0].bulk(), b"asking");
    assert!(matches!(entries[0].arr()[1], Frame::Int(1)));
    w.is(&[b"COMMAND", b"COUNT"], b":188\r\n").await;

    let entry = b"*6\r\n$3\r\nget\r\n:2\r\n*0\r\n:1\r\n:1\r\n:1\r\n";
    let one = [b"*1\r\n".as_slice(), entry].concat();
    w.is(&[b"COMMAND", b"INFO", b"get"], &one).await;
    w.is(&[b"COMMAND", b"INFO", b"nosuchcmd"], b"*1\r\n$-1\r\n")
        .await;
    let two = [b"*2\r\n".as_slice(), entry, b"$-1\r\n".as_slice()].concat();
    w.is(&[b"COMMAND", b"INFO", b"get", b"nosuchcmd"], &two)
        .await;

    // DOCS: flat [name, map, ...] pairs, one pair per table row.
    let docs_bare = w.frame(&[b"COMMAND", b"DOCS"]).await;
    let pairs = docs_bare.arr();
    assert_eq!(pairs.len(), 2 * TABLE, "flat name/map pairs, even length");
    for (i, p) in pairs.iter().enumerate() {
        if i % 2 == 0 {
            assert!(matches!(p, Frame::Bulk(Some(_))), "even slot is bulk");
        } else {
            assert_eq!(p.arr().len(), 4, "map carries summary + since");
        }
    }
    let docs =
        b"*2\r\n$3\r\nget\r\n*4\r\n$7\r\nsummary\r\n$3\r\nget\r\n$5\r\nsince\r\n$5\r\n1.0.0\r\n";
    w.is(&[b"COMMAND", b"DOCS", b"get"], docs).await;

    w.is(
        &[b"COMMAND", b"GETKEYS", b"SET", b"k", b"v"],
        b"*1\r\n$1\r\nk\r\n",
    )
    .await;
    let two_keys = b"*2\r\n$1\r\na\r\n$1\r\nb\r\n";
    let mset: Vec<&[u8]> = vec![b"COMMAND", b"GETKEYS", b"MSET", b"a", b"1", b"b", b"2"];
    w.is(&mset, two_keys).await;
    w.is(
        &[b"COMMAND", b"GETKEYS", b"PING"],
        b"-ERR Invalid command specified\r\n",
    )
    .await;
    w.is(&[b"COMMAND", b"GETKEYS"], &arity("command|getkeys"))
        .await;

    // Unknown subcommand keeps the caller's original case (Redis quirk);
    // exact-arity COUNT degrades to the same error on extra args.
    w.is(&[b"COMMAND", b"FROBNICATE"], &unknown_sub("FROBNICATE"))
        .await;
    w.is(&[b"COMMAND", b"count", b"x"], &unknown_sub("count"))
        .await;
}

#[tokio::test]
async fn info_sections_filtering_expiry_counts_and_lazy_expiry() {
    let mut w = Wire::connect(spawn(32772, "info"), "test-token").await;

    w.is(&[b"SET", b"a", b"1"], b"+OK\r\n").await;
    w.is(&[b"HSET", b"h", b"f", b"v"], b":1\r\n").await;
    w.is(&[b"LPUSH", b"l", b"x"], b":1\r\n").await;

    let payload = w.frame(&[b"INFO"]).await.bulk().to_vec();
    assert!(contains(&payload, b"# Server\r\n"));
    assert!(contains(&payload, b"redis_version:"));
    assert!(contains(
        &payload,
        b"# Keyspace\r\ndb0:keys=3,expires=0\r\n"
    ));

    let server = w.frame(&[b"INFO", b"server"]).await.bulk().to_vec();
    assert!(contains(&server, b"# Server\r\n"));
    assert!(contains(&server, b"redis_version:7.4.0\r\n"));
    assert!(!contains(&server, b"# Keyspace"));
    assert!(!contains(&server, b"# Cluster"));

    // Section filtering is case-insensitive; unknown sections contribute
    // nothing at all (an empty payload, per server_cmd.rs docs).
    let again = w.frame(&[b"INFO", b"SeRvEr"]).await;
    assert_eq!(again.bulk(), server.as_slice());
    let ks = b"$36\r\n# Keyspace\r\ndb0:keys=3,expires=0\r\n\r\n\r\n";
    w.is(&[b"INFO", b"keyspace"], ks).await;
    w.is(&[b"INFO", b"NoTaSeCtIoN"], b"$0\r\n\r\n").await;

    // PSETEX 500ms: live now (expires=1), lazily expired once due.
    w.is(&[b"PSETEX", b"g", b"500", b"v"], b"+OK\r\n").await;
    let live = w.frame(&[b"INFO", b"keyspace"]).await.bulk().to_vec();
    assert!(contains(&live, b"db0:keys=4,expires=1\r\n"));
    tokio::time::sleep(Duration::from_millis(600)).await;
    w.is(&[b"INFO", b"keyspace"], ks).await;

    // SETEX keeps ONE root with a live deadline: keys+1, expires=1.
    w.is(&[b"SETEX", b"t", b"100", b"v"], b"+OK\r\n").await;
    let ttl = w.frame(&[b"INFO", b"keyspace"]).await.bulk().to_vec();
    assert!(contains(&ttl, b"db0:keys=4,expires=1\r\n"));
}

#[tokio::test]
async fn dbsize_counts_one_root_per_family() {
    let mut w = Wire::connect(spawn(32773, "dbsize"), "test-token").await;

    w.is(&[b"DBSIZE"], b":0\r\n").await;
    w.is(&[b"SET", b"s", b"v"], b"+OK\r\n").await;
    w.is(&[b"HSET", b"h", b"f", b"v"], b":1\r\n").await;
    w.is(&[b"LPUSH", b"l", b"x"], b":1\r\n").await;
    w.is(&[b"ZADD", b"z", b"1", b"m"], b":1\r\n").await;
    w.is(&[b"SADD", b"st", b"m"], b":1\r\n").await;
    // Lite stream: the first XADD creates exactly ONE stream-meta root
    // (members and the expire index never count as roots).
    w.frame(&[b"XADD", b"qp", b"f", b"v"]).await;
    w.is(&[b"DBSIZE"], b":6\r\n").await;
    // TTL envelope: still a single root (the expire index is a member).
    w.is(&[b"SETEX", b"t", b"100", b"v"], b"+OK\r\n").await;
    w.is(&[b"DBSIZE"], b":7\r\n").await;
    w.is(&[b"DEL", b"h"], b":1\r\n").await;
    w.is(&[b"DEL", b"t"], b":1\r\n").await;
    w.is(&[b"DBSIZE"], b":5\r\n").await;
    w.is(&[b"DBSIZE", b"x"], &arity("dbsize")).await;
}

#[tokio::test]
async fn flushdb_crosses_chunk_pages_and_keeps_the_connection_usable() {
    let mut w = Wire::connect(spawn(32774, "flushdb"), "test-token").await;

    w.is(&[b"SET", b"sentinel", b"v"], b"+OK\r\n").await;
    // >2 FLUSH_PAGEs of single-slot keys: the chunked delete must resume
    // its scan after each committed page.
    let total = 2 * FLUSH_PAGE + 52;
    let per_mset = 350;
    for c in 0..total.div_ceil(per_mset) {
        let keys: Vec<String> = (c * per_mset..total.min((c + 1) * per_mset))
            .map(|i| format!("{{f}}k{i:04}"))
            .collect();
        let mut parts: Vec<&[u8]> = vec![b"MSET"];
        for k in &keys {
            parts.push(k.as_bytes());
            parts.push(b"v");
        }
        w.is(&parts, b"+OK\r\n").await;
    }
    let full = format!(":{}\r\n", total + 1).into_bytes();
    w.is(&[b"DBSIZE"], &full).await;

    w.is(&[b"FLUSHDB"], b"+OK\r\n").await;
    w.is(&[b"DBSIZE"], b":0\r\n").await;
    w.is(&[b"GET", b"sentinel"], b"$-1\r\n").await;

    // Modifiers are accepted (and ignored); junk is a syntax error.
    w.is(&[b"FLUSHDB", b"ASYNC"], b"+OK\r\n").await;
    w.is(&[b"FLUSHDB", b"SYNC"], b"+OK\r\n").await;
    w.is(&[b"FLUSHDB", b"JUNK"], b"-ERR syntax error\r\n").await;
    w.is(&[b"FLUSHDB", b"a", b"b"], &arity("flushdb")).await;

    // No framing desync: the same connection still answers.
    w.is(&[b"PING"], b"+PONG\r\n").await;
    w.is(&[b"DBSIZE"], b":0\r\n").await;
}

#[tokio::test]
async fn multi_queues_keyless_commands_and_exec_replays_in_order() {
    let mut w = Wire::connect(spawn(32775, "multi"), "test-token").await;

    w.is(&[b"MULTI"], b"+OK\r\n").await;
    // Keyless commands (keyspec Shape::None) must queue like any other.
    w.is(&[b"DBSIZE"], b"+QUEUED\r\n").await;
    w.is(&[b"INCR", b"ctr"], b"+QUEUED\r\n").await;
    w.is(&[b"ECHO", b"hi"], b"+QUEUED\r\n").await;
    w.is(&[b"EXEC"], b"*3\r\n:0\r\n:1\r\n$2\r\nhi\r\n").await;
    w.is(&[b"DBSIZE"], b":1\r\n").await;
}
