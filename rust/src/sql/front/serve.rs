//! MySQL listener: TCP accept loop + per-connection shim runner.
//!
//! Mirrors `resp::serve` (one task per connection, 10ms backoff on accept
//! errors). Each connection splits its socket, builds a [`SqlShim`] and
//! hands both halves to `AsyncMysqlIntermediary::run_with_options`, which
//! owns the whole packet loop; this module never touches MySQL packets
//! itself.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use opensrv_mysql::AsyncMysqlIntermediary;
use tokio::net::{TcpListener, TcpStream};

use crate::state::Shared;

use super::shim::{intermediary_options, new_shim};

/// Bind a TCP listener on `addr`. Same contract and error text as
/// `resp::bind` ("listen <addr> failed: <err>") so startup output stays
/// uniform.
pub fn bind(addr: &str) -> Result<TcpListener, String> {
    let std_listener =
        std::net::TcpListener::bind(addr).map_err(|e| format!("listen {addr} failed: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("listen {addr} failed: {e}"))?;
    TcpListener::from_std(std_listener).map_err(|e| format!("listen {addr} failed: {e}"))
}

/// Configured login user; an empty `mysql_user` means "root", like the
/// MySQL server default.
pub fn effective_user(mysql_user: &str) -> String {
    if mysql_user.is_empty() {
        "root".to_string()
    } else {
        mysql_user.to_string()
    }
}

/// Accept loop: one task per connection. Accept errors are skipped with a
/// 10ms backoff (same policy as `resp::serve`).
pub async fn serve(listener: TcpListener, shared: Arc<Shared>) -> ! {
    let user = effective_user(&shared.conf.mysql_user);
    let password = shared.conf.mysql_password.clone();
    loop {
        match listener.accept().await {
            Ok((sock, _peer)) => {
                let shared = shared.clone();
                let user = user.clone();
                let password = password.clone();
                tokio::spawn(handle_conn(sock, shared, user, password));
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        }
    }
}

/// Drive one connection to completion (handshake, queries, disconnect).
/// Connection-level errors (failed auth, reset peer, malformed packet)
/// are logged and dropped: the listener outlives every connection.
async fn handle_conn(sock: TcpStream, shared: Arc<Shared>, user: String, password: String) {
    let (read_half, write_half) = sock.into_split();
    let shim = new_shim::<tokio::net::tcp::OwnedWriteHalf>(
        Arc::clone(&shared),
        user,
        password,
        conn_seed(),
    );
    let sess = shim.session_handle();
    let opts = intermediary_options();
    if let Err(e) =
        AsyncMysqlIntermediary::run_with_options(shim, read_half, write_half, &opts).await
    {
        eprintln!("[mysql] connection ended with error: {e}");
    }
    // Connection end: a client that just drops the socket may leave a
    // BEGIN's snapshot registered; release it so the GC watermark never
    // pins on a dead session (staged writes die with it -- nothing was
    // written to the store).
    let mut sess = sess.lock().await;
    if let Some(txn) = sess.txn.take() {
        crate::sql::tx::rollback(&shared.sql_ts, txn);
    }
}

/// Seed for a new connection's scramble: wall-clock nanos mixed with a
/// process-wide counter, so simultaneous connections on restarts still
/// get distinct salts.
fn conn_seed() -> u64 {
    static CONNS: AtomicU64 = AtomicU64::new(0);
    let n = CONNS.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: the accept loop survives bare connect-and-drop clients
    /// (e2e protocol behavior lives in tests/sql_e2e.rs).
    #[tokio::test]
    async fn serve_accepts_bare_connections() {
        let shared = Arc::new(crate::state::testutil::shared_with(
            crate::state::testutil::test_config(),
        ));
        let listener = bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(serve(listener, shared));

        let first = TcpStream::connect(addr).await.expect("connect 1");
        drop(first);
        // The loop must still be alive after the first conn's EOF path.
        let second = TcpStream::connect(addr).await.expect("connect 2");
        drop(second);
        // Let the spawned handlers run through their error paths.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[test]
    fn bind_error_text_matches_resp_style() {
        let err = bind("999.999.999.999:99").unwrap_err();
        assert!(
            err.starts_with("listen 999.999.999.999:99 failed: "),
            "{err}"
        );
    }

    #[test]
    fn empty_mysql_user_defaults_to_root() {
        assert_eq!(effective_user(""), "root");
        assert_eq!(effective_user("alice"), "alice");
    }

    #[test]
    fn conn_seed_is_fresh_per_call() {
        let a = conn_seed();
        let b = conn_seed();
        assert_ne!(a, b);
    }
}
