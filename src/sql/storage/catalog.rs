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

    /// Remove a table's schema (tombstone: `""` marks dropped).
    pub async fn drop(self, table: &str) -> Result<(), String> {
        let entry = RaftLogEntryData {
            key: catalog_key(table),
            value: String::new(),
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
    let raw = state::raft_get(&shared.raft.read().unwrap(), &catalog_key(table));
    if raw.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("corrupt catalog entry for {table}: {e}"))
}

/// All live schemas, ordered by name.
pub fn list_tables(shared: &Shared) -> Vec<TableSchema> {
    let raft = shared.raft.read().unwrap();
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

/// Next free table id (max+1); callers hold the DDL serialization.
pub fn next_table_id(shared: &Arc<Shared>) -> u32 {
    list_tables(shared).iter().map(|s| s.id).max().unwrap_or(0) + 1
}

/// Next free index id within a table.
pub fn next_index_id(schema: &TableSchema) -> u32 {
    schema.indexes.iter().map(|i| i.id).max().unwrap_or(0) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_key_shape() {
        assert_eq!(catalog_key("users"), "sql_catalog/users");
    }
}
