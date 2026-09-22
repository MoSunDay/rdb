//! Kafka broker error codes (subset) + a name map for logs/metrics.
//!
//! Codes follow the Kafka protocol error registry; only the values this
//! front can emit (or will emit in P1-P3) are listed. NOTE: CORRUPT_
//! MESSAGE is 2 -- code 31 is CLUSTER_AUTHORIZATION_FAILED (a common
//! off-by-one against stale client tables).

/// The (only) success code.
pub const NONE: i16 = 0;
pub const OFFSET_OUT_OF_RANGE: i16 = 1;
pub const CORRUPT_MESSAGE: i16 = 2;
pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
pub const LEADER_NOT_AVAILABLE: i16 = 5;
pub const NOT_LEADER_OR_FOLLOWER: i16 = 6;
pub const REQUEST_TIMED_OUT: i16 = 7;
pub const NETWORK_EXCEPTION: i16 = 13;
pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
pub const NOT_COORDINATOR: i16 = 16;
pub const INVALID_TOPIC_EXCEPTION: i16 = 17;
pub const INVALID_REQUIRED_ACKS: i16 = 21;
pub const ILLEGAL_GENERATION: i16 = 22;
pub const INCONSISTENT_GROUP_PROTOCOL: i16 = 23;
pub const INVALID_GROUP_ID: i16 = 24;
pub const UNKNOWN_MEMBER_ID: i16 = 25;
/// KIP-394: an initial JoinGroup with an empty member id is answered
/// with this code (NOT UNKNOWN_MEMBER_ID -- librdkafka clears the id
/// on 25 and retries forever; only 79 makes it adopt the new id).
pub const MEMBER_ID_REQUIRED: i16 = 79;
pub const INVALID_SESSION_TIMEOUT: i16 = 26;
/// Registry anchor (the P2 table had this wrong): 27 is
/// REBALANCE_IN_PROGRESS -- 28 is INVALID_COMMIT_OFFSET_SIZE.
pub const REBALANCE_IN_PROGRESS: i16 = 27;
pub const INVALID_COMMIT_OFFSET_SIZE: i16 = 28;
pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
pub const CLUSTER_AUTHORIZATION_FAILED: i16 = 31;
pub const INVALID_TIMESTAMP: i16 = 32;
pub const UNSUPPORTED_VERSION: i16 = 35;
pub const INVALID_REQUEST: i16 = 42;
/// Registry anchor (a common off-by-one): 76, NOT 29 (that is
/// TOPIC_AUTHORIZATION_FAILED) -- KIP-110-era code list.
pub const UNSUPPORTED_COMPRESSION_TYPE: i16 = 76;
/// KIP-345 static membership fence.
pub const FENCED_INSTANCE_ID: i16 = 82;
pub const UNKNOWN_SERVER_ERROR: i16 = -1;

/// Human-readable name for an error code (log labels; unknown codes
/// render as "error <code>", never a panic).
pub fn error_name(code: i16) -> &'static str {
    match code {
        NONE => "NONE",
        OFFSET_OUT_OF_RANGE => "OFFSET_OUT_OF_RANGE",
        CORRUPT_MESSAGE => "CORRUPT_MESSAGE",
        UNKNOWN_TOPIC_OR_PARTITION => "UNKNOWN_TOPIC_OR_PARTITION",
        LEADER_NOT_AVAILABLE => "LEADER_NOT_AVAILABLE",
        NOT_LEADER_OR_FOLLOWER => "NOT_LEADER_OR_FOLLOWER",
        REQUEST_TIMED_OUT => "REQUEST_TIMED_OUT",
        NETWORK_EXCEPTION => "NETWORK_EXCEPTION",
        COORDINATOR_NOT_AVAILABLE => "COORDINATOR_NOT_AVAILABLE",
        NOT_COORDINATOR => "NOT_COORDINATOR",
        INVALID_TOPIC_EXCEPTION => "INVALID_TOPIC_EXCEPTION",
        INVALID_REQUIRED_ACKS => "INVALID_REQUIRED_ACKS",
        ILLEGAL_GENERATION => "ILLEGAL_GENERATION",
        INCONSISTENT_GROUP_PROTOCOL => "INCONSISTENT_GROUP_PROTOCOL",
        INVALID_GROUP_ID => "INVALID_GROUP_ID",
        UNKNOWN_MEMBER_ID => "UNKNOWN_MEMBER_ID",
        MEMBER_ID_REQUIRED => "MEMBER_ID_REQUIRED",
        INVALID_SESSION_TIMEOUT => "INVALID_SESSION_TIMEOUT",
        REBALANCE_IN_PROGRESS => "REBALANCE_IN_PROGRESS",
        INVALID_COMMIT_OFFSET_SIZE => "INVALID_COMMIT_OFFSET_SIZE",
        TOPIC_AUTHORIZATION_FAILED => "TOPIC_AUTHORIZATION_FAILED",
        CLUSTER_AUTHORIZATION_FAILED => "CLUSTER_AUTHORIZATION_FAILED",
        INVALID_TIMESTAMP => "INVALID_TIMESTAMP",
        UNSUPPORTED_VERSION => "UNSUPPORTED_VERSION",
        INVALID_REQUEST => "INVALID_REQUEST",
        UNSUPPORTED_COMPRESSION_TYPE => "UNSUPPORTED_COMPRESSION_TYPE",
        FENCED_INSTANCE_ID => "FENCED_INSTANCE_ID",
        UNKNOWN_SERVER_ERROR => "UNKNOWN_SERVER_ERROR",
        _ => "UNKNOWN_ERROR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cover_emitted_codes() {
        assert_eq!(error_name(NONE), "NONE");
        assert_eq!(error_name(UNSUPPORTED_VERSION), "UNSUPPORTED_VERSION");
        assert_eq!(error_name(UNKNOWN_SERVER_ERROR), "UNKNOWN_SERVER_ERROR");
        assert_eq!(error_name(999), "UNKNOWN_ERROR");
        // The registry anchor the plan text got wrong: 2 vs 31.
        assert_eq!(error_name(CORRUPT_MESSAGE), "CORRUPT_MESSAGE");
        assert_eq!(CORRUPT_MESSAGE, 2);
        assert_eq!(CLUSTER_AUTHORIZATION_FAILED, 31);
        // OffsetCommit v1+ fencing code (P2 ledger).
        assert_eq!(ILLEGAL_GENERATION, 22);
        // P3 coordinator anchors: the classic eager-rebalance quartet
        // is 25/27/22/82 (27, NOT 28 -- 28 is commit-size).
        assert_eq!(REBALANCE_IN_PROGRESS, 27);
        assert_eq!(INVALID_COMMIT_OFFSET_SIZE, 28);
        assert_eq!(UNKNOWN_MEMBER_ID, 25);
        assert_eq!(INVALID_GROUP_ID, 24);
        assert_eq!(INCONSISTENT_GROUP_PROTOCOL, 23);
        assert_eq!(INVALID_SESSION_TIMEOUT, 26);
        assert_eq!(FENCED_INSTANCE_ID, 82);
        assert_eq!(INVALID_REQUEST, 42);
    }
}
