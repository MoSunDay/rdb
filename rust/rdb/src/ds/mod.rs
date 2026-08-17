//! Data-structure foundation shared by every typed command family.
//!
//! Phase 0 ships the physical encoding only: `codec` builds the derived
//! keys every type writes under a slot prefix, `expire` gives uniform TTL
//! (envelope + active-expiration index/sampler), `latch` serializes
//! read-modify-write sequences per user key, `wait` parks blocking
//! commands (BLPOP/...) until a signal. Later phases implement the seven
//! structures on top of these primitives without touching the encoder
//! again.

pub mod codec;
pub mod expire;
pub mod hash_ds;
pub mod latch;
pub mod list_ds;
pub mod set_ds;
pub mod setops;
pub mod wait;
pub mod zset_ds;

pub use codec::{
    family_of, is_user_key_kind, meta_kinds, CodecFamily, HASH_FAMILY, JSON_FAMILY,
    KIND_EXPIRE_INDEX, KIND_HASH_FLD, KIND_HASH_META, KIND_JSON, KIND_LIST_L, KIND_LIST_META,
    KIND_LIST_R, KIND_SET_MEMBER, KIND_SET_META, KIND_STREAM_ENTRY, KIND_STREAM_GROUP,
    KIND_STREAM_META, KIND_STREAM_PEND, KIND_STRING, KIND_STRING_TTL, KIND_VECTORSET_ELEM,
    KIND_VECTORSET_META, KIND_ZSET_MEMBER, KIND_ZSET_META, KIND_ZSET_SCORE, LIST_FAMILY,
    SET_FAMILY, STREAM_FAMILY, STRING_FAMILY, VECTORSET_FAMILY, ZSET_FAMILY,
};

/// Redis `TYPE` reply for a kind byte. Raw/typed strings answer "string";
/// every kind of a family maps to the family's name; `EXPIRE_INDEX` and any
/// unknown byte answer "none" (the index is never a user-visible type).
pub fn type_name(kind: u8) -> &'static str {
    match kind {
        codec::KIND_STRING | codec::KIND_STRING_TTL => "string",
        codec::KIND_HASH_META | codec::KIND_HASH_FLD => "hash",
        codec::KIND_LIST_META | codec::KIND_LIST_L | codec::KIND_LIST_R => "list",
        codec::KIND_SET_META | codec::KIND_SET_MEMBER => "set",
        codec::KIND_ZSET_META | codec::KIND_ZSET_MEMBER | codec::KIND_ZSET_SCORE => "zset",
        codec::KIND_STREAM_META
        | codec::KIND_STREAM_ENTRY
        | codec::KIND_STREAM_GROUP
        | codec::KIND_STREAM_PEND => "stream",
        codec::KIND_JSON => "ReJSON-RL",
        codec::KIND_VECTORSET_META | codec::KIND_VECTORSET_ELEM => "vectorset",
        _ => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_name_covers_every_registered_kind() {
        let expect = [
            (codec::KIND_STRING, "string"),
            (codec::KIND_STRING_TTL, "string"),
            (codec::KIND_HASH_META, "hash"),
            (codec::KIND_HASH_FLD, "hash"),
            (codec::KIND_LIST_META, "list"),
            (codec::KIND_LIST_L, "list"),
            (codec::KIND_LIST_R, "list"),
            (codec::KIND_SET_META, "set"),
            (codec::KIND_SET_MEMBER, "set"),
            (codec::KIND_ZSET_META, "zset"),
            (codec::KIND_ZSET_MEMBER, "zset"),
            (codec::KIND_ZSET_SCORE, "zset"),
            (codec::KIND_STREAM_META, "stream"),
            (codec::KIND_STREAM_ENTRY, "stream"),
            (codec::KIND_STREAM_GROUP, "stream"),
            (codec::KIND_STREAM_PEND, "stream"),
            (codec::KIND_JSON, "ReJSON-RL"),
            (codec::KIND_VECTORSET_META, "vectorset"),
            (codec::KIND_VECTORSET_ELEM, "vectorset"),
            (codec::KIND_EXPIRE_INDEX, "none"),
            (0xEE, "none"),
        ];
        for (kind, name) in expect {
            assert_eq!(type_name(kind), name, "kind {kind:#04x}");
        }
    }
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod codec_tests;

#[cfg(test)]
#[path = "list_ds_tests.rs"]
mod list_ds_tests;

#[cfg(test)]
#[path = "zset_ds_tests.rs"]
mod zset_ds_tests;
