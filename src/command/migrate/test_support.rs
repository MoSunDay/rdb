//! Shared test infrastructure for the `migrate` submodules: a `Shared`
//! harness per test, sync/async `handle` drivers, and a fake cluster
//! node speaking just enough RESP for the reshard protocol.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::command::test_ctx;
use crate::state::{testutil, Shared};

/// Every store-opening test holds the crate-wide lock for its whole
/// lifetime (see `string::tests`): `shared_with` wipes the shared
/// `/tmp/rdb-test-{pid}` root. Guard returned FIRST so it outlives the
/// Shared (locals drop in reverse declaration order).
pub(super) fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
    let guard = crate::command::string::TEST_STORE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut conf = testutil::test_config();
    conf.bind = bind.to_string();
    (guard, testutil::shared_with(conf))
}

pub(super) fn call(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let mut ctx = test_ctx(shared, vec![], argv, &mut out);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(super::handle(&mut ctx));
    out
}

/// `call` for `#[tokio::test]` bodies (no nested runtime).
pub(super) async fn call_async(shared: &Shared, args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let mut ctx = test_ctx(shared, vec![], argv, &mut out);
    super::handle(&mut ctx).await;
    out
}

/// Read one RESP command (array of bulk strings) from `stream`;
/// `None` on a clean EOF (the peer closed).
pub(super) async fn read_cmd(stream: &mut TcpStream, line: &mut Vec<u8>) -> Option<Vec<Vec<u8>>> {
    if !read_cmd_line(stream, line).await {
        return None;
    }
    assert_eq!(line[0], b'*');
    let n: usize = std::str::from_utf8(&line[1..])
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        if !read_cmd_line(stream, line).await {
            return None;
        }
        assert_eq!(line[0], b'$');
        let len: usize = std::str::from_utf8(&line[1..])
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut data = vec![0u8; len];
        stream.read_exact(&mut data).await.unwrap();
        let mut crlf = [0u8; 2];
        stream.read_exact(&mut crlf).await.unwrap();
        assert_eq!(&crlf, b"\r\n");
        args.push(data);
    }
    Some(args)
}

/// `false` on EOF.
pub(super) async fn read_cmd_line(stream: &mut TcpStream, buf: &mut Vec<u8>) -> bool {
    buf.clear();
    let mut byte = [0u8; 1];
    loop {
        if stream.read_exact(&mut byte).await.is_err() {
            return false;
        }
        if byte[0] == b'\r' {
            let mut next = [0u8; 1];
            if stream.read_exact(&mut next).await.is_err() {
                return false;
            }
            assert_eq!(next[0], b'\n');
            return true;
        }
        buf.push(byte[0]);
    }
}

/// Fake cluster node: AUTH +OK, then `SETSLOT`/`MIGRATE` -> +OK and
/// `GETKEYSINSLOT` -> `keys` once, `*0` afterwards. SETSLOT
/// subcommands are appended to `log`.
pub(super) async fn fake_node(
    keys: Vec<Vec<u8>>,
    log: Arc<Mutex<Vec<String>>>,
) -> (tokio::task::JoinHandle<()>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut line = Vec::new();
        let auth = read_cmd(&mut sock, &mut line).await.expect("auth frame");
        assert_eq!(auth[0], b"AUTH");
        sock.write_all(b"+OK\r\n").await.unwrap();
        let mut served = false;
        while let Some(cmd) = read_cmd(&mut sock, &mut line).await {
            // MIGRATE host port "" 0 timeout KEYS k1 ...: the empty
            // key + KEYS marker identify it (and the name is checked
            // so a missing command name cannot slip through).
            if cmd.len() >= 7 && cmd[0] == b"MIGRATE" && cmd[3].is_empty() && cmd[6] == b"KEYS" {
                sock.write_all(b"+OK\r\n").await.unwrap();
                continue;
            }
            assert_eq!(cmd[0], b"CLUSTER", "expected CLUSTER, got {:?}", cmd);
            match cmd[1].as_slice() {
                // cmd = [CLUSTER, SETSLOT, slot, SUB, id?]
                b"SETSLOT" => {
                    log.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&cmd[3]).into_owned());
                    sock.write_all(b"+OK\r\n").await.unwrap();
                }
                b"GETKEYSINSLOT" => {
                    let reply = if served {
                        b"*0\r\n".to_vec()
                    } else {
                        served = true;
                        let mut out = format!("*{}\r\n", keys.len()).into_bytes();
                        for k in &keys {
                            out.extend_from_slice(format!("${}\r\n", k.len()).as_bytes());
                            out.extend_from_slice(k);
                            out.extend_from_slice(b"\r\n");
                        }
                        out
                    };
                    sock.write_all(&reply).await.unwrap();
                }
                other => {
                    panic!(
                        "unexpected cluster subcommand {:?}",
                        String::from_utf8_lossy(other)
                    );
                }
            }
        }
    });
    (handle, addr)
}
