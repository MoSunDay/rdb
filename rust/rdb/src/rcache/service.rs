//! Raft RPC server for rcache (Rust port of the server side of the Go
//! `internal/rcache` TCP transport): accepts connections on the node's
//! RaftTCPAddress, reads length-prefixed JSON frames and dispatches them
//! to the local openraft instance.
//!
//! Robustness follows the Go original: per-connection errors are logged
//! and only that connection task exits; the accept loop keeps serving.
//! Logging uses plain `eprintln!`, consistent with `src/main.rs` (the
//! crate has no logging framework).

use std::io;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};

use crate::rcache::transport::{read_frame, write_frame, InMsg, OutMsg};
use crate::rcache::RdbRaft;

/// Bind `addr` (a RaftTCPAddress like `127.0.0.1:32681`) and serve raft
/// RPCs forever; returns only when binding fails.
pub async fn serve(addr: String, raft: Arc<RdbRaft>) -> io::Result<()> {
    let listener = TcpListener::bind(&addr).await?;
    serve_on(listener, raft).await
}

/// Serve on an already bound listener (tests use this with ephemeral
/// ports). Runs the accept loop forever.
pub async fn serve_on(listener: TcpListener, raft: Arc<RdbRaft>) -> io::Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let raft = raft.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, raft).await {
                        // Peer closed the connection: routine, stay quiet.
                        if e.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("rcache raft rpc: connection {peer} failed: {e}");
                        }
                    }
                });
            }
            Err(e) => eprintln!("rcache raft rpc: accept failed: {e}"),
        }
    }
}

/// One connection: frame in -> dispatch -> frame out, until the peer
/// disconnects or the socket breaks.
async fn handle_conn(mut stream: TcpStream, raft: Arc<RdbRaft>) -> io::Result<()> {
    loop {
        let frame = read_frame(&mut stream).await?;
        let msg: InMsg = serde_json::from_slice(&frame).map_err(|e| {
            eprintln!("rcache raft rpc: dropping malformed frame: {e}");
            io::Error::new(io::ErrorKind::InvalidData, e)
        })?;
        let out = dispatch(msg, &raft).await;
        let payload =
            serde_json::to_vec(&out).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_frame(&mut stream, &payload).await?;
    }
}

/// Route a request frame to the matching openraft handler.
async fn dispatch(msg: InMsg, raft: &RdbRaft) -> OutMsg {
    match msg {
        InMsg::AppendEntries(req) => OutMsg::AppendEntries(raft.append_entries(req).await),
        InMsg::Vote(req) => OutMsg::Vote(raft.vote(req).await),
        InMsg::InstallSnapshot(req) => OutMsg::InstallSnapshot(raft.install_snapshot(req).await),
    }
}
