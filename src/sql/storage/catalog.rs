//! Catalog: table schemas replicated through the raft control plane.
//!
//! DDL is linearizable: `CREATE/DROP TABLE|INDEX` runs only on the raft
//! leader (`raft_apply_start` + await commit); every node then reads the
//! schema from its FSM `live_kv`, which converges with the log. Physical
//! rows are NOT replicated by raft -- they live in each node's RocksDB
//! under their slot, so a dropped table may leave orphaned row bytes on
//! non-leader nodes; the catalog tombstone makes them unreachable and a
//! recreated table gets a fresh `table_id`, never aliasing the orphans.

use std::sync::Arc;

use crate::rtypes::RaftLogEntryData;
use crate::sql::storage::schema::TableSchema;
use crate::state::{self, RaftState, Shared};

/// FSM key prefix under which schemas are stored.
pub const CATALOG_PREFIX: &str = "sql_catalog/";

pub fn catalog_key(table: &str) -> String {
    format!("{CATALOG_PREFIX}{table}")
}

/// DDL mutex holder: serializes schema mutation + table-id allocation on
/// the leader (raft serializes applies, but two concurrent CREATEs could
/// both observe max_id and pick the same new id).
pub struct CatalogTxn<'a> {
    raft: &'a mut RaftState,
}

impl CatalogTxn<'_> {
    /// Persist a schema (upsert) through raft; awaits commit.
    pub async fn put(self, schema: &TableSchema) -> Result<(), String> {
        let value = serde_json::to_string(schema).map_err(|e| e.to_string())?;
        let entry = RaftLogEntryData {
            key: catalog_key(&schema.name),
            value,
        };
        let ticket = state::raft_apply_start(self.raft, &entry)?;
        state::raft_apply_await(ticket).await
    }

    /// Remove a table's schema. The tombstone value carries the dropped
    /// table's id (a bare decimal, never valid TableSchema JSON), so id
    /// allocation stays monotone across drop+recreate cycles even after
    /// restarts; readers treat unparseable values as absent.
    pub async fn drop(self, table: &str, id: u32) -> Result<(), String> {
        let entry = RaftLogEntryData {
            key: catalog_key(table),
            value: id.to_string(),
        };
        let ticket = state::raft_apply_start(self.raft, &entry)?;
        state::raft_apply_await(ticket).await
    }
}

/// Begin a catalog mutation; fails fast on non-leaders, mirroring the RESP
/// control-plane behavior ("not leader").
pub fn begin(raft: &mut RaftState) -> Result<CatalogTxn<'_>, String> {
    if !raft.is_leader {
        let hint = if raft.leader_addr.is_empty() {
            String::new()
        } else {
            format!(" (leader: {})", raft.leader_addr)
        };
        return Err(format!("DDL requires the raft leader{hint}"));
    }
    Ok(CatalogTxn { raft })
}

/// Read one table's schema from the FSM view (leader and followers alike).
pub fn lookup(shared: &Shared, table: &str) -> Result<Option<TableSchema>, String> {
    lookup_raft(&shared.raft, table)
}

/// Same as [`lookup`] but over a bare raft-state handle: the 2PC
/// participant code runs inside `spawn_blocking` with no `Shared`.
pub fn lookup_raft(
    raft: &std::sync::RwLock<RaftState>,
    table: &str,
) -> Result<Option<TableSchema>, String> {
    let raw = state::raft_get(&raft.read().unwrap(), &catalog_key(table));
    if raw.is_empty() {
        return Ok(None); // pre-upgrade `""` tombstone: absent
    }
    if let Ok(schema) = serde_json::from_str::<TableSchema>(&raw) {
        return Ok(Some(schema));
    }
    // A bare decimal is the dropped-table tombstone (its id) and reads
    // as absent. Anything else is a genuinely corrupt entry and errors:
    // the M5 columnar GC relies on that distinction ("unreadable =
    // unknown, keep its metas").
    if raw.parse::<u32>().is_ok() {
        return Ok(None);
    }
    Err(format!("corrupt catalog entry for {table}"))
}

/// All live schemas, ordered by name. `Shared`-shaped wrapper over
/// [`list_tables_raft`].
pub fn list_tables(shared: &Shared) -> Vec<TableSchema> {
    list_tables_raft(&shared.raft)
}

/// Raft-handle shaped schema listing for call sites that run with no
/// `Shared` (the columnar GC sweep parks on the blocking pool).
pub fn list_tables_raft(raft: &std::sync::RwLock<RaftState>) -> Vec<TableSchema> {
    let raft = raft.read().unwrap();
    let mut out = Vec::new();
    // live_kv is the FSM view on a real node; the leader-local `kv` map
    // is the stub/apply-time source and stands in when there is no FSM
    // handle yet (unit tests, fresh leader before the first sync).
    match &raft.live_kv {
        Some(kv) => {
            if let Ok(map) = kv.read() {
                collect_tables(map.iter().map(|(k, v)| (k.as_str(), v.as_str())), &mut out);
            }
        }
        None => collect_tables(
            raft.kv.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            &mut out,
        ),
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn collect_tables<'a, I>(entries: I, out: &mut Vec<TableSchema>)
where
    I: Iterator<Item = (&'a str, &'a str)>,
{
    for (k, v) in entries {
        if let Some(name) = k.strip_prefix(CATALOG_PREFIX) {
            if v.is_empty() || name.is_empty() {
                continue; // tombstone
            }
            if let Ok(s) = serde_json::from_str::<TableSchema>(v) {
                out.push(s);
            }
        }
    }
}

/// Next free table id: max of the live and dropped (tombstoned) ids
/// plus one, so ids stay monotone across drop+recreate cycles even
/// after restarts. Callers hold the DDL serialization.
pub fn next_table_id(shared: &Arc<Shared>) -> u32 {
    let live_max = list_tables(shared).iter().map(|s| s.id).max().unwrap_or(0);
    let dropped_max = dropped_ids(shared).into_iter().max().unwrap_or(0);
    live_max.max(dropped_max) + 1
}

/// Next free index id within a table.
pub fn next_index_id(schema: &TableSchema) -> u32 {
    schema.indexes.iter().map(|i| i.id).max().unwrap_or(0) + 1
}

/// Ids of every dropped table still carrying a tombstone (value =
/// decimal id). Together with the live set they keep id allocation
/// monotone: an id is either live, tombstoned, or never issued.
pub fn dropped_ids_raft(raft: &std::sync::RwLock<RaftState>) -> Vec<u32> {
    let raft = raft.read().unwrap();
    let mut out = Vec::new();
    let entries: Vec<(String, String)> = match &raft.live_kv {
        Some(kv) => {
            if let Ok(map) = kv.read() {
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            } else {
                Vec::new()
            }
        }
        None => raft
            .kv
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };
    for (k, v) in entries {
        if !k.starts_with(CATALOG_PREFIX) || v.is_empty() {
            continue;
        }
        if serde_json::from_str::<TableSchema>(&v).is_ok() {
            continue; // live schema
        }
        if let Ok(id) = v.parse::<u32>() {
            out.push(id);
        }
    }
    out
}

/// `Shared`-shaped wrapper over [`dropped_ids_raft`].
pub fn dropped_ids(shared: &Shared) -> Vec<u32> {
    dropped_ids_raft(&shared.raft)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::storage::schema::{ColumnDef, Engine, SqlType};
    use crate::state::testutil;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn catalog_key_shape() {
        assert_eq!(catalog_key("users"), "sql_catalog/users");
    }

    fn schema_json(id: u32, name: &str) -> String {
        serde_json::to_string(&TableSchema {
            id,
            name: name.to_string(),
            columns: vec![ColumnDef {
                name: "id".to_string(),
                sql_type: SqlType::Int,
                nullable: false,
            }],
            pk: "id".to_string(),
            engine: Engine::Row,
            indexes: Vec::new(),
        })
        .unwrap()
    }

    fn shared_with_kv(entries: Vec<(String, String)>) -> Arc<Shared> {
        let shared = Arc::new(testutil::shared_with(testutil::test_config()));
        {
            let mut raft = shared.raft.write().unwrap();
            for (k, v) in entries {
                raft.kv.insert(k, v);
            }
        }
        shared
    }

    /// Id allocation is monotone over BOTH id sources on the stub kv
    /// view: live schemas and decimal drop tombstones. Empty values
    /// (pre-upgrade "" tombstones) and non-catalog keys never feed it.
    #[test]
    fn next_table_id_is_monotone_over_stub_kv_tombstones() {
        let shared = shared_with_kv(vec![
            (catalog_key("live"), schema_json(5, "live")),
            (catalog_key("gone"), "3".to_string()),
            (catalog_key("gone_hi"), "9".to_string()),
            (catalog_key("legacy"), String::new()),
            ("other/42".to_string(), "42".to_string()),
        ]);
        let mut dropped = dropped_ids(&shared);
        dropped.sort_unstable();
        assert_eq!(dropped, vec![3, 9]);
        assert_eq!(next_table_id(&shared), 10, "max(live=5, dropped=9) + 1");
        assert_eq!(next_table_id(&shared_with_kv(Vec::new())), 1);
    }

    /// Real nodes (and any restart) read the FSM `live_kv` map, not the
    /// stub kv: tombstones must stay visible to id allocation there.
    #[test]
    fn dropped_ids_reads_the_fsm_live_kv_view() {
        let map: crate::rcache::fsm::KvMap = Arc::new(std::sync::RwLock::new(HashMap::new()));
        {
            let mut m = map.write().unwrap();
            m.insert(catalog_key("t"), schema_json(2, "t"));
            m.insert(catalog_key("dropped"), "7".to_string());
            m.insert("unrelated".to_string(), "99".to_string());
        }
        let raft = std::sync::RwLock::new(RaftState {
            live_kv: Some(map),
            ..Default::default()
        });
        assert_eq!(dropped_ids_raft(&raft), vec![7]);
        assert_eq!(list_tables_raft(&raft).len(), 1, "live schema still listed");
    }
}
