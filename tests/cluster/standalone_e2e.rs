//! `rdb standalone` e2e: one command, zero config, EVERY protocol front
//! up (mysql/sql-rpc/kafka/es/s3/rocksmq probed directly), a stable
//! token across restarts with data intact, and refusal to downgrade a
//! store that belongs to a multi-node cluster.

use crate::common;

use std::io::{Read as StdRead, Seek, SeekFrom};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::lite::text;
use common::{cmd_one_shot, contains_bytes};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// One `rdb standalone` child + everything the assertions need.
struct Standalone {
    child: Child,
    dir: PathBuf,
    stderr_path: PathBuf,
    host: String,
    port: u16,
}

impl Standalone {
    fn at(&self, off: u16) -> String {
        format!("{}:{}", self.host, self.port + off)
    }
    fn resp(&self) -> String {
        self.at(0)
    }
    fn token(&self) -> String {
        std::fs::read_to_string(self.dir.join("standalone_token"))
            .unwrap_or_default()
            .trim()
            .to_string()
    }
    fn stderr_tail(&self) -> String {
        let mut f = match std::fs::File::open(&self.stderr_path) {
            Ok(f) => f,
            Err(_) => return "<no stderr log>".to_string(),
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = f.seek(SeekFrom::Start(len.saturating_sub(4096)));
        let mut s = String::new();
        let _ = f.read_to_string(&mut s);
        s
    }
    fn alive(&mut self, what: &str) {
        match self.child.try_wait() {
            Ok(None) => {}
            Ok(Some(st)) => panic!(
                "standalone exited during {what} (status {st})\n{}",
                self.stderr_tail()
            ),
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
    fn kill_now(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Standalone {
    fn drop(&mut self) {
        self.kill_now();
    }
}

fn spawn_standalone(dir: &Path, host: &str, port: u16, turso_dir: Option<&Path>) -> Standalone {
    std::fs::create_dir_all(dir).expect("mkdir");
    let stderr_path = dir.join("stderr.log");
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
        .expect("stderr log");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rdb"));
    cmd.arg("standalone")
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port.to_string())
        .arg("--dir")
        .arg(dir);
    if let Some(t) = turso_dir {
        cmd.arg("--turso-db-dir").arg(t);
    }
    let child = cmd
        .env_remove("RAFT_BOOTSTRAP")
        .env_remove("RAFT_JOIN_ADDR")
        .env_remove("RDB_BEACON")
        .env_remove("RDB_DEBUG_REPL")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn rdb standalone");
    Standalone {
        child,
        dir: dir.to_path_buf(),
        stderr_path,
        host: host.to_string(),
        port,
    }
}

/// A base port with the whole +0..+10 derivation window free (held
/// simultaneously while probing, then released for the child).
fn free_port_block() -> u16 {
    for _ in 0..16 {
        let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let port = probe.local_addr().expect("addr").port();
        drop(probe);
        let held: Vec<_> = (0u16..=10)
            .filter_map(|o| {
                port.checked_add(o)
                    .and_then(|p| TcpListener::bind(("127.0.0.1", p)).ok())
            })
            .collect();
        if held.len() == 11 {
            return port;
        }
    }
    panic!("no free 11-port block on 127.0.0.1");
}

/// Poll until the token file exists and RESP answers PING after AUTH.
async fn wait_ready(s: &mut Standalone, secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        s.alive("startup");
        let token = s.token();
        if !token.is_empty() {
            let r = cmd_one_shot(&s.resp(), &token, &[b"PING"]).await;
            if contains_bytes(&r, b"PONG") {
                return token;
            }
        }
        assert!(
            Instant::now() < deadline,
            "standalone not RESP-ready within {secs}s\n{}",
            s.stderr_tail()
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Minimal HTTP/1.1 GET; returns the status line ("" on transport error).
async fn http_status(addr: &str, path: &str, bearer: Option<&str>) -> String {
    let Ok(mut sock) = TcpStream::connect(addr).await else {
        return "<CONN-ERR>".to_string();
    };
    let auth = bearer
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n");
    if sock.write_all(req.as_bytes()).await.is_err() {
        return "<WRITE-ERR>".to_string();
    }
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

/// First bytes the MySQL wire front sends (the greeting advertises
/// mysql_native_password).
async fn mysql_greeting(addr: &str) -> Vec<u8> {
    let Ok(mut sock) = TcpStream::connect(addr).await else {
        return b"<CONN-ERR>".to_vec();
    };
    let mut buf = vec![0u8; 512];
    match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf)).await {
        Ok(Ok(n)) => buf[..n].to_vec(),
        _ => Vec::new(),
    }
}

/// Kafka ApiVersions v0 bootstrap round; returns the raw reply.
async fn kafka_api_versions(addr: &str) -> Vec<u8> {
    let Ok(mut sock) = TcpStream::connect(addr).await else {
        return b"<CONN-ERR>".to_vec();
    };
    let corr = 0x5E2E_i32;
    let mut payload = Vec::new();
    payload.extend_from_slice(&18i16.to_be_bytes()); // ApiVersions
    payload.extend_from_slice(&0i16.to_be_bytes()); // v0
    payload.extend_from_slice(&corr.to_be_bytes());
    payload.extend_from_slice(&(-1i16).to_be_bytes()); // nullable client id
    let mut req = (payload.len() as i32).to_be_bytes().to_vec();
    req.extend_from_slice(&payload);
    if sock.write_all(&req).await.is_err() {
        return b"<WRITE-ERR>".to_vec();
    }
    let mut buf = vec![0u8; 4096];
    match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf)).await {
        Ok(Ok(n)) => buf[..n].to_vec(),
        _ => Vec::new(),
    }
}

#[tokio::test]
async fn standalone_boots_every_protocol_and_restarts_idempotently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("data");
    let port = free_port_block();
    let mut s = spawn_standalone(&data, "127.0.0.1", port, None);
    let token = wait_ready(&mut s, 90).await;
    let resp = s.resp();

    // RESP data plane.
    assert_eq!(
        text(&cmd_one_shot(&resp, &token, &[b"SET", b"k", b"v"]).await),
        "+OK",
        "{}",
        s.stderr_tail()
    );
    assert_eq!(
        text(&cmd_one_shot(&resp, &token, &[b"GET", b"k"]).await),
        "$1\r\nv"
    );

    // Auto topology: CLUSTER INFO/NODES answer with NO manual CLUSTER
    // INIT once ensure_topology's seed replicates (3s sync flips ready).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let info = text(&cmd_one_shot(&resp, &token, &[b"cluster", b"info"]).await);
        if info.contains("cluster_state:true") {
            break;
        }
        s.alive("topology seed");
        assert!(
            Instant::now() < deadline,
            "cluster never became ready\n{info}\n{}",
            s.stderr_tail()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let nodes = text(&cmd_one_shot(&resp, &token, &[b"cluster", b"nodes"]).await);
    assert!(contains_bytes(nodes.as_bytes(), b"myself,"), "{nodes}");
    assert!(contains_bytes(nodes.as_bytes(), b"0-16383"), "{nodes}");

    // Multi-protocol fronts, one credential.
    let greeting = mysql_greeting(&s.at(5)).await;
    assert!(
        contains_bytes(&greeting, b"mysql_native_password"),
        "mysql greeting: {greeting:?}\n{}",
        s.stderr_tail()
    );
    assert!(TcpStream::connect(s.at(6)).await.is_ok(), "sql_rpc accepts");
    let kv = kafka_api_versions(&s.at(7)).await;
    assert!(kv.len() > 8, "kafka ApiVersions reply: {kv:?}");
    assert_eq!(kv[4..8], 0x5E2E_i32.to_be_bytes(), "corr echo: {kv:?}");
    // HTTP fronts: no Bearer -> 401; the standalone token -> past the gate.
    for (plane, addr) in [("es", s.at(9)), ("s3", s.at(10)), ("rocksmq", s.at(8))] {
        let anon = http_status(&addr, "/", None).await;
        assert!(anon.contains(" 401 "), "{plane} anon gate: {anon}");
        let authed = http_status(&addr, "/", Some(&token)).await;
        assert!(!authed.contains(" 401 "), "{plane} token gate: {authed}");
    }
    s.alive("protocol probes");

    // Restart idempotence: same dir + port -> same token, data intact,
    // topology still ready, no re-init side effects.
    s.kill_now();
    let mut s2 = spawn_standalone(&data, "127.0.0.1", port, None);
    let token2 = wait_ready(&mut s2, 90).await;
    assert_eq!(token2, token, "token must survive restart");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let got = text(&cmd_one_shot(&resp, &token, &[b"GET", b"k"]).await);
        if got == "$1\r\nv" {
            break;
        }
        s2.alive("post-restart GET");
        assert!(
            Instant::now() < deadline,
            "data lost after restart\n{got}\n{}",
            s2.stderr_tail()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let info = text(&cmd_one_shot(&resp, &token, &[b"cluster", b"info"]).await);
    assert!(info.contains("cluster_state:true"), "{info}");
    s2.alive("end");
}

#[tokio::test]
async fn standalone_refuses_to_downgrade_a_multi_node_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A real 2-node cluster: node0's raft FSM ends up holding the
    // multi-address topology key.
    let (mut nodes, _) = common::start_cluster(dir.path(), 2).await;
    let node0_resp = nodes[0].resp.clone();
    let node0_dir = nodes[0].dir.clone();
    for n in nodes.iter_mut() {
        n.kill_now();
    }
    drop(nodes);
    let (host, port) = node0_resp.rsplit_once(':').expect("host:port");
    let port: u16 = port.parse().expect("port");

    // Same store dir + same bind => the very same raft log/store; the
    // standalone boot must refuse (exit 1), not silently overwrite.
    let mut s = spawn_standalone(&node0_dir, host, port, None);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(st) = s.child.try_wait().expect("try_wait") {
            assert_eq!(st.code(), Some(1), "refuse exit code\n{}", s.stderr_tail());
            let tail = s.stderr_tail();
            assert!(tail.contains("refusing to start"), "{tail}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "standalone did not refuse the multi-node store\n{}",
            s.stderr_tail()
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[tokio::test]
async fn standalone_boots_turso_engine_with_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("data");
    let turso_dir = dir.path().join("tursodb");
    let port = free_port_block();
    // The turso dir is deliberately NOT pre-created: the flag alone
    // must make the boot prepare it.
    let mut s = spawn_standalone(&data, "127.0.0.1", port, Some(&turso_dir));
    let token = wait_ready(&mut s, 30).await;

    // MySQL front, user root, password = the one standalone token.
    let mysql_port = s.port + 5;
    let opts = || {
        mysql_async::OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(mysql_port)
            .user(Some("root"))
            .pass(Some(token.as_str()))
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut c = loop {
        match mysql_async::Conn::new(opts()).await {
            Ok(c) => break c,
            Err(mysql_async::Error::Io(_)) if Instant::now() < deadline => {
                s.alive("mysql connect");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => panic!("mysql connect: {e}\n{}", s.stderr_tail()),
        }
    };
    // A `turso_*` database rides the embedded engine: create, route,
    // write, read back.
    common::mysql::ddl(&mut c, "CREATE DATABASE turso_flag").await;
    common::mysql::run(&mut c, "USE turso_flag").await;
    common::mysql::ddl(&mut c, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").await;
    common::mysql::run(&mut c, "INSERT INTO t VALUES (1, 'hello-turso')").await;
    assert_eq!(
        common::mysql::rows(&mut c, "SELECT v FROM t WHERE id = 1").await,
        vec![vec![common::mysql::s("hello-turso")]]
    );
    // The library file lands under the flag's directory.
    assert!(
        turso_dir.join("turso_flag.db").exists(),
        "turso library file missing\n{}",
        s.stderr_tail()
    );
    s.alive("end");
}
