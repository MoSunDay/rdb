//! Kafka wire-protocol front (P0 skeleton): a protocol-only adapter in
//! the `sql::front` mold -- the Lite engine keeps serving Redis Streams
//! verbs; this front translates the Kafka handshakes onto it.
//!
//! Mapping (see `features/kafka-front.md`): topic = Lite parent,
//! partition = Lite child queue, offset = entry ordinal. P0 implements
//! the bootstrap pair ApiVersions (18) + Metadata (3); produce/fetch/
//! group coordination land in P1-P3.
//!
//! Lifecycle mirrors `resp::serve`/`sql::front::serve`: [`bind`] the
//! configured `kafka_bind`, [`serve`] accepts and spawns one task per
//! connection (`conn::handle_conn` owns the frame loop). Empty
//! `kafka_bind` disables the front; the backup listener never wires it.

pub mod catalog;
#[cfg(feature = "kafka-codecs")]
pub mod codec;
#[cfg(all(test, feature = "kafka-codecs"))]
#[path = "codec_tests.rs"]
mod codec_tests;
pub mod conn;
pub mod coordinator;
pub mod errors;
pub mod fetch;
pub mod fetch_records;
#[cfg(test)]
#[path = "fetch_tests.rs"]
mod fetch_tests;
pub mod frame;
pub mod handshake;
pub mod ledger;
pub mod mapping;
pub mod offsets_commit;
#[cfg(test)]
#[path = "offsets_commit_tests.rs"]
mod offsets_commit_tests;
pub mod offsets_query;
pub mod produce;
#[cfg(test)]
#[path = "produce_tests.rs"]
mod produce_tests;
pub mod record;

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

use crate::state;

// ---- api registry ---------------------------------------------------------

pub const API_KEY_PRODUCE: i16 = 0; // P1
pub const API_KEY_FETCH: i16 = 1; // P2
pub const API_KEY_LIST_OFFSETS: i16 = 2; // P1
pub const API_KEY_METADATA: i16 = 3; // P0
pub const API_KEY_OFFSET_COMMIT: i16 = 8; // P2
pub const API_KEY_OFFSET_FETCH: i16 = 9; // P2
pub const API_KEY_FIND_COORDINATOR: i16 = 10; // P3
pub const API_KEY_JOIN_GROUP: i16 = 11; // P3
pub const API_KEY_HEARTBEAT: i16 = 12; // P3
pub const API_KEY_LEAVE_GROUP: i16 = 13; // P3
pub const API_KEY_SYNC_GROUP: i16 = 14; // P3
pub const API_KEY_DESCRIBE_GROUPS: i16 = 15; // P3
pub const API_KEY_API_VERSIONS: i16 = 18; // P0

/// Implemented api keys with their supported version ranges, sorted by
/// key (the ApiVersions body order).
pub fn implemented_apis() -> Vec<(i16, i16, i16)> {
    vec![
        (API_KEY_PRODUCE, 0, 3),
        (API_KEY_FETCH, 0, 10),
        (API_KEY_LIST_OFFSETS, 0, 1),
        (API_KEY_METADATA, 0, 8),
        (API_KEY_OFFSET_COMMIT, 0, 2),
        (API_KEY_OFFSET_FETCH, 0, 7),
        (API_KEY_FIND_COORDINATOR, 0, 1),
        (API_KEY_JOIN_GROUP, 0, 4),
        (API_KEY_HEARTBEAT, 0, 4),
        (API_KEY_LEAVE_GROUP, 0, 2),
        (API_KEY_SYNC_GROUP, 0, 4),
        (API_KEY_DESCRIBE_GROUPS, 0, 3),
        (API_KEY_API_VERSIONS, 0, 3),
    ]
}

/// Version range of an implemented api; `None` = unknown api key.
pub fn api_range(key: i16) -> Option<(i16, i16)> {
    implemented_apis()
        .into_iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, lo, hi)| (lo, hi))
}

/// `true` when `version` of `key` is dispatchable.
pub fn api_supported(key: i16, version: i16) -> bool {
    api_range(key)
        .map(|(lo, hi)| version >= lo && version <= hi)
        .unwrap_or(false)
}

/// Whether key+version uses flexible (tagged/compact) framing. Only
/// the implemented flexible apis matter today: ApiVersions v3+
/// (Metadata caps below its flexible v9), SyncGroup v4+ and Heartbeat
/// v4+ (canonical gates from the Kafka message schemas; JoinGroup
/// turns flexible at v6, LeaveGroup v4, FindCoordinator v3,
/// DescribeGroups v5 -- all above our caps). Note ApiVersions v3
/// still answers with the CLASSIC v0 response header (KIP-511), see
/// conn.rs.
pub fn api_flexible(key: i16, version: i16) -> bool {
    (key == API_KEY_API_VERSIONS && version >= 3)
        || (key == API_KEY_SYNC_GROUP && version >= 4)
        || (key == API_KEY_HEARTBEAT && version >= 4)
        || (key == API_KEY_OFFSET_FETCH && version >= 6)
}

/// Log/metric label for an api key (P1+ keys pre-named).
pub fn api_name(key: i16) -> &'static str {
    match key {
        API_KEY_PRODUCE => "Produce",
        API_KEY_FETCH => "Fetch",
        API_KEY_METADATA => "Metadata",
        API_KEY_OFFSET_COMMIT => "OffsetCommit",
        API_KEY_OFFSET_FETCH => "OffsetFetch",
        API_KEY_FIND_COORDINATOR => "FindCoordinator",
        API_KEY_JOIN_GROUP => "JoinGroup",
        API_KEY_HEARTBEAT => "Heartbeat",
        API_KEY_LEAVE_GROUP => "LeaveGroup",
        API_KEY_SYNC_GROUP => "SyncGroup",
        API_KEY_DESCRIBE_GROUPS => "DescribeGroups",
        API_KEY_API_VERSIONS => "ApiVersions",
        _ => "UnknownApi",
    }
}

// ---- listener -------------------------------------------------------------

/// Bind the kafka listener; same contract/error text as `resp::bind`.
pub fn bind(addr: &str) -> Result<TcpListener, String> {
    let std_listener =
        std::net::TcpListener::bind(addr).map_err(|e| format!("listen {addr} failed: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("listen {addr} failed: {e}"))?;
    TcpListener::from_std(std_listener).map_err(|e| format!("listen {addr} failed: {e}"))
}

/// Accept loop: one task per connection, 10ms backoff on accept errors
/// (same policy as `resp::serve`). One coordinator runtime per process
/// (P3): membership lives in it, the background sweep expires members,
/// and every connection task shares it by Arc.
pub async fn serve(listener: TcpListener, shared: Arc<state::Shared>) -> ! {
    let coord = Arc::new(coordinator::CoordRuntime::new());
    tokio::spawn(coordinator::session::run_sweep(Arc::clone(&coord)));
    loop {
        match listener.accept().await {
            Ok((sock, _peer)) => {
                let shared = shared.clone();
                let coord = Arc::clone(&coord);
                tokio::spawn(conn::handle_conn(sock, shared, coord));
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

    #[test]
    fn registry_ranges() {
        assert_eq!(api_range(API_KEY_API_VERSIONS), Some((0, 3)));
        assert_eq!(api_range(API_KEY_METADATA), Some((0, 8)));
        assert_eq!(api_range(API_KEY_PRODUCE), Some((0, 3)));
        assert_eq!(api_range(API_KEY_LIST_OFFSETS), Some((0, 1)));
        assert_eq!(api_range(API_KEY_FETCH), Some((0, 10)), "v11 is flexible");
        assert_eq!(api_range(API_KEY_OFFSET_COMMIT), Some((0, 2)));
        assert_eq!(api_range(API_KEY_OFFSET_FETCH), Some((0, 7)));
        assert_eq!(api_range(999), None);
        assert!(api_supported(API_KEY_METADATA, 8));
        assert!(!api_supported(API_KEY_METADATA, 9), "v9 is flexible/UUID");
        assert!(!api_supported(API_KEY_API_VERSIONS, 4));
        assert!(api_supported(API_KEY_PRODUCE, 3), "v3 unlocks MSGVER2");
        assert!(!api_supported(API_KEY_PRODUCE, 4));
        assert!(api_supported(API_KEY_LIST_OFFSETS, 1));
        assert!(!api_supported(API_KEY_LIST_OFFSETS, 2));
        assert!(api_flexible(API_KEY_API_VERSIONS, 3));
        assert!(!api_flexible(API_KEY_METADATA, 8));
        assert!(!api_flexible(API_KEY_PRODUCE, 2), "produce stays classic");
        assert!(!api_flexible(API_KEY_LIST_OFFSETS, 1));
        assert!(api_supported(API_KEY_FETCH, 0));
        assert!(api_supported(API_KEY_FETCH, 10), "v10 = kmsg-era max");
        assert!(!api_supported(API_KEY_FETCH, 11), "v11 is flexible");
        assert!(!api_flexible(API_KEY_FETCH, 10), "fetch stays classic");
        assert!(api_supported(API_KEY_OFFSET_COMMIT, 2));
        assert!(api_supported(API_KEY_OFFSET_FETCH, 7));
        assert!(!api_supported(API_KEY_OFFSET_FETCH, 8), "v8 is flexible");
        // P3 coordinator ladder: classic up to the caps; SyncGroup and
        // Heartbeat turn flexible at v3 (JoinGroup only at v5).
        assert_eq!(api_range(API_KEY_FIND_COORDINATOR), Some((0, 1)));
        assert_eq!(api_range(API_KEY_JOIN_GROUP), Some((0, 4)));
        assert_eq!(api_range(API_KEY_HEARTBEAT), Some((0, 4)));
        assert_eq!(api_range(API_KEY_LEAVE_GROUP), Some((0, 2)));
        assert_eq!(api_range(API_KEY_SYNC_GROUP), Some((0, 4)));
        assert_eq!(api_range(API_KEY_DESCRIBE_GROUPS), Some((0, 3)));
        assert!(!api_flexible(API_KEY_JOIN_GROUP, 4), "v6 is flexible");
        assert!(!api_flexible(API_KEY_SYNC_GROUP, 3), "v4 is flexible");
        assert!(api_flexible(API_KEY_SYNC_GROUP, 4));
        assert!(api_flexible(API_KEY_HEARTBEAT, 4));
        assert!(!api_flexible(API_KEY_HEARTBEAT, 2));
        assert!(!api_flexible(API_KEY_HEARTBEAT, 3), "v4 is flexible");
        assert!(!api_flexible(API_KEY_LEAVE_GROUP, 2));
        assert!(!api_flexible(API_KEY_DESCRIBE_GROUPS, 3));
        // Body order is sorted by key.
        let apis = implemented_apis();
        assert!(apis.windows(2).all(|w| w[0].0 < w[1].0));
    }
}
