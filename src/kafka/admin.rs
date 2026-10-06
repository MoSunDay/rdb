//! Group-admin APIs (Batch 2): ListGroups (16) v0-v1 and DeleteGroups
//! (42) v0-v1, classic framing only (both APIs turn flexible/tagged at
//! v2+, above the caps).
//!
//! ListGroups answers the UNION of the two places a group can live:
//! the coordinator runtime (joined members, any stage) and the kind
//! 0x20 committed-offset ledger (a group that only ever OffsetCommits
//! exists as ledger rows alone -- no lite 0x0E record is ever created
//! on that path). A ledger-only group reports the stable placeholder
//! state "Empty": it owns committed offsets but holds no members, and
//! "Dead" is reserved for unknown groups (DescribeGroups' vocabulary).
//! protocol_type is "consumer" for ledger rows (the only group shape
//! this front's commit path produces; the ledger records no type).
//!
//! DeleteGroups is the wire-side exit for the XTRIM/XDEL ledger guard:
//! it runs the group through the SAME public lite teardown XGROUP
//! DESTROY uses (fold of the group's 0x20 rows + lite group record +
//! PEL window + offset/ordered caches), then evicts runtime state.
//! Existence = runtime entry OR ledger rows: a group with neither
//! answers GROUP_ID_NOT_FOUND(69) per protocol; the rest of the batch
//! is still processed.

use crate::kafka::coordinator::{self, CoordRuntime};
use crate::kafka::errors;
use crate::kafka::frame::{put_array_len, put_i16, put_i32, put_string, Reader};
use crate::kafka::ledger;
use crate::lite;
use crate::state::Shared;
use crate::tx::session::ConnState;

/// protocol_type reported for groups the coordinator has never seen
/// (ledger-only rows carry no type; the commit path is consumer-only).
pub const LEDGER_PROTOCOL_TYPE: &str = "consumer";
/// State reported for ledger-only groups (no members anywhere).
pub const LEDGER_STATE: &str = "Empty";

/// One row of a ListGroups answer.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupListing {
    pub id: String,
    pub protocol_type: String,
    pub state: String,
}

/// Pure union: coordinator rows win (their stage/protocol_type are
/// live), ledger-only ids are appended sorted. Deduplicated by id.
pub fn merge_groups(runtime: Vec<GroupListing>, ledger_ids: &[Vec<u8>]) -> Vec<GroupListing> {
    let mut out = runtime;
    for id in ledger_ids {
        let Some(id) = String::from_utf8(id.to_vec()).ok() else {
            continue; // non-utf8 group ids cannot be listed (string field)
        };
        if out.iter().any(|g| g.id == id) {
            continue;
        }
        out.push(GroupListing {
            id,
            protocol_type: LEDGER_PROTOCOL_TYPE.to_string(),
            state: LEDGER_STATE.to_string(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// ListGroups v0-v1. v1 request: [states_filter] (KIP-518); an empty
/// filter lists every state. Response v0: error + [groups(id, type)];
/// v1 leads with throttle and adds the per-group state field.
pub async fn handle_list_groups(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
    coord: &CoordRuntime,
) -> Result<Vec<u8>, String> {
    let bad = || "malformed listgroups request".to_string();
    let mut states_filter: Vec<String> = Vec::new();
    if version >= 1 {
        let n = body.array_len().ok_or_else(bad)?.unwrap_or(0);
        for _ in 0..n {
            states_filter.push(body.string().ok_or_else(bad)?);
        }
    }
    let runtime = coord
        .groups
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|(id, st)| GroupListing {
            id: id.clone(),
            protocol_type: st.protocol_type.clone(),
            state: st.stage.name().to_string(),
        })
        .collect();
    let ledger_ids = ledger::scan_groups(&shared.store)?;
    let groups: Vec<GroupListing> = merge_groups(runtime, &ledger_ids)
        .into_iter()
        .filter(|g| states_filter.is_empty() || states_filter.contains(&g.state))
        .collect();
    Ok(list_groups_body(version, errors::NONE, &groups))
}

/// Encode a ListGroups response body (v0-v1).
pub fn list_groups_body(version: i16, error: i16, groups: &[GroupListing]) -> Vec<u8> {
    let mut out = Vec::new();
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms (v1 leads with it)
    }
    put_i16(&mut out, error);
    put_array_len(&mut out, groups.len());
    for g in groups {
        put_string(&mut out, &g.id);
        put_string(&mut out, &g.protocol_type);
        if version >= 1 {
            put_string(&mut out, &g.state);
        }
    }
    out
}

/// DeleteGroups v0-v1: [group_id] in, throttle(v1) + [results(id,
/// error)] out, request order preserved. Per group: ledger fold via
/// the lite teardown, then runtime eviction; 69 when nothing existed.
pub async fn handle_delete_groups(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
    coord: &CoordRuntime,
) -> Result<Vec<u8>, String> {
    let bad = || "malformed deletegroups request".to_string();
    let n = body.array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut names = Vec::with_capacity(n);
    for _ in 0..n {
        names.push(body.string().ok_or_else(bad)?);
    }
    let mut results = Vec::with_capacity(names.len());
    for name in &names {
        results.push((name.clone(), delete_one(shared, coord, name).await));
    }
    Ok(delete_groups_body(version, &results))
}

/// Encode a DeleteGroups response body (v0-v1): results in request
/// order, throttle leading on v1.
pub fn delete_groups_body(version: i16, results: &[(String, i16)]) -> Vec<u8> {
    let mut out = Vec::new();
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    put_array_len(&mut out, results.len());
    for (id, code) in results {
        put_string(&mut out, id);
        put_i16(&mut out, *code);
    }
    out
}

/// Delete one group: existence check (runtime OR ledger rows), fold
/// every stream's rows through the lite XGROUP DESTROY path, evict
/// runtime state. `Err` never happens -- store failures surface as the
/// per-group UNKNOWN_SERVER_ERROR so a batch makes progress.
async fn delete_one(shared: &Shared, coord: &CoordRuntime, group: &str) -> i16 {
    let rows = match ledger::scan_group(&shared.store, group.as_bytes()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[kafka] deletegroups ledger scan failed on {group}: {e}");
            return errors::UNKNOWN_SERVER_ERROR;
        }
    };
    if rows.is_empty() && !runtime_has(coord, group) {
        return errors::GROUP_ID_NOT_FOUND;
    }
    // One teardown per DISTINCT stream the group committed to (rows
    // carry the full partition stream name). The lite path folds the
    // 0x20 window AND any lite group record/PEL rows under the same
    // latch, fsync and wakeups XGROUP DESTROY uses -- no divergent
    // delete logic here.
    let mut streams: Vec<Vec<u8>> = Vec::new();
    for row in &rows {
        if !streams.contains(&row.stream) {
            streams.push(row.stream.clone());
        }
    }
    streams.sort();
    for stream in &streams {
        if let Err(e) = lite_destroy(shared, stream, group.as_bytes()).await {
            eprintln!(
                "[kafka] deletegroups teardown failed on {} for {group}: {e}",
                String::from_utf8_lossy(stream)
            );
            return errors::UNKNOWN_SERVER_ERROR;
        }
    }
    coordinator::remove_group(coord, group);
    errors::NONE
}

/// Does the runtime hold an entry (any stage) for `group`?
fn runtime_has(coord: &CoordRuntime, group: &str) -> bool {
    coord
        .groups
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(group)
}

/// Run the public lite group teardown (`XGROUP DESTROY`) for one
/// (stream, group): the reply buffer tells success (":1"/":0") from a
/// rejection ("-ERR ..."), so the kafka side never re-implements the
/// fold. The command ctx is synthesized -- the kafka front owns no
/// connection state, and the lite handlers only read args/shared.
async fn lite_destroy(shared: &Shared, stream: &[u8], group: &[u8]) -> Result<(), String> {
    let mut conn = ConnState::default();
    let mut out = Vec::new();
    let mut ctx = crate::command::Ctx {
        shared,
        prefix_key: Vec::new(),
        args: vec![b"destroy".to_vec(), stream.to_vec(), group.to_vec()],
        out: &mut out,
        close_conn: false,
        conn: &mut conn,
        wrote: false,
    };
    lite::group::xgroup(&mut ctx).await;
    if out.starts_with(b":") {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&out).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(id: &str, ty: &str, state: &str) -> GroupListing {
        GroupListing {
            id: id.to_string(),
            protocol_type: ty.to_string(),
            state: state.to_string(),
        }
    }

    #[test]
    fn merge_dedups_runtime_first_and_sorts() {
        let runtime = vec![listing("b", "consumer", "Stable")];
        let merged = merge_groups(runtime, &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        assert_eq!(
            merged,
            vec![
                listing("a", LEDGER_PROTOCOL_TYPE, LEDGER_STATE),
                listing("b", "consumer", "Stable"),
                listing("c", LEDGER_PROTOCOL_TYPE, LEDGER_STATE),
            ]
        );
        // Non-utf8 ledger ids are dropped, not fatal.
        assert_eq!(merge_groups(vec![], &[vec![0xff, 0xfe]]).len(), 0);
    }

    #[test]
    fn list_groups_bodies_by_version() {
        let groups = vec![listing("g1", "consumer", "Empty")];
        let v0 = list_groups_body(0, 0, &groups);
        let mut r = Reader::new(&v0);
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.string().as_deref(), Some("g1"));
        assert_eq!(r.string().as_deref(), Some("consumer"));
        assert_eq!(r.remaining(), 0, "v0 carries no state field");
        let v1 = list_groups_body(1, 0, &groups);
        let mut r = Reader::new(&v1);
        assert_eq!(r.i32(), Some(0), "throttle first on v1");
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.string().as_deref(), Some("g1"));
        assert_eq!(r.string().as_deref(), Some("consumer"));
        assert_eq!(r.string().as_deref(), Some("Empty"));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn delete_groups_bodies_by_version() {
        let v0 = delete_groups_body(0, &[("g1".to_string(), 0)]);
        let mut r = Reader::new(&v0);
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.string().as_deref(), Some("g1"));
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.remaining(), 0, "v0 carries no throttle");
        let v1 = delete_groups_body(1, &[("g1".to_string(), 0)]);
        let mut r = Reader::new(&v1);
        assert_eq!(r.i32(), Some(0), "throttle first on v1");
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.string().as_deref(), Some("g1"));
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.remaining(), 0);
    }
}
