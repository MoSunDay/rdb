//! Multi-store scenario #52 -- a black-Friday doorbuster fed by
//! pipelined RESP batches: a seed batch of many commands sent in ONE
//! TCP write returns replies in strict submission order, a mid-batch
//! unknown command does not poison the rest of the batch (the error
//! rides inline and the trailing writes still land), and a whole
//! MULTI..EXEC transaction queues and replays inside a single
//! pipelined write (+QUEUED acks then the nested `*3` typed replay).
//! One REAL node, RESP only (persistent AUTH session).

use crate::common;
use crate::scenario_common;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use scenario_common::{front_world, int_of, r, rb, FrontSpec};

/// Persistent AUTH'd session whose `pipe` sends many commands in ONE
/// write and returns everything the server says until the socket
/// falls quiet.
struct Sess {
    sock: TcpStream,
}

impl Sess {
    async fn connect(addr: &str) -> Sess {
        let mut sock = TcpStream::connect(addr).await.expect("connect");
        let auth = pipe_raw(&mut sock, &[&[b"AUTH", common::TOKEN.as_bytes()]]).await;
        assert_eq!(auth, "+OK\r\n", "auth");
        Sess { sock }
    }

    async fn pipe(&mut self, cmds: &[&[&[u8]]]) -> String {
        pipe_raw(&mut self.sock, cmds).await
    }
}

/// Frame one command as RESP.
fn frame(args: &[&[u8]]) -> Vec<u8> {
    let mut f = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        f.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        f.extend_from_slice(a);
        f.extend_from_slice(b"\r\n");
    }
    f
}

/// Write every command in one shot, then drain replies until a 150ms
/// quiet gap (the batch is fully answered).
async fn pipe_raw(sock: &mut TcpStream, cmds: &[&[&[u8]]]) -> String {
    let mut buf = Vec::new();
    for c in cmds {
        buf.extend_from_slice(&frame(c));
    }
    sock.write_all(&buf).await.expect("write");
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_millis(150), sock.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => out.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) | Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn blackfriday_batches_pipeline_through_one_socket() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec::default();
    let (_front, a) = front_world("bf52", &spec).await;
    let mut s = Sess::connect(&a).await;

    // a) Seed batch -- six commands, one write: replies arrive in
    //    strict submission order.
    let seed = s
        .pipe(&[
            &[b"SET", b"{bf}deal:1001", b"49"],
            &[b"SET", b"{bf}deal:1002", b"99"],
            &[b"SET", b"{bf}gate", b"open-at-9"],
            &[b"EXPIREAT", b"{bf}gate", b"4102444800"],
            &[b"INCR", b"{bf}orders"],
            &[b"HSET", b"{bf}door", b"state", b"shut"],
        ])
        .await;
    assert_eq!(
        seed, "+OK\r\n+OK\r\n+OK\r\n:1\r\n:1\r\n:1\r\n",
        "ordered seed replies"
    );

    // b) Poison batch -- an unknown command mid-batch: the error rides
    //    inline and the trailing write still lands.
    let poison = s
        .pipe(&[
            &[b"SET", b"{bf}cfg", b"armed"],
            &[b"NOSUCHCMD", b"x"],
            &[b"INCR", b"{bf}orders"],
        ])
        .await;
    assert_eq!(
        poison, "+OK\r\n-ERR unknown command 'NOSUCHCMD'\r\n:2\r\n",
        "error does not poison the batch"
    );

    // c) Transaction batch -- the whole door-buster queues and replays
    //    inside ONE write: three +QUEUED acks, then EXEC's nested
    //    three-reply array (+OK / :3 / :1) with typed per-verb results.
    let tx = s
        .pipe(&[
            &[b"MULTI"],
            &[b"SET", b"{bf}state", b"open"],
            &[b"INCR", b"{bf}orders"],
            &[b"LPUSH", b"{bf}feed", b"doors-open"],
            &[b"EXEC"],
        ])
        .await;
    assert_eq!(
        tx, "+OK\r\n+QUEUED\r\n+QUEUED\r\n+QUEUED\r\n*3\r\n+OK\r\n:3\r\n:1\r\n",
        "pipelined transaction"
    );

    // d) Read-back: every write of every batch landed.
    assert_eq!(rb(&a, &[b"GET", b"{bf}deal:1001"]).await, "49");
    assert_eq!(rb(&a, &[b"GET", b"{bf}cfg"]).await, "armed");
    assert_eq!(rb(&a, &[b"GET", b"{bf}state"]).await, "open");
    assert_eq!(rb(&a, &[b"GET", b"{bf}orders"]).await, "3", "three orders");
    assert_eq!(rb(&a, &[b"HGET", b"{bf}door", b"state"]).await, "shut");
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"{bf}feed"]).await), 1);
    assert!(
        int_of(&r(&a, &[b"TTL", b"{bf}gate"]).await) > 0,
        "gate clock ticking"
    );
}
