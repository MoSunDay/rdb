//! One-shot 2PC client: connect, send one request, read one reply.
//! Every exchange gets its own connection -- requests are tiny and
//! rare (per distributed COMMIT), so pooling would only add state.

use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::proto::{self, Req, Resp};

/// Connect+request+reply budget (loopback LANs need far less; a hung
/// participant must not stall the coordinator's commit path).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Send `req` to the participant at `sql_rpc_addr`, return its reply.
pub async fn request(sql_rpc_addr: &str, req: &Req) -> Result<Resp, String> {
    let work = async {
        let stream = TcpStream::connect(sql_rpc_addr)
            .await
            .map_err(|e| format!("connect {sql_rpc_addr}: {e}"))?;
        stream.set_nodelay(true).ok();
        let (mut r, mut w) = stream.into_split();
        proto::send(&mut w, req).await.map_err(|e| e.to_string())?;
        w.shutdown().await.map_err(|e| e.to_string())?;
        proto::recv::<_, Resp>(&mut r)
            .await
            .map_err(|e| e.to_string())
    };
    tokio::time::timeout(REQUEST_TIMEOUT, work)
        .await
        .map_err(|_| format!("request to {sql_rpc_addr} timed out"))?
}
