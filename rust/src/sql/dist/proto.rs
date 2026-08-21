//! Wire protocol of the M3 node-to-node SQL 2PC transport.
//!
//! Framing mirrors `rcache::transport` exactly: one u32 big-endian
//! length prefix followed by that many bytes of JSON. One connection
//! carries any number of request/response pairs; every reply matches
//! the request kind it answers (`Prepare` -> `Vote`, `Decide` ->
//! `Ack`, `TxnStatus` -> `Status`, `Ping` -> `Pong`, `ScanBand` ->
//! `BandRows`), so a client with one outstanding request per
//! connection needs no correlation ids.
//!
//! All payloads are JSON (serde): `Vec<u8>` fields encode as number
//! arrays -- verbose but inspectable, and the messages are small (one
//! row version per prepared entry).

use serde::{Deserialize, Serialize};

use crate::rcache::transport::{read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// One prepared write a participant must stage atomically.
///
/// - `RowPrepared`: `key` is the FINAL row-version key (slot prefix +
///   kind + table id + pk + inverted commit ts -- the ts the
///   coordinator allocated), `value` the final version payload with
///   its header byte swapped to 0x02; the commit decision flips that
///   byte in place.
/// - `UniquePut` / `UniqueDel`: a 0x22 unique-index reservation
///   (`value` = owning pk key); reservations ride the SAME prepare
///   batch so a concurrent prepare on the same unique value sees the
///   owner and vetoes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub kind: EntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    RowPrepared,
    UniquePut,
    UniqueDel,
}

/// One secondary-index (0x21) entry op applied only at Decide{commit}:
/// `Some(value)` = put, `None` = delete. Index entries carry no
/// timestamp (latest-committed-state pointers), so they must NOT move
/// before the decision -- a snapshot reader would otherwise see the
/// in-flight write.
pub type WireOp = (Vec<u8>, Option<Vec<u8>>);

/// Participant recovery answer for one txn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Decided commit; carries the asking participant's index ops so a
    /// participant that lost the Decide message can still finish.
    Committed {
        index_ops: Vec<WireOp>,
    },
    Aborted,
    Unknown,
}

/// Request messages (client -> participant).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Req {
    Ping,
    Prepare {
        txn_id: String,
        /// Coordinator HTTP control address (`/sql2pc/status` lives
        /// there) for participant-side recovery queries.
        coordinator: String,
        commit_ts: u64,
        read_ts: u64,
        entries: Vec<Entry>,
    },
    Decide {
        txn_id: String,
        commit: bool,
        /// The txn's highest granted ts (`ts.end - 1`). Rows of one
        /// distributed commit carry CONSECUTIVE ts values spread over
        /// several participants, so a participant advancing only past
        /// its own rows would stay blind to the rest of the txn --
        /// exactly what a later scatter-gather read on that node must
        /// not be. 0 = absent (pre-watermark frames): keep the old
        /// local-max advance.
        #[serde(default)]
        watermark: u64,
        #[serde(default)]
        index_ops: Vec<WireOp>,
    },
    TxnStatus {
        txn_id: String,
        /// Asking participant's RESP address: a committed outcome only
        /// returns THAT node's index ops.
        node: String,
    },
    /// M3 scatter-gather read: every row of `table_id` whose slot lies
    /// in `[slot_lo, slot_hi]` (inclusive), visible at `read_ts`. The
    /// participant answers with the SAME bytes its local scan would
    /// materialize (see [`Resp::BandRows`]).
    ScanBand {
        table_id: u32,
        slot_lo: u16,
        slot_hi: u16,
        read_ts: u64,
    },
}

/// Response messages (participant -> client).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resp {
    Pong,
    Vote {
        yes: bool,
        reason: String,
    },
    Ack,
    Status {
        outcome: Outcome,
    },
    /// M3 scatter-gather reply: the band's visible rows as
    /// `(pk_key, raw version payload)` pairs in pk order -- the same
    /// bytes a local scan of that band would decode. `error` is empty
    /// on success; a non-empty error fails the coordinator's WHOLE
    /// query (partial results are never served).
    BandRows {
        rows: Vec<(Vec<u8>, Vec<u8>)>,
        #[serde(default)]
        error: String,
    },
}

/// Encode and write one frame.
pub async fn send<W: AsyncWrite + Unpin>(w: &mut W, msg: &impl Serialize) -> std::io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(proto_err)?;
    write_frame(w, &payload).await
}

/// Read and decode one frame.
pub async fn recv<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> std::io::Result<T> {
    let payload = read_frame(r).await?;
    serde_json::from_slice(&payload).map_err(proto_err)
}

/// One live connection's halves (client convenience wrapper).
pub struct Channel {
    pub reader: OwnedReadHalf,
    pub writer: OwnedWriteHalf,
}

fn proto_err(e: serde_json::Error) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("bad proto frame: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    #[test]
    fn messages_roundtrip_through_frames() {
        let e = Entry {
            key: vec![1, 2, 3],
            value: vec![2, 0xff],
            kind: EntryKind::UniquePut,
        };
        let reqs = vec![
            Req::Ping,
            Req::Prepare {
                txn_id: "t7-n1".into(),
                coordinator: "127.0.0.1:9".into(),
                commit_ts: 42,
                read_ts: 40,
                entries: vec![e.clone()],
            },
            Req::Decide {
                txn_id: "t7".into(),
                commit: true,
                watermark: 87,
                index_ops: vec![(vec![9], None), (vec![8], Some(vec![7]))],
            },
            Req::TxnStatus {
                txn_id: "t7".into(),
                node: "127.0.0.1:1".into(),
            },
        ];
        rt().block_on(async {
            let mut buf = Vec::new();
            for req in &reqs {
                send(&mut buf, req).await.unwrap();
                let back: Req = recv(&mut Cursor::new(&buf)).await.unwrap();
                assert_eq!(&back, req);
                buf.clear();
            }
        });
    }

    #[test]
    fn responses_roundtrip_through_frames() {
        let resps = vec![
            Resp::Pong,
            Resp::Vote {
                yes: false,
                reason: "dup: uq".into(),
            },
            Resp::Ack,
            Resp::Status {
                outcome: Outcome::Committed {
                    index_ops: vec![(vec![1], Some(vec![2]))],
                },
            },
            Resp::Status {
                outcome: Outcome::Unknown,
            },
            Resp::BandRows {
                rows: vec![(vec![1, 0xff], vec![0x01, 0x00, 0x09])],
                error: String::new(),
            },
            Resp::BandRows {
                rows: Vec::new(),
                error: "table 9 vanished".into(),
            },
        ];
        rt().block_on(async {
            let mut buf = Vec::new();
            for resp in &resps {
                send(&mut buf, resp).await.unwrap();
                let back: Resp = recv(&mut Cursor::new(&buf)).await.unwrap();
                assert_eq!(&back, resp);
                buf.clear();
            }
        });
    }

    #[test]
    fn wire_enum_names_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&EntryKind::RowPrepared).unwrap(),
            r#""row_prepared""#
        );
        assert_eq!(
            serde_json::to_string(&Outcome::Aborted).unwrap(),
            r#""aborted""#
        );
    }
}
