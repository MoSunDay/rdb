//! The SQL 2PC RPC listener (`sql_rpc_bind`; empty = disabled).
//!
//! One task per connection; requests are handled strictly serially per
//! NODE behind a single mutex, which closes the check-then-write race
//! between two concurrent Prepares on the same unique value or row --
//! both would pass validation independently, but the serialized batch
//! writer makes the second one see the first one's staged state.

use std::io;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};

use super::client;
use super::participant;
use super::proto::{self, Outcome, Req, Resp};
use crate::state::Shared;

/// Bind `addr` for the 2PC transport (fatal-error style of the other
/// listeners: returns the failure text, never a socket on error).
pub fn bind(addr: &str) -> Result<TcpListener, String> {
    let std_listener =
        std::net::TcpListener::bind(addr).map_err(|e| format!("listen {addr} failed: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("listen {addr} failed: {e}"))?;
    TcpListener::from_std(std_listener).map_err(|e| format!("listen {addr} failed: {e}"))
}

/// Bind `addr` and serve 2PC requests forever.
pub async fn serve(addr: &str, shared: Arc<Shared>) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    serve_on(listener, shared).await
}

/// Serve on an already-bound listener.
pub async fn serve_on(listener: TcpListener, shared: Arc<Shared>) -> io::Result<()> {
    let mux = Arc::new(tokio::sync::Mutex::new(()));
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("sql rpc: accept failed: {e}");
                continue;
            }
        };
        let shared = Arc::clone(&shared);
        let mux = Arc::clone(&mux);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, shared, mux).await {
                eprintln!("sql rpc: connection ended: {e}");
            }
        });
    }
}

/// One connection: request -> reply until EOF or a bad frame.
async fn handle(
    stream: TcpStream,
    shared: Arc<Shared>,
    mux: Arc<tokio::sync::Mutex<()>>,
) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    let (mut r, mut w) = stream.into_split();
    loop {
        let req: Req = match proto::recv(&mut r).await {
            Ok(req) => req,
            Err(_) => return Ok(()), // EOF or garbage: close quietly
        };
        let resp = dispatch(&req, &shared, &mux).await;
        proto::send(&mut w, &resp).await?;
    }
}

/// Map one request to its reply. All store mutations sit behind the
/// node-wide 2PC mutex (see module doc).
async fn dispatch(req: &Req, shared: &Shared, mux: &tokio::sync::Mutex<()>) -> Resp {
    match req {
        Req::Ping => Resp::Pong,
        Req::Prepare {
            txn_id,
            coordinator,
            commit_ts,
            read_ts,
            entries,
        } => {
            let _guard = mux.lock().await;
            match participant::vote(
                &shared.store,
                txn_id,
                coordinator,
                *commit_ts,
                *read_ts,
                entries,
            ) {
                Ok(participant::Vote::Yes) => Resp::Vote {
                    yes: true,
                    reason: String::new(),
                },
                Ok(participant::Vote::No(reason)) => Resp::Vote { yes: false, reason },
                Err(e) => Resp::Vote {
                    yes: false,
                    reason: format!("error: {e}"),
                },
            }
        }
        Req::Decide {
            txn_id,
            commit,
            watermark,
            index_ops,
        } => {
            let _guard = mux.lock().await;
            match participant::decide(&shared.store, txn_id, *commit, index_ops) {
                Ok(commit_ts) => {
                    // The flipped rows carry ts values this node never
                    // allocated: raise its read point or local snapshots
                    // would stay below the commit forever. The
                    // coordinator's watermark covers the rows that
                    // landed on OTHER participants of the same txn (a
                    // later scatter-gather read here must see them).
                    if *commit {
                        shared.sql_ts.advance_to((*watermark).max(commit_ts));
                    }
                    Resp::Ack
                }
                Err(e) => Resp::Vote {
                    yes: false,
                    reason: format!("error: {e}"),
                },
            }
        }
        Req::TxnStatus { txn_id, node } => Resp::Status {
            outcome: participant::status(&shared.store, txn_id, node),
        },
        Req::ScanBand {
            table_id,
            slot_lo,
            slot_hi,
            read_ts,
        } => {
            // Read-only band scan: no mutex, it cannot race the 2PC
            // write path any worse than a local scan could (MVCC
            // versions are immutable once written).
            let res =
                crate::sql::exec::scan::band_rows(shared, *table_id, *read_ts, *slot_lo, *slot_hi);
            match res {
                Ok(rows) => Resp::BandRows {
                    rows,
                    error: String::new(),
                },
                Err(e) => Resp::BandRows {
                    rows: Vec::new(),
                    error: e.to_string(),
                },
            }
        }
    }
}

/// Coordinator helper: address to reach a participant's 2PC port for
/// its RESP address (resolved through the `sql_nodes` registry; None
/// when the peer has not registered its sql_rpc bind yet).
pub fn sql_rpc_of(shared: &Shared, resp_addr: &str) -> Option<String> {
    if resp_addr == shared.conf.bind {
        return None; // self: direct call, no TCP
    }
    let binds = crate::sql::tx::nodes::binds_by_resp(&shared.raft, resp_addr)?;
    (!binds.sql_rpc.is_empty()).then_some(binds.sql_rpc)
}

/// Health probe used by tests and startup wiring.
pub async fn ping(sql_rpc_addr: &str) -> Result<(), String> {
    match client::request(sql_rpc_addr, &Req::Ping).await {
        Ok(Resp::Pong) => Ok(()),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(e),
    }
}

/// TxnStatus over TCP (mainly tests; recovery uses HTTP).
pub async fn txn_status(sql_rpc_addr: &str, txn_id: &str, node: &str) -> Result<Outcome, String> {
    match client::request(
        sql_rpc_addr,
        &Req::TxnStatus {
            txn_id: txn_id.to_string(),
            node: node.to_string(),
        },
    )
    .await
    {
        Ok(Resp::Status { outcome }) => Ok(outcome),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(e),
    }
}
