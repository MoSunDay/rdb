//! Whole-keyspace record classification shared by the keyspace-wide
//! commands (DBSIZE/INFO counting, FLUSHDB wipe): what one physical
//! record is when scanning the WHOLE local keyspace (all slot prefixes
//! at once), versus the single-key view of `keys_scan`.

use crate::ds::codec;
use crate::ds::expire;

/// FLUSHDB chunk: physical keys collected synchronously per batch.
pub(crate) const FLUSH_PAGE: usize = 1024;

/// What one physical record is when scanning the WHOLE local keyspace
/// (all slot prefixes at once): `keys_scan::user_entry_of` widened from
/// "is this a whole user key" to "who does this record belong to".
pub(crate) enum RecordRole {
    /// Whole user key: bare legacy string or META root. Carries the
    /// envelope deadline (0 = none) so counters honor lazy expiry.
    Root { deadline: u64 },
    /// Family member of some user key: element records (hash fields,
    /// list nodes, ...) and 0xFD expire-index entries.
    Member,
    /// Control-plane / foreign layout: no `"<slot>/"` prefix (raft, SQL
    /// segment meta) or a typed kind outside every user family. Never
    /// counted by DBSIZE/INFO and never wiped by FLUSHDB.
    Foreign,
}

pub(crate) fn record_role(physical: &[u8], value: &[u8]) -> RecordRole {
    let Some(plen) = expire::slot_prefix_len(physical) else {
        return RecordRole::Foreign;
    };
    match codec::classify(&physical[plen..]) {
        codec::Classification::Raw => RecordRole::Root { deadline: 0 },
        codec::Classification::Typed(kind) => {
            if kind == codec::KIND_EXPIRE_INDEX {
                RecordRole::Member
            } else if codec::is_user_key_kind(kind) {
                let (deadline, _) = codec::decode_envelope(value);
                RecordRole::Root { deadline }
            } else if codec::family_of(kind).is_some() {
                RecordRole::Member
            } else {
                RecordRole::Foreign
            }
        }
    }
}
