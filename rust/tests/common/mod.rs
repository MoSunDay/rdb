//! Shared helpers for the process-level e2e tests: spawn the REAL `rdb`
//! binary (cargo's `CARGO_BIN_EXE_rdb`) against REAL yaml configs written
//! into a per-test tempdir, talk RESP2 over raw TcpStreams, and drive the
//! cluster lifecycle (bootstrap -> join -> leader -> init). This is a
//! module (`mod common;`), not a test binary of its own.

#![allow(dead_code)]

use std::io::{Read, Seek, SeekFrom};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Fake raft token for the e2e configs (NEVER the real one from
/// /root/rdb/config -- no secrets in the repo).
pub mod lite;

pub const TOKEN: &str = "e2e-fake-token-0123456789abcdef0123456789abcdef";

/// Per-reply socket timeout; loopback plus debug-profile handlers stay far
/// below this, so a timeout means something is genuinely stuck.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// How much of the node's stderr log failure context prints.
const STDERR_TAIL_BYTES: u64 = 2048;

/// Substring test on raw bytes (`haystack.contains(needle)` for &[u8]).
pub fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

/// Reserve one free loopback port: bind :0, read the port, drop the
/// listener. Reuse races are an accepted risk; assertions print stderr.
fn free_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    listener.local_addr().expect("local_addr").to_string()
}

/// One spawned rdb process plus everything needed to talk to or restart it.
pub struct ProcNode {
    /// Per-node data dir (config, rocksdb store and raft log live below it).
    pub dir: PathBuf,
    pub config_path: PathBuf,
    pub stderr_path: PathBuf,
    pub child: Child,
    /// resp / raft-rpc / raft-http / monitor bind addresses.
    pub resp: String,
    pub raft: String,
    pub http: String,
    pub monitor: String,
    /// MySQL-protocol bind address ("" when the SQL plane is disabled).
    pub mysql: String,
}

impl ProcNode {
    /// Last chunk of the child's stderr log, for assertion messages.
    pub fn stderr_tail(&self) -> String {
        let mut f = match std::fs::File::open(&self.stderr_path) {
            Ok(f) => f,
            Err(_) => return "<no stderr log>".to_string(),
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = f.seek(SeekFrom::Start(len.saturating_sub(STDERR_TAIL_BYTES)));
        let mut s = String::new();
        let _ = f.read_to_string(&mut s);
        s
    }

    /// Failure context: who this node is, plus its stderr tail.
    pub fn ctx(&self) -> String {
        format!(
            "[resp={} raft={} http={} monitor={} dir={}] stderr tail:\n{}",
            self.resp,
            self.raft,
            self.http,
            self.monitor,
            self.dir.display(),
            self.stderr_tail()
        )
    }

    /// SIGKILL + reap (kill() is SIGKILL on unix).
    pub fn kill_now(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Restart with the SAME config path + data dir and NO RAFT_BOOTSTRAP /
    /// RAFT_JOIN_ADDR (persistent raft state + unchanged addr: the leader
    /// replicates to it). Stderr is appended to the same log.
    pub fn respawn(&mut self) {
        self.kill_now();
        self.child = spawn_child(&self.config_path, false, None, &self.stderr_path, true);
    }
}

impl Drop for ProcNode {
    fn drop(&mut self) {
        self.kill_now();
    }
}

/// Failure context for a whole cluster.
pub fn all_ctx(nodes: &[ProcNode]) -> String {
    nodes
        .iter()
        .map(|n| n.ctx())
        .collect::<Vec<_>>()
        .join("\n----\n")
}

/// Write the node's conf.yaml (only the keys the binary needs).
fn write_config(
    path: &Path,
    node_dir: &Path,
    resp: &str,
    raft: &str,
    http: &str,
    monitor: &str,
    mysql: &str,
) {
    let sql_keys = if mysql.is_empty() {
        String::new()
    } else {
        format!("mysql_bind: \"{mysql}\"\nmysql_user: \"root\"\nmysql_password: \"e2e-sql-pass\"\n")
    };
    let yaml = format!(
        "bind: \"{resp}\"\nstore_path: \"{}\"\nraft_bind_address: \"{raft}\"\n\
         raft_http_bind_address: \"{http}\"\nmonitor_addr: \"{monitor}\"\nraft_token: \"{TOKEN}\"\n{sql_keys}",
        node_dir.display()
    );
    std::fs::write(path, yaml).expect("write conf.yaml");
}

fn spawn_child(
    config_path: &Path,
    bootstrap: bool,
    join_http: Option<&str>,
    stderr_path: &Path,
    append: bool,
) -> Child {
    let stderr = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(stderr_path)
            .expect("open stderr log")
    } else {
        std::fs::File::create(stderr_path).expect("create stderr log")
    };
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rdb"));
    cmd.arg("-config").arg(config_path);
    // Start from a clean env regardless of what the test runner carries.
    cmd.env_remove("RAFT_BOOTSTRAP")
        .env_remove("RAFT_JOIN_ADDR")
        .env_remove("RDB_BEACON")
        .env_remove("RDB_DEBUG_REPL")
        .env_remove("RUST_LOG");
    if bootstrap {
        cmd.env("RAFT_BOOTSTRAP", "true"); // exactly "true" enables it
    }
    if let Some(join) = join_http {
        cmd.env("RAFT_JOIN_ADDR", join);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    cmd.spawn().expect("spawn rdb binary")
}

/// Spawn node `id` below the per-test `dir`: node dir + conf.yaml + 4 fresh
/// ports, child launched per `bootstrap`/`join_http`.
pub fn spawn_node(dir: &Path, id: usize, bootstrap: bool, join_http: Option<&str>) -> ProcNode {
    let node_dir = dir.join(format!("node{id}"));
    std::fs::create_dir_all(&node_dir).expect("create node dir");
    let (resp, raft, http, monitor) = (free_addr(), free_addr(), free_addr(), free_addr());
    let config_path = node_dir.join("conf.yaml");
    write_config(&config_path, &node_dir, &resp, &raft, &http, &monitor, "");
    let stderr_path = node_dir.join("stderr.log");
    let child = spawn_child(&config_path, bootstrap, join_http, &stderr_path, false);
    ProcNode {
        dir: node_dir,
        config_path,
        stderr_path,
        child,
        resp,
        raft,
        http,
        monitor,
        mysql: String::new(),
    }
}

/// Spawn a node with the SQL (MySQL-protocol) plane enabled on a fresh
/// port; native-password login is root/e2e-sql-pass.
pub fn spawn_node_mysql(
    dir: &Path,
    id: usize,
    bootstrap: bool,
    join_http: Option<&str>,
) -> ProcNode {
    let node_dir = dir.join(format!("node{id}"));
    std::fs::create_dir_all(&node_dir).expect("create node dir");
    let (resp, raft, http, monitor, mysql) = (
        free_addr(),
        free_addr(),
        free_addr(),
        free_addr(),
        free_addr(),
    );
    let config_path = node_dir.join("conf.yaml");
    write_config(
        &config_path,
        &node_dir,
        &resp,
        &raft,
        &http,
        &monitor,
        &mysql,
    );
    let stderr_path = node_dir.join("stderr.log");
    let child = spawn_child(&config_path, bootstrap, join_http, &stderr_path, false);
    ProcNode {
        dir: node_dir,
        config_path,
        stderr_path,
        child,
        resp,
        raft,
        http,
        monitor,
        mysql,
    }
}

/// Poll the node's MySQL port until it accepts connections.
pub async fn wait_mysql_ready(node: &ProcNode, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if TcpStream::connect(&node.mysql).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "mysql port never came up; {}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll the node's RESP port (100ms interval) until it accepts connections;
/// a child that exits first fails fast with its stderr tail. RESP is the
/// LAST listener the binary binds, so readiness implies monitor + raft +
/// http (+ join) are all up.
pub async fn wait_resp_ready(node: &mut ProcNode, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(Some(status)) = node.child.try_wait() {
            let msg = format!(
                "rdb exited before RESP ready (status {status})\n{}",
                node.ctx()
            );
            panic!("{msg}");
        }
        if TcpStream::connect(&node.resp).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "resp {} not accepting within {secs}s\n{}",
            node.resp,
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Encode one RESP array command frame.
fn encode_cmd(args: &[&[u8]]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// Read ONE reply from `sock`+`buf`, drill.py `read_one` semantics: the
/// buffer is shared across successive replies on one connection (the
/// server pipelines AUTH + command replies into one TCP segment). A
/// `simple/error/int` line is returned as-is; a bulk `$N` (N >= 0) as
/// `$N\r\n<payload>` (no trailing CRLF); `$-1` stays `$-1`. Timeouts/EOF
/// become markers so exact equality assertions still print something
/// diagnosable.
async fn read_one(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + REPLY_TIMEOUT;
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            let line = buf[..pos].to_vec();
            let mut rest = buf.split_off(pos + 2);
            if line.starts_with(b"$") && line != b"$-1" {
                let n: usize = match std::str::from_utf8(&line[1..])
                    .ok()
                    .and_then(|s| s.parse().ok())
                {
                    Some(n) => n,
                    None => return line,
                };
                while rest.len() < n + 2 {
                    match tokio::time::timeout_at(deadline, sock.read(&mut chunk)).await {
                        // EOF, IO error or timeout: return what arrived.
                        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {
                            let mut reply = line.clone();
                            reply.extend_from_slice(b"\r\n");
                            reply.extend_from_slice(&rest);
                            reply.extend_from_slice(b"<INCOMPLETE>");
                            return reply;
                        }
                        Ok(Ok(k)) => rest.extend_from_slice(&chunk[..k]),
                    }
                }
                let mut reply = line;
                reply.extend_from_slice(b"\r\n");
                reply.extend_from_slice(&rest[..n]);
                *buf = rest.split_off(n + 2);
                return reply;
            }
            *buf = rest;
            return line;
        }
        match tokio::time::timeout_at(deadline, sock.read(&mut chunk)).await {
            Ok(Ok(0)) => {
                let mut reply = buf.clone();
                reply.extend_from_slice(b"<EOF>");
                return reply;
            }
            Ok(Ok(k)) => buf.extend_from_slice(&chunk[..k]),
            Ok(Err(_)) => return b"<IO-ERR>".to_vec(),
            Err(_) => return b"<TIMEOUT>".to_vec(),
        }
    }
}

/// Connect, AUTH with `token`, run one command, return the raw command
/// reply (the AUTH reply is consumed and discarded). Connect failures are
/// returned as markers so poll loops can just retry.
pub async fn cmd_one_shot(addr: &str, token: &str, args: &[&[u8]]) -> Vec<u8> {
    let mut sock = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => return format!("<CONN-ERR {e}>").into_bytes(),
    };
    let auth = encode_cmd(&[b"AUTH", token.as_bytes()]);
    let cmd = encode_cmd(args);
    if sock.write_all(&auth).await.is_err() || sock.write_all(&cmd).await.is_err() {
        return b"<WRITE-ERR>".to_vec();
    }
    let mut buf: Vec<u8> = Vec::new();
    let _auth_reply = read_one(&mut sock, &mut buf).await;
    read_one(&mut sock, &mut buf).await
}

/// Connect and exchange raw bytes with no AUTH (pre-auth probes); returns
/// the first reply line/payload.
pub async fn raw_exchange(addr: &str, bytes: &[u8]) -> Vec<u8> {
    let mut sock = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => return format!("<CONN-ERR {e}>").into_bytes(),
    };
    if sock.write_all(bytes).await.is_err() {
        return b"<WRITE-ERR>".to_vec();
    }
    let mut buf: Vec<u8> = Vec::new();
    read_one(&mut sock, &mut buf).await
}

/// The reply to a bare AUTH (used for the explicit +OK assertion).
pub async fn auth_reply(addr: &str, token: &str) -> Vec<u8> {
    raw_exchange(addr, &encode_cmd(&[b"AUTH", token.as_bytes()])).await
}

/// Poll `raft nodes` on every node until one reports "[Leader]" (the label
/// refreshes from raft metrics within ~500ms); returns its index.
pub async fn wait_leader(nodes: &[ProcNode], secs: u64) -> usize {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last: Vec<Vec<u8>> = Vec::new();
    loop {
        for (i, n) in nodes.iter().enumerate() {
            let r = cmd_one_shot(&n.resp, TOKEN, &[b"raft", b"nodes"]).await;
            if contains_bytes(&r, b"[Leader]") {
                return i;
            }
            last.push(r);
        }
        assert!(
            Instant::now() < deadline,
            "no [Leader] in `raft nodes` within {secs}s; last replies {last:?}\n{}",
            all_ctx(nodes)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// `CLUSTER INIT <resp-binds>` on the leader; must reply exactly `+done`.
pub async fn cluster_init(leader: &ProcNode, resp_binds: &[String]) {
    let instances = resp_binds.join(",");
    let r = cmd_one_shot(
        &leader.resp,
        TOKEN,
        &[b"cluster", b"init", instances.as_bytes()],
    )
    .await;
    assert_eq!(
        r,
        b"+done",
        "cluster init on leader (resp {}) failed\n{}",
        leader.resp,
        leader.ctx()
    );
}

/// Poll `cluster nodes` on EVERY node until every reply lists every bind
/// (topology sync is a 3s ticker, so allow for a few rounds).
pub async fn wait_cluster_nodes_list_all(nodes: &[ProcNode], binds: &[String], secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let mut all = true;
        for n in nodes {
            let r = cmd_one_shot(&n.resp, TOKEN, &[b"cluster", b"nodes"]).await;
            if !binds.iter().all(|b| contains_bytes(&r, b.as_bytes())) {
                all = false;
                break;
            }
        }
        if all {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "cluster nodes did not list every bind within {secs}s\n{}",
            all_ctx(nodes)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Full cluster bring-up: node0 bootstraps and must be leading BEFORE any
/// joiner spawns (the HTTP /join is a single attempt, no retry), joiners
/// connect through node0's raft-http addr, then leader + `CLUSTER INIT` +
/// topology convergence. Returns the nodes and the leader index.
pub async fn start_cluster(dir: &Path, n: usize) -> (Vec<ProcNode>, usize) {
    let mut nodes = Vec::new();
    let mut first = spawn_node(dir, 0, true, None);
    wait_resp_ready(&mut first, 30).await;
    nodes.push(first);
    let l0 = wait_leader(&nodes, 60).await;
    assert_eq!(l0, 0, "node0 must lead before joins\n{}", all_ctx(&nodes));

    let join = nodes[0].http.clone();
    for id in 1..n {
        let mut node = spawn_node(dir, id, false, Some(&join));
        wait_resp_ready(&mut node, 30).await;
        nodes.push(node);
    }
    let leader = wait_leader(&nodes, 60).await;
    let binds: Vec<String> = nodes.iter().map(|x| x.resp.clone()).collect();
    cluster_init(&nodes[leader], &binds).await;
    wait_cluster_nodes_list_all(&nodes, &binds, 30).await;
    (nodes, leader)
}
