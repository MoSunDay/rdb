//! Process-level backup-listener test: spawn the REAL rdb binary with
//! `backup_bind` set (read-only replica mode) and assert the full gate
//! matrix -- every non-read command replies the Redis-standard
//! `-READONLY You can't write against a read only replica.`, reads stay
//! usable, the NORMAL listener on the same process is unaffected (the
//! gate is per-listener), unknown commands keep their own error (the
//! gate runs after lookup), and a MULTI/EXEC transaction aborts
//! wholesale (EXECABORT) when a write is queued, leaving the connection
//! usable. Also proves the backup listener binds and serves at all (the
//! backup store is a SEPARATE store: data written via the normal port
//! is NOT visible on the backup port).

mod common;

use std::time::{Duration, Instant};

use common::{cmd_one_shot, contains_bytes, spawn_node_backup, wait_resp_ready, TOKEN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The exact Redis replica error (prefix of the `-` error line).
const READONLY_ERR: &[u8] = b"READONLY You can't write against a read only replica.";

/// Per-reply socket timeout for the persistent MULTI/EXEC connection.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Encode one RESP array command frame (same wire format as common's
/// private helper, kept local for the persistent-conn helper below).
fn encode_cmd(args: &[&[u8]]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// Read ONE reply line from `sock`+`buf` (the buffer is shared across
/// successive replies on one connection). Returns the full line up to
/// (not including) CRLF; timeouts/EOF become markers so assertions
/// still print something diagnosable.
async fn read_line(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + REPLY_TIMEOUT;
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            let line: Vec<u8> = buf.drain(..pos).collect();
            buf.drain(..2); // consume the CRLF terminator too
            return line;
        }
        match tokio::time::timeout_at(deadline, sock.read(&mut chunk)).await {
            Ok(Ok(0)) => return b"<EOF>".to_vec(),
            Ok(Ok(k)) => buf.extend_from_slice(&chunk[..k]),
            Ok(Err(_)) => return b"<IO-ERR>".to_vec(),
            Err(_) => return b"<TIMEOUT>".to_vec(),
        }
    }
}

/// One AUTHed persistent connection: send one command, read its reply
/// line (all replies asserted here are single-line: +OK / -errors / nil).
struct Wire {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl Wire {
    async fn connect(addr: &str, token: &str) -> Wire {
        let mut w = Wire {
            sock: TcpStream::connect(addr).await.expect("connect backup"),
            buf: Vec::new(),
        };
        w.rpc(&[b"AUTH", token.as_bytes()]).await;
        w
    }

    async fn rpc(&mut self, args: &[&[u8]]) -> Vec<u8> {
        let frame = encode_cmd(args);
        self.sock.write_all(&frame).await.expect("write");
        read_line(&mut self.sock, &mut self.buf).await
    }
}

/// Poll the backup addr until it answers PING with PONG (fresh conn per
/// probe; conn-refused markers are just retried).
async fn wait_backup_ready(backup: &str, node: &common::ProcNode, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let r = cmd_one_shot(backup, TOKEN, &[b"PING"]).await;
        if contains_bytes(&r, b"PONG") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "backup listener never answered PING; last={r:?}\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_listener_rejects_writes_with_readonly_and_keeps_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut node, backup) = spawn_node_backup(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    wait_backup_ready(&backup, &node, 30).await;

    // Reads survive: PING works, GET on the (separate, empty) backup
    // store replies nil.
    let pong = cmd_one_shot(&backup, TOKEN, &[b"PING"]).await;
    assert!(contains_bytes(&pong, b"PONG"), "ping: {pong:?}");
    let nil = cmd_one_shot(&backup, TOKEN, &[b"GET", b"bk"]).await;
    assert_eq!(nil, b"$-1", "get on empty backup store: {nil:?}");

    // Every non-read command gets the Redis-standard replica error.
    let writes: [&[&[u8]]; 8] = [
        &[b"SET", b"bk", b"1"],
        &[b"DEL", b"bk"],
        &[b"INCR", b"bk"],
        &[b"FLUSHDB"],
        &[b"EXPIRE", b"bk", b"1"],
        &[b"RESTORE", b"bk", b"0", b"\x00"],
        // XIDLE is denied in ALL forms: the `<secs>` form persists stream
        // meta + TTL entries (lite::append), and the name-based gate
        // rejects the bare query form too.
        &[b"XIDLE", b"s", b"30"],
        &[b"XIDLE", b"s"],
    ];
    for args in writes {
        let r = cmd_one_shot(&backup, TOKEN, args).await;
        assert!(
            contains_bytes(&r, READONLY_ERR),
            "{:?} must reply -READONLY, got {r:?}",
            String::from_utf8_lossy(args[0])
        );
    }

    // Control: the gate is per-listener -- the NORMAL port writes fine
    // (its own store), and the value is invisible on the backup port.
    let ok = cmd_one_shot(&node.resp, TOKEN, &[b"SET", b"bk", b"1"]).await;
    assert_eq!(ok, b"+OK", "set via normal port: {ok:?}");
    let got = cmd_one_shot(&node.resp, TOKEN, &[b"GET", b"bk"]).await;
    assert_eq!(got, b"$1\r\n1", "get via normal port: {got:?}");
    let still_nil = cmd_one_shot(&backup, TOKEN, &[b"GET", b"bk"]).await;
    assert_eq!(still_nil, b"$-1", "separate store: {still_nil:?}");

    // The gate runs AFTER lookup: unknown commands keep their own error.
    let unknown = cmd_one_shot(&backup, TOKEN, &[b"NOTACOMMAND"]).await;
    assert!(
        contains_bytes(&unknown, b"ERR unknown command"),
        "unknown command error: {unknown:?}"
    );

    // MULTI/EXEC on ONE persistent connection: the write is rejected at
    // QUEUE time (dirty), EXEC aborts wholesale, the connection stays
    // usable and reads still work.
    let mut w = Wire::connect(&backup, TOKEN).await;
    assert_eq!(w.rpc(&[b"MULTI"]).await, b"+OK");
    let queued = w.rpc(&[b"SET", b"q", b"1"]).await;
    assert!(
        contains_bytes(&queued, READONLY_ERR),
        "queued set must reply -READONLY: {queued:?}"
    );
    let exec = w.rpc(&[b"EXEC"]).await;
    assert!(
        contains_bytes(&exec, b"EXECABORT"),
        "exec must abort wholesale: {exec:?}"
    );
    assert_eq!(w.rpc(&[b"GET", b"q"]).await, b"$-1");

    // The process survived everything above.
    assert!(
        node.child.try_wait().unwrap().is_none(),
        "rdb must still be alive\n{}",
        node.ctx()
    );
    node.kill_now();
}
