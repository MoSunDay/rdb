//! M3: `sql_nodes` node registry -- raft-replicated listener addresses
//! used to resolve the current leader's control-API (http) address for
//! timestamp block leases, and (M3 2PC) ANY node's SQL RPC port.
//!
//! Each node registers while it is the LEADER directly (raft writes
//! are leader-gated), keyed by its raft TCP address. Followers cannot
//! write raft themselves, so the M3 2PC registration loop ALSO forwards
//! their binds to the current leader over the HTTP control API
//! (`/sql/nodes`), which merges them through raft on their behalf --
//! without it a never-led follower's `sql_rpc` port would stay unknown
//! and cross-node SQL writes could not reach it. Stale entries are
//! never deleted on depart; they are simply re-registered by their
//! (live) owner whenever it leads again, and a dead owner's entry is
//! only ever read for a leader address that no longer wins an
//! election.

use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::Duration;

use crate::rtypes::RaftLogEntryData;
use crate::state::{self, RaftState};

use super::global::{ClusterTs, ClusterTsDeps};

/// FSM key holding the node registry (JSON: raft addr -> binds).
pub const SQL_NODES_KEY: &str = "sql_nodes";

/// Self-registration cadence (idempotent; skips no-op writes).
const REGISTER_INTERVAL: Duration = Duration::from_secs(3);

/// One node's listener addresses, replicated in the registry so peers
/// can resolve the leader's control-API address.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeBinds {
    pub resp: String,
    pub raft: String,
    pub http: String,
    pub mysql: String,
    /// M3 2PC listener (`sql_rpc_bind`); empty = disabled. `default`
    /// keeps registry entries persisted by pre-2PC builds readable.
    #[serde(default)]
    pub sql_rpc: String,
}

/// Registry entry of the node whose RESP listener is `resp_addr`
/// (routing addresses participants by RESP bind; the 2PC transport
/// needs the matching sql_rpc port). Missing when that node has not
/// registered yet.
pub fn binds_by_resp(raft: &RwLock<RaftState>, resp_addr: &str) -> Option<NodeBinds> {
    let r = raft.read().unwrap();
    parse_registry(&state::raft_get(&r, SQL_NODES_KEY))
        .into_values()
        .find(|b| b.resp == resp_addr)
}

/// Registry map (empty on absent/corrupt JSON).
pub fn parse_registry(raw: &str) -> BTreeMap<String, NodeBinds> {
    if raw.is_empty() {
        return BTreeMap::new();
    }
    serde_json::from_str(raw).unwrap_or_default()
}

/// Registry JSON with this node's binds merged in; `None` when the entry
/// is already current (callers skip the raft write).
pub fn merged_registry(current: &str, binds: &NodeBinds) -> Option<String> {
    let mut map = parse_registry(current);
    if map.get(&binds.raft) == Some(binds) {
        return None;
    }
    map.insert(binds.raft.clone(), binds.clone());
    serde_json::to_string(&map).ok()
}

/// Current leader's control-API address from the local registry view
/// (`RaftState.leader_addr` -> `sql_nodes[addr].http`).
pub fn leader_http_addr(raft: &RwLock<RaftState>) -> Option<String> {
    let r = raft.read().unwrap();
    let leader = r.leader_addr.clone();
    if leader.is_empty() {
        return None;
    }
    let http = parse_registry(&state::raft_get(&r, SQL_NODES_KEY))
        .get(&leader)?
        .http
        .clone();
    (!http.is_empty()).then_some(http)
}

/// One self-registration round: `false` = leader-gated off or the entry
/// is already current (no raft write).
pub async fn register_once(deps: &ClusterTsDeps) -> Result<bool, String> {
    if !deps.raft.read().unwrap().is_leader {
        return Ok(false);
    }
    register_foreign(deps, &deps.binds).await
}

/// Merge ANY node's binds into the registry through raft; the caller
/// has already checked this node is the leader. `false` = the entry
/// is already current (no raft write).
pub async fn register_foreign(deps: &ClusterTsDeps, binds: &NodeBinds) -> Result<bool, String> {
    let current = {
        let r = deps.raft.read().unwrap();
        if !r.is_leader {
            return Err("not leader".into());
        }
        state::raft_get(&r, SQL_NODES_KEY)
    };
    let Some(merged) = merged_registry(&current, binds) else {
        return Ok(false);
    };
    let entry = RaftLogEntryData {
        key: SQL_NODES_KEY.to_string(),
        value: merged,
    };
    let ticket = {
        let mut r = deps.raft.write().unwrap();
        state::raft_apply_start(&mut r, &entry)?
    };
    state::raft_apply_await(ticket).await?;
    Ok(true)
}

/// Self-registration loop (leader-gated retry, like the existing
/// backup-map init loop): runs only once the cluster topology is ready.
/// Leaders self-write; followers forward their binds to the current
/// leader over `/sql/nodes` so their sql_rpc port is discoverable for
/// 2PC routing (see module doc).
pub fn spawn_register(deps: ClusterTsDeps) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REGISTER_INTERVAL);
        ticker.tick().await; // Go sleeps before the first registration
        loop {
            ticker.tick().await;
            if !deps.topo.read().unwrap().cluster_ready {
                continue;
            }
            let is_leader = deps.raft.read().unwrap().is_leader;
            let out = if is_leader {
                register_once(&deps).await.map(|_| ())
            } else {
                // Followers cannot raft-write: forward the binds to the
                // leader so 2PC routing can resolve our sql_rpc port.
                forward_binds(&deps).await
            };
            if let Err(e) = out {
                eprintln!("sql nodes: registration round failed: {e} (retry)");
            }
        }
    });
}

/// HTTP handler for `/sql/nodes`: a node (usually a follower) asks the
/// leader to merge its binds into the raft registry. Mirrors
/// `/sql/ts` gating: token check, leader-only, plain-text body.
pub async fn route_register(
    deps: Option<&std::sync::Arc<ClusterTs>>,
    token: &str,
    params: &[(String, String)],
) -> (&'static str, String) {
    use crate::rcache::http::first_param;
    let Some(deps) = deps else {
        return ("404 Not Found", "404 page not found\n".to_string());
    };
    if first_param(params, "raft-token") != token {
        return ("401 Unauthorized", "unauthorized\n".to_string());
    }
    let binds = NodeBinds {
        resp: first_param(params, "resp").to_string(),
        raft: first_param(params, "raft").to_string(),
        http: first_param(params, "http").to_string(),
        mysql: first_param(params, "mysql").to_string(),
        sql_rpc: first_param(params, "sql_rpc").to_string(),
    };
    if binds.raft.is_empty() {
        return ("200 OK", "missing raft bind\n".to_string());
    }
    if !deps.deps().raft.read().unwrap().is_leader {
        return ("404 Not Found", "not leader\n".to_string());
    }
    // register_foreign is leader-gated and no-op-skips; a forwarded
    // follower entry lands through the same merged-registry write.
    match register_foreign(deps.deps(), &binds).await {
        Ok(_) => ("200 OK", "ok\n".to_string()),
        Err(e) => ("200 OK", format!("internal error: {e}\n")),
    }
}

/// One follower round: forward this node's binds to the leader's
/// `/sql/nodes` endpoint (no-op when we lead or nobody is known).
async fn forward_binds(deps: &ClusterTsDeps) -> Result<(), String> {
    let leader = match leader_http_addr(&deps.raft) {
        Some(h) => h,
        None => return Ok(()),
    };
    let b = &deps.binds;
    let url = format!(
        "http://{leader}/sql/nodes?resp={}&raft={}&http={}&mysql={}&sql_rpc={}&raft-token={}",
        crate::rcache::join::percent_encode(&b.resp),
        crate::rcache::join::percent_encode(&b.raft),
        crate::rcache::join::percent_encode(&b.http),
        crate::rcache::join::percent_encode(&b.mysql),
        crate::rcache::join::percent_encode(&b.sql_rpc),
        crate::rcache::join::percent_encode(&deps.token),
    );
    let (status, body) = crate::rcache::join::http_get_status(&url).await?;
    if status != 200 || body.trim() != "ok" {
        return Err(format!("registry forward rejected: {body}"));
    }
    Ok(())
}
