//! Shared fixtures for the Kafka-front process-level e2e tests:
//! spawn one REAL rdb binary with `kafka_bind` set, drive its RESP
//! port to lay down Lite streams, and hand-frame Kafka requests over
//! raw TCP. `kafka_wire_e2e.rs` (P0 handshakes),
//! `kafka_produce_e2e.rs` (P1 produce/listoffsets) and
//! `kafka_fetch_e2e.rs` (P2 fetch/offsets) mount this module.

// Helpers are shared across the per-binary e2e crates; each binary uses
// a different subset, so dead_code fires per-crate (same as common/mod.rs).
#![allow(dead_code)]

pub mod groups;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::common::TOKEN;

/// One rdb process: RESP + kafka listeners and its log paths.
pub struct KafkaNode {
    pub child: Child,
    pub dir: PathBuf,
    pub config_path: PathBuf,
    pub stderr_path: PathBuf,
    pub resp: String,
    pub kafka: String,
}

impl KafkaNode {
    /// SIGKILL + reap (kill() is SIGKILL on unix).
    pub fn kill_now(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Restart with the SAME config + data dir, no RAFT_BOOTSTRAP (the
    /// persisted raft state already knows this single-node cluster);
    /// stderr is appended to the same log.
    pub fn respawn(&mut self) {
        self.kill_now();
        let stderr = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.stderr_path)
            .expect("reopen stderr log");
        self.child = Command::new(env!("CARGO_BIN_EXE_rdb"))
            .arg("-config")
            .arg(&self.config_path)
            .env_remove("RAFT_JOIN_ADDR")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("respawn rdb binary");
    }
}

impl Drop for KafkaNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn free_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    l.local_addr().expect("local_addr").to_string()
}

pub fn spawn_kafka_node(dir: &Path) -> KafkaNode {
    spawn_kafka_node_bind(dir, "", "")
}

/// Variant with an explicit `kafka_bind` (e.g. a wildcard address for
/// advertised-listener tests) and extra yaml keys appended (each line,
/// e.g. "kafka_advertised_host: \"broker.example\"\n").
pub fn spawn_kafka_node_bind(dir: &Path, kafka_bind_override: &str, extra_conf: &str) -> KafkaNode {
    std::fs::create_dir_all(dir).expect("create node dir");
    let (resp, raft, http, monitor, kafka) = (
        free_addr(),
        free_addr(),
        free_addr(),
        free_addr(),
        free_addr(),
    );
    let kafka_bind = if kafka_bind_override.is_empty() {
        &kafka
    } else {
        kafka_bind_override
    };
    let config_path = dir.join("conf.yaml");
    let yaml = format!(
        "bind: \"{resp}\"\nstore_path: \"{}\"\nraft_bind_address: \"{raft}\"\n\
         raft_http_bind_address: \"{http}\"\nmonitor_addr: \"{monitor}\"\n\
         raft_token: \"{TOKEN}\"\nkafka_bind: \"{kafka_bind}\"\n{extra_conf}",
        dir.display()
    );
    std::fs::write(&config_path, yaml).expect("write conf.yaml");
    let stderr_path = dir.join("stderr.log");
    let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
    let child = Command::new(env!("CARGO_BIN_EXE_rdb"))
        .arg("-config")
        .arg(&config_path)
        .env("RAFT_BOOTSTRAP", "true")
        .env_remove("RAFT_JOIN_ADDR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn rdb binary");
    KafkaNode {
        child,
        dir: dir.to_path_buf(),
        config_path,
        stderr_path,
        resp,
        kafka,
    }
}

pub async fn wait_accepting(addr: &str, node: &mut KafkaNode, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(status)) = node.child.try_wait() {
            let tail = std::fs::read_to_string(&node.stderr_path).unwrap_or_default();
            panic!("rdb exited before {what} ready (status {status})\nstderr:\n{tail}");
        }
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        let tail = std::fs::read_to_string(&node.stderr_path).unwrap_or_default();
        assert!(
            Instant::now() < deadline,
            "{what} {addr} not accepting in 15s\nstderr:\n{tail}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// AUTH + one command on one fresh RESP connection; returns the whole
/// drained reply (auth ok + command reply).
pub async fn resp_one_shot(addr: &str, args: &[&[u8]]) -> Vec<u8> {
    let mut sock = TcpStream::connect(addr).await.expect("connect resp");
    let mut buf = Vec::new();
    buf.extend_from_slice(b"*2\r\n$4\r\nAUTH\r\n");
    buf.extend_from_slice(format!("${}\r\n{}\r\n", TOKEN.len(), TOKEN).as_bytes());
    buf.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    sock.write_all(&buf).await.expect("write resp cmds");
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_millis(300), sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => out.extend_from_slice(&chunk[..n]),
        }
    }
    out
}

/// Kafka request header (classic client_id, optional v2 tagged tail) ++
/// body; the caller frames it via [`kafka_round`].
pub fn kafka_req(
    api_key: i16,
    api_version: i16,
    corr: i32,
    flexible: bool,
    body: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&api_key.to_be_bytes());
    out.extend_from_slice(&api_version.to_be_bytes());
    out.extend_from_slice(&corr.to_be_bytes());
    out.extend_from_slice(&3i16.to_be_bytes());
    out.extend_from_slice(b"e2e");
    if flexible {
        out.push(0); // header tagged fields
    }
    out.extend_from_slice(body);
    out
}

/// Send one framed request, read one framed response payload.
pub async fn kafka_round(sock: &mut TcpStream, req: &[u8]) -> Vec<u8> {
    let mut framed = (req.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(req);
    sock.write_all(&framed).await.expect("write kafka req");
    let mut lenb = [0u8; 4];
    sock.read_exact(&mut lenb).await.expect("read kafka len");
    let len = i32::from_be_bytes(lenb) as usize;
    let mut payload = vec![0u8; len];
    sock.read_exact(&mut payload)
        .await
        .expect("read kafka body");
    payload
}

#[allow(dead_code)] // only the P0 wire test uses this
pub fn kafka_port(bind: &str) -> i32 {
    bind.rsplit_once(':').unwrap().1.parse().unwrap()
}
