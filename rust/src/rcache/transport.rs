//! Raft RPC transport for rcache (Rust port of the TCP transport of Go
//! `internal/rcache`): length-prefixed JSON frames over plain TCP.
//!
//! Wire format: a `u32` big-endian payload length followed by one
//! JSON-encoded [`InMsg`] (request) or [`OutMsg`] (response), matching the
//! Go codec that framed each RPC as a single length-delimited JSON message.
//!
//! Node ids: Go identifies raft nodes by their RaftTCPAddress string, but
//! openraft 0.9.25 needs a numeric `NodeId`; [`node_id_of`] derives the id
//! deterministically from the address (first 16 hex chars of
//! `utils.MD5With40`, parsed as u64). The address string itself still
//! travels as the `Node` payload of every membership config.
//!
//! Deviation from Go: the original keeps a pool of 3 connections per peer;
//! openraft serializes RPCs to each peer itself, so a single persistent
//! connection per peer (re-established on error) is sufficient.
#![allow(clippy::result_large_err)] // openraft's RaftNetwork signatures force large Errs

use std::io;
use std::time::Duration;

use openraft::error::{NetworkError, RPCError, RaftError, RemoteError, Timeout, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::RPCTypes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::rcache::{typ, Node, NodeId, TypeConfig};
use crate::utils::md5_with40;

/// Per-RPC deadline; Go used a 10s deadline on every dial + write + read.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Refuse frames above 256 MiB; the rcache payload is tiny (log entries,
/// small KV snapshots), so this only guards against corrupt length fields.
const MAX_FRAME_BYTES: u32 = 256 * 1024 * 1024;

/// Body bytes read per step: a lying (but under-cap) length header then
/// costs only the bytes actually streamed before the short read trips,
/// never one giant up-front allocation.
const READ_CHUNK_BYTES: usize = 256 * 1024;

/// TCP keepalive profile for raft sockets: after 60s idle, probe every
/// 10s up to 3 times — a silently-dead peer (cable pull, NAT/VM drop) is
/// detected within ~90s by the OS instead of pinning the socket until
/// the RPC deadline notices. Errors are ignored: keepalive is an
/// optimization, the deadline is the bound.
pub fn apply_tcp_keepalive(stream: &TcpStream) {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    let _ = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

/// Deterministic raft node id of a RaftTCPAddress: the first 16 hex chars
/// of `utils.MD5With40(addr)` parsed as a u64 (Go uses the raw address
/// string as `raft.ServerID`; openraft needs a numeric id instead).
pub fn node_id_of(addr: &str) -> NodeId {
    let hash = md5_with40(addr);
    u64::from_str_radix(&hash[..16], 16).expect("md5 hex is always valid")
}

/// Inbound RPC request (client -> server), one variant per raft RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InMsg {
    AppendEntries(AppendEntriesRequest<TypeConfig>),
    Vote(VoteRequest<NodeId>),
    InstallSnapshot(InstallSnapshotRequest<TypeConfig>),
}

/// Outbound RPC reply (server -> client): the result of dispatching the
/// matching [`InMsg`] to the local raft instance, including its error so
/// the caller sees openraft errors (e.g. ForwardToLeader) verbatim.
#[derive(Debug, Serialize, Deserialize)]
pub enum OutMsg {
    AppendEntries(Result<AppendEntriesResponse<NodeId>, typ::RaftError>),
    Vote(Result<VoteResponse<NodeId>, typ::RaftError>),
    InstallSnapshot(
        Result<
            InstallSnapshotResponse<NodeId>,
            typ::RaftError<openraft::error::InstallSnapshotError>,
        >,
    ),
}

/// Write one length-prefixed frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame exceeds u32"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(payload).await?;
    w.flush().await
}

/// Read one length-prefixed frame. The declared length is first checked
/// against [`MAX_FRAME_BYTES`]; the body is then read in
/// [`READ_CHUNK_BYTES`] steps through the buffer's spare capacity, so the
/// wire format (u32 big-endian length + payload) is unchanged while a
/// corrupt-but-under-cap header never triggers a giant single allocation.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the {MAX_FRAME_BYTES} byte limit"),
        ));
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut remaining = len as usize;
    while remaining > 0 {
        buf.reserve(remaining.min(READ_CHUNK_BYTES));
        // `Vec<u8>` doubles as its own read buffer here (bytes::BufMut):
        // the vec's len advances with every read, no second copy.
        let n = r.read_buf(&mut buf).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("frame body truncated: {} of {len} bytes", len as usize - remaining),
            ));
        }
        remaining -= n;
    }
    Ok(buf)
}

/// Network factory handing out one [`Connection`] per peer.
pub struct Transport {
    local_id: NodeId,
}

/// Build a transport factory; `local_id` is only used to fill the
/// informational `Timeout` error when an RPC deadline expires.
pub fn new(local_id: NodeId) -> Transport {
    Transport { local_id }
}

impl RaftNetworkFactory<TypeConfig> for Transport {
    type Network = Connection;

    async fn new_client(&mut self, target: NodeId, node: &Node) -> Self::Network {
        Connection {
            addr: node.clone(),
            local_id: self.local_id,
            target,
            stream: None,
        }
    }
}

/// One persistent connection to a peer (see module docs for the pooling
/// deviation from Go). Lazily connects on the first RPC and drops the
/// socket on any error so the next RPC reconnects.
pub struct Connection {
    /// RaftTCPAddress of the peer.
    addr: String,
    local_id: NodeId,
    target: NodeId,
    stream: Option<TcpStream>,
}

impl Connection {
    /// Send one request frame and await the response frame, bounded by
    /// [`RPC_TIMEOUT`] (Go: 10s stream deadline per RPC). The dial is
    /// part of the budget: a blackholed peer fails fast as
    /// `Unreachable` instead of hanging past the deadline semantics.
    async fn call<E: std::error::Error>(
        &mut self,
        msg: &InMsg,
        action: RPCTypes,
    ) -> Result<OutMsg, typ::RPCError<E>> {
        match timeout(RPC_TIMEOUT, self.ensure_connected()).await {
            Err(_elapsed) => {
                // Cancelling the connect future dropped the half-open
                // socket; surface as Unreachable so openraft backs off.
                self.drop_stream();
                let e = io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("connect to {} timed out after {:?}", self.addr, RPC_TIMEOUT),
                );
                return Err(RPCError::Unreachable(Unreachable::new(&e)));
            }
            // The peer cannot be reached at all: advise openraft to back
            // off instead of retrying immediately.
            Ok(Err(e)) => return Err(RPCError::Unreachable(Unreachable::new(&e))),
            Ok(Ok(())) => {}
        }
        let stream = self
            .stream
            .as_mut()
            .expect("stream is Some right after ensure_connected");

        let payload = match serde_json::to_vec(msg) {
            Ok(p) => p,
            Err(e) => return Err(network_err(&e)),
        };

        let res = timeout(RPC_TIMEOUT, roundtrip(stream, &payload)).await;
        let frame = match res {
            Err(_elapsed) => {
                self.drop_stream();
                return Err(RPCError::Timeout(Timeout {
                    action,
                    id: self.local_id,
                    target: self.target,
                    timeout: RPC_TIMEOUT,
                }));
            }
            // A mid-connection IO error: drop the socket (the next RPC
            // reconnects) and let openraft retry immediately.
            Ok(Err(e)) => {
                self.drop_stream();
                return Err(network_err(&e));
            }
            Ok(Ok(frame)) => frame,
        };

        match serde_json::from_slice::<OutMsg>(&frame) {
            Ok(out) => Ok(out),
            Err(e) => {
                self.drop_stream();
                Err(network_err(&e))
            }
        }
    }

    /// Connect (bounded by the caller's [`RPC_TIMEOUT`]) unless a live
    /// stream is already held; nodelay for latency, keepalive to notice
    /// silently-dead peers (see [`apply_tcp_keepalive`]).
    async fn ensure_connected(&mut self) -> io::Result<()> {
        if self.stream.is_none() {
            let stream = TcpStream::connect(&self.addr).await?;
            stream.set_nodelay(true).ok();
            apply_tcp_keepalive(&stream);
            self.stream = Some(stream);
        }
        Ok(())
    }

    fn drop_stream(&mut self) {
        self.stream = None;
    }
}

/// Write the request frame and read the response frame on one connection.
async fn roundtrip(stream: &mut TcpStream, payload: &[u8]) -> io::Result<Vec<u8>> {
    write_frame(stream, payload).await?;
    read_frame(stream).await
}

/// Map a local transport failure into `RPCError::Network`.
fn network_err<E: std::error::Error>(e: &(impl std::error::Error + 'static)) -> typ::RPCError<E> {
    RPCError::Network(NetworkError::new(e))
}

/// Wrap the raft-level error returned by the peer into `RemoteError`.
fn remote_res<E: std::error::Error, T>(
    target: NodeId,
    res: Result<T, RaftError<NodeId, E>>,
) -> Result<T, typ::RPCError<E>> {
    res.map_err(|e| RPCError::RemoteError(RemoteError::new(target, e)))
}

/// A response frame of the wrong variant is a protocol violation.
fn mismatch<E: std::error::Error>(want: &str, got: &OutMsg) -> typ::RPCError<E> {
    let e = io::Error::new(
        io::ErrorKind::InvalidData,
        format!("rcache raft rpc: expected {want} response, got {got:?}"),
    );
    network_err(&e)
}

impl RaftNetwork<TypeConfig> for Connection {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, typ::RPCError> {
        let out = self
            .call(&InMsg::AppendEntries(rpc), RPCTypes::AppendEntries)
            .await?;
        match out {
            OutMsg::AppendEntries(res) => remote_res(self.target, res),
            other => Err(mismatch("AppendEntries", &other)),
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, typ::RPCError> {
        let out = self.call(&InMsg::Vote(rpc), RPCTypes::Vote).await?;
        match out {
            OutMsg::Vote(res) => remote_res(self.target, res),
            other => Err(mismatch("Vote", &other)),
        }
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<InstallSnapshotResponse<NodeId>, typ::RPCError<openraft::error::InstallSnapshotError>>
    {
        let out = self
            .call(&InMsg::InstallSnapshot(rpc), RPCTypes::InstallSnapshot)
            .await?;
        match out {
            OutMsg::InstallSnapshot(res) => remote_res(self.target, res),
            other => Err(mismatch("InstallSnapshot", &other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use openraft::{CommittedLeaderId, LogId, Vote};

    use super::*;
    use crate::rtypes::RaftLogEntryData;

    #[test]
    fn node_id_is_deterministic() {
        let a = node_id_of("127.0.0.1:32681");
        assert_eq!(a, node_id_of("127.0.0.1:32681"));
        // First 16 hex chars of MD5With40("127.0.0.1:32681")
        // = "844806f0817b5100" (see utils::md5_with40 fixture).
        assert_eq!(a, u64::from_str_radix("844806f0817b5100", 16).unwrap());
    }

    #[test]
    fn node_ids_differ_across_addresses() {
        let ids = [
            node_id_of("127.0.0.1:32681"),
            node_id_of("127.0.0.1:32682"),
            node_id_of("10.0.0.1:7000"),
            node_id_of(""),
        ];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "ids[{i}] and ids[{j}] must differ");
            }
        }
    }

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let payload = serde_json::to_vec(&"hello").unwrap();
        let p = payload.clone();
        let writer = tokio::spawn(async move { write_frame(&mut client, &p).await });
        let got = read_frame(&mut server).await.unwrap();
        writer.await.unwrap().unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        let mut buf = (MAX_FRAME_BYTES + 1).to_be_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 4]);
        let mut cur = std::io::Cursor::new(buf);
        let err = read_frame(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// A frame larger than one read chunk (600 KiB > 256 KiB) must be
    /// reassembled byte-identically across several chunked reads.
    #[tokio::test]
    async fn large_frame_roundtrips_across_chunks() {
        let (mut client, mut server) = tokio::io::duplex(READ_CHUNK_BYTES);
        let payload: Vec<u8> = (0..600_000).map(|i| (i % 251) as u8).collect();
        let p = payload.clone();
        let writer = tokio::spawn(async move { write_frame(&mut client, &p).await });
        let got = read_frame(&mut server).await.unwrap();
        writer.await.unwrap().unwrap();
        assert_eq!(got.len(), payload.len());
        assert_eq!(got, payload);
    }

    /// A lying-but-under-cap length header streams fewer bytes than it
    /// declares: the chunked reader must stop at EOF with
    /// UnexpectedEof (the kind service.rs treats as routine), not hang
    /// or allocate the full lie.
    #[tokio::test]
    async fn truncated_body_under_cap_errors_with_unexpected_eof() {
        let mut buf = 1_000_000u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"only a few real bytes");
        let mut cur = std::io::Cursor::new(buf);
        let err = read_frame(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A request and a response (both Ok and Err) must survive the wire
    /// encoding byte-for-byte.
    #[tokio::test]
    async fn in_out_msg_serde_roundtrip() {
        let ae = AppendEntriesRequest::<TypeConfig> {
            vote: Vote::new_committed(3, 1),
            prev_log_id: Some(LogId::new(CommittedLeaderId::new(2, 1), 7)),
            entries: vec![typ::Entry {
                log_id: LogId::new(CommittedLeaderId::new(3, 1), 8),
                payload: openraft::EntryPayload::Normal(RaftLogEntryData {
                    key: "store/set".to_string(),
                    value: r#"{"slot":1}"#.to_string(),
                }),
            }],
            leader_commit: Some(LogId::new(CommittedLeaderId::new(3, 1), 7)),
        };
        let in_msg = InMsg::AppendEntries(ae);
        let js = serde_json::to_string(&in_msg).unwrap();
        let back: InMsg = serde_json::from_str(&js).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), js);
        assert!(matches!(back, InMsg::AppendEntries(_)));

        let ok = OutMsg::Vote(Ok(VoteResponse {
            vote: Vote::new_committed(3, 1),
            vote_granted: true,
            last_log_id: None,
        }));
        let js = serde_json::to_string(&ok).unwrap();
        let back: OutMsg = serde_json::from_str(&js).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), js);

        let err = OutMsg::AppendEntries(Err(openraft::error::RaftError::Fatal(
            openraft::error::Fatal::Stopped,
        )));
        let js = serde_json::to_string(&err).unwrap();
        let back: OutMsg = serde_json::from_str(&js).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), js);
    }
}
