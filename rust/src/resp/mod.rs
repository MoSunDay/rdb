//! RESP server layer: TCP listener + per-connection command machine.
//!
//! Mirrors Go `internal/server/server.go`: bind on the configured address,
//! accept connections, hand each socket to `conn::handle_conn`. The AUTH
//! gate and `-ERR: NOAUTH` text come from the MoSunDay/redcon fork; the
//! command dispatch/routing pipeline itself lives in `conn.rs`.

pub mod codec;
pub mod conn;

use std::sync::Arc;
use std::time::Duration;

use crate::state;

/// Bind a TCP listener on `addr`. Error text mirrors Go
/// `confLogger.Fatal(fmt.Sprintf("listen %s failed: %s", ...))`.
///
/// Synchronous (std listener converted to tokio) so callers can grab
/// `local_addr()` before handing the listener to [`serve`].
pub fn bind(addr: &str) -> Result<tokio::net::TcpListener, String> {
    let std_listener =
        std::net::TcpListener::bind(addr).map_err(|e| format!("listen {addr} failed: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("listen {addr} failed: {e}"))?;
    tokio::net::TcpListener::from_std(std_listener)
        .map_err(|e| format!("listen {addr} failed: {e}"))
}

/// Accept loop (Go redcon `ListenAndServe`): one task per connection.
/// Accept errors are skipped, matching redcon's silent retry, but with a
/// 10ms backoff so an EMFILE-style error storm cannot busy-spin the loop.
pub async fn serve(listener: tokio::net::TcpListener, shared: Arc<state::Shared>) -> ! {
    loop {
        match listener.accept().await {
            Ok((sock, _peer)) => {
                let shared = shared.clone();
                tokio::spawn(conn::handle_conn(sock, shared));
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conf;
    use crate::monitor;
    use crate::store;
    use crate::topology;
    use std::sync::RwLock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn ephemeral_shared() -> Arc<state::Shared> {
        let c = conf::Config {
            bind: "127.0.0.1:0".to_string(),
            raft_token: "tok".to_string(),
            ..Default::default()
        };
        let dir = std::env::temp_dir().join(format!("rdb-resp-mod-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = store::data_path(dir.to_str().unwrap(), &c.bind);
        let st = store::open(path.to_str().unwrap()).unwrap();
        Arc::new(state::Shared {
            mode: state::Mode::Normal,
            store: Arc::new(st),
            topology: Arc::new(RwLock::new(topology::empty())),
            raft: Arc::new(RwLock::new(state::stub_raft(&c))),
            monitor: Arc::new(monitor::new_collector()),
            latch: crate::ds::latch::Latch::new(),
            wait_hub: crate::ds::wait::WaitHub::new(),
            lite: std::sync::Arc::new(crate::lite::new_runtime()),
            conf: c,
        })
    }

    #[tokio::test]
    async fn bind_error_text_and_serve_roundtrip() {
        let err = bind("999.999.999.999:99").unwrap_err();
        assert!(
            err.starts_with("listen 999.999.999.999:99 failed: "),
            "{err}"
        );

        let shared = ephemeral_shared();
        let listener = bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(serve(listener, shared));

        let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
        // Pre-auth anything is rejected with the fork's exact NOAUTH text.
        sock.write_all(b"*1\r\n$4\r\nPING\r\n").await.unwrap();
        let mut buf = vec![0u8; 14];
        tokio::time::timeout(std::time::Duration::from_secs(5), sock.read_exact(&mut buf))
            .await
            .expect("timeout")
            .expect("read");
        assert_eq!(buf, b"-ERR: NOAUTH\r\n");
    }
}
