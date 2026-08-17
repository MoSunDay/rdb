//! Shared wire types (Rust mirror of Go `internal/rtypes/types.go`).
//!
//! Only the JSON-serialized types live here; types tied to the connection
//! layer (`CommandContext`, `RDBServer`, ...) are defined by the modules
//! that own them, matching the rewrite's module layout.

/// Go `rtypes.RaftLogEntryData`. The Go struct has NO json tags, so
/// `encoding/json` emits the capitalized field names; the serde renames
/// below reproduce that byte-for-byte: `{"Key":"...","Value":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RaftLogEntryData {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "Value")]
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go equivalent:
    ///   bs, _ := json.Marshal(RaftLogEntryData{Key: "k", Value: "v"})
    ///   // bs == {"Key":"k","Value":"v"}
    #[test]
    fn serialize_matches_go_encoding_json() {
        let d = RaftLogEntryData {
            key: "k".to_string(),
            value: "v".to_string(),
        };
        let js = serde_json::to_string(&d).expect("serialize");
        assert_eq!(js, r#"{"Key":"k","Value":"v"}"#);
    }

    #[test]
    fn round_trip_byte_for_byte() {
        let raw = r#"{"Key":"k","Value":"v"}"#;
        let d: RaftLogEntryData = serde_json::from_str(raw).expect("parse");
        assert_eq!(d.key, "k");
        assert_eq!(d.value, "v");
        assert_eq!(serde_json::to_string(&d).expect("serialize"), raw);
    }

    /// Sample shaped exactly like Go `encoding/json` output (capitalized
    /// keys, escapes for embedded JSON in Value).
    #[test]
    fn parse_go_produced_sample() {
        let raw = r#"{"Key":"store/set","Value":"{\"slot\":12,\"ttl\":0}"}"#;
        let d: RaftLogEntryData = serde_json::from_str(raw).expect("parse");
        assert_eq!(d.key, "store/set");
        assert_eq!(d.value, r#"{"slot":12,"ttl":0}"#);
        assert_eq!(serde_json::to_string(&d).expect("reserialize"), raw);
    }

    #[test]
    fn lowercase_field_names_are_rejected_or_ignored() {
        // Go would NOT emit lowercase keys; make sure we don't silently
        // accept them as the canonical shape (they must not map to fields).
        let raw = r#"{"key":"k","value":"v"}"#;
        let res: Result<RaftLogEntryData, _> = serde_json::from_str(raw);
        assert!(res.is_err(), "lowercase keys must not deserialize");
    }
}
