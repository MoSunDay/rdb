//! Shared runtime state: config, store, routing topology and the raft
//! control-plane view consumed by command handlers.
//!
//! P1 shipped an in-process single-node raft stub; P2 wires openraft in
//! behind the SAME `RaftState` shape, so the command/resp layers are
//! untouched:
//! - `live_kv` is the FSM map handle (Go `CM`): `raft_get` reads it live
//!   instead of the 3s-stale `kv` dump (only the stub path still uses the
//!   dump);
//! - `apply_tx` queues entries to a background `client_write` loop that
//!   enforces Go's 5s Apply timeout;
//! - a metrics-sync task feeds `is_leader`/`leader_addr`/`state_label`/
//!   `node_desc`/`stats` from openraft `RaftMetrics` via
//!   [`sync_from_metrics`].

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use openraft::metrics::RaftMetrics;
use openraft::ServerState;

use crate::conf;
use crate::ds;
use crate::monitor;
use crate::rcache::fsm::KvMap;
use crate::rcache::{Node, NodeId, RdbRaft};
use crate::rtypes;
use crate::store;
use crate::topology;

/// Serve mode of a RESP listener; mirrors Go `newDB(host, path, mode)`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Backup,
}

pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::Backup => "backup",
    }
}

/// Go `Apply(..., 5*time.Second)` — the same 5s at every Go call site.
pub const APPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// One queued apply (Go raft.Apply future): the entry plus the channel
/// the awaiting caller is woken on.
pub struct ApplyReq {
    pub entry: rtypes::RaftLogEntryData,
    pub reply: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// Pending-apply handle returned by [`raft_apply_start`]. Plain data
/// carrier: `None` = the stub path already applied synchronously, else
/// the oneshot the background apply loop answers on.
pub struct ApplyTicket {
    reply: Option<tokio::sync::oneshot::Receiver<Result<(), String>>>,
}

/// Snapshot of the control plane as seen by command handlers.
#[derive(Clone, Default)]
pub struct RaftState {
    pub is_leader: bool,
    /// Raft TCP address of the current leader; "" when unknown.
    pub leader_addr: String,
    /// State label for the raft_stats gauge (Go parses it from Raft.String()).
    pub state_label: String,
    /// `raft nodes` prefix; Go hashicorp Raft.String(): "<addr> [<State>]".
    pub node_desc: String,
    /// `raft stats` rows. Key names are engine-defined (P2 documents drift).
    pub stats: Vec<(String, String)>,
    /// FSM map dump, refreshed every 3s by the topology sync (topology
    /// reads go through it; direct gets prefer `live_kv`).
    pub kv: BTreeMap<String, String>,
    /// Applies counted; drives the stub commit_index stat.
    pub apply_count: u64,
    /// Live FSM map (Go `CM`); present once real raft runs.
    pub live_kv: Option<KvMap>,
    /// Queue to the background client_write loop; None in the stub.
    pub apply_tx: Option<tokio::sync::mpsc::UnboundedSender<ApplyReq>>,
}

/// Go cacheManager.Get: missing key -> "". Live reads go through the FSM
/// handle (Go reads CM directly); only the stub falls back to `kv`.
pub fn raft_get(state: &RaftState, key: &str) -> String {
    if let Some(live) = &state.live_kv {
        return live.read().unwrap().get(key).cloned().unwrap_or_default();
    }
    state.kv.get(key).cloned().unwrap_or_default()
}

/// Go stats[key]: missing key -> "".
pub fn raft_stats_get(state: &RaftState, key: &str) -> String {
    state
        .stats
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// Start applying one replicated entry. NEVER blocks: leader check, then
/// either the stub's direct write (unit tests) or queueing onto the
/// background client_write loop. Callers must DROP any `shared.raft`
/// guard taken for this call BEFORE awaiting [`raft_apply_await`] -- the
/// real path parks for up to Go's 5s Apply timeout and must not hold the
/// control-plane lock while doing so.
///
/// Error text mirrors hashicorp raft.ErrNotLeader, which Go surfaces as
/// `internal error err: not leader` for `raft set`.
pub fn raft_apply_start(
    state: &mut RaftState,
    entry: &rtypes::RaftLogEntryData,
) -> Result<ApplyTicket, String> {
    if !state.is_leader {
        return Err("not leader".to_string());
    }
    let Some(tx) = state.apply_tx.clone() else {
        state.kv.insert(entry.key.clone(), entry.value.clone());
        state.apply_count += 1;
        refresh_stub_stats(state);
        return Ok(ApplyTicket { reply: None });
    };
    let (reply, rx) = tokio::sync::oneshot::channel();
    if tx
        .send(ApplyReq {
            entry: entry.clone(),
            reply,
        })
        .is_err()
    {
        return Err("apply loop stopped".to_string());
    }
    Ok(ApplyTicket { reply: Some(rx) })
}

/// Await a started apply. The worker enforces the 5s client_write
/// deadline; this bound only guards against a dead worker. Lost sender
/// (dropped apply loop) reads the same as a timeout.
pub async fn raft_apply_await(ticket: ApplyTicket) -> Result<(), String> {
    let Some(rx) = ticket.reply else {
        return Ok(()); // stub path: already applied in raft_apply_start
    };
    match tokio::time::timeout(APPLY_TIMEOUT + Duration::from_secs(1), rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_sender_dropped)) => Err("apply timeout".to_string()),
        Err(_elapsed) => Err("apply timeout".to_string()),
    }
}

/// Background loop executing queued applies through `client_write` (Go:
/// every Apply future blocks until the entry commits). Spawned once at
/// startup; exits when every `apply_tx` sender is dropped.
pub fn spawn_apply_loop(
    raft: Arc<RdbRaft>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ApplyReq>,
) {
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let result = apply_entry(&raft, req.entry).await;
            let _ = req.reply.send(result);
        }
        eprintln!("[task-exit] apply_loop (channel closed)");
    });
}

/// One client write bounded by Go's 5s Apply timeout.
async fn apply_entry(raft: &RdbRaft, entry: rtypes::RaftLogEntryData) -> Result<(), String> {
    match tokio::time::timeout(APPLY_TIMEOUT, raft.client_write(entry)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(client_write_error(e)),
        Err(_) => Err(format!("apply timeout after {}s", APPLY_TIMEOUT.as_secs())),
    }
}

/// hashicorp error texts surface verbatim in the RESP layer, so map the
/// openraft equivalents onto them ("not leader"). Match the variant, not
/// the Display text: `ForwardToLeader` renders as "has to forward request
/// to: ...", which a string search would miss.
fn client_write_error(
    err: crate::rcache::typ::RaftError<crate::rcache::typ::ClientWriteError>,
) -> String {
    use crate::rcache::typ::{ClientWriteError, RaftError};
    match err {
        RaftError::APIError(ClientWriteError::ForwardToLeader(_)) => "not leader".to_string(),
        e => e.to_string(),
    }
}

/// Refresh the control-plane view from openraft metrics (Go: Raft.Leader/
/// State/Stats/GetConfiguration; node_desc mirrors Raft.String()).
pub fn sync_from_metrics(state: &mut RaftState, m: &RaftMetrics<NodeId, Node>, self_addr: &str) {
    let label = state_label(m.state);
    state.is_leader = matches!(m.state, ServerState::Leader);
    // Go Raft.Leader(): "" while the leader is unknown.
    state.leader_addr = m
        .current_leader
        .and_then(|id| m.membership_config.membership().get_node(&id))
        .cloned()
        .unwrap_or_default();
    state.state_label = label.to_string();
    // Go main.go's gauge parser extracts the text between '[' and the
    // final ']' of Raft.String(), so node_desc must end " [<State>]".
    state.node_desc = format!("{self_addr} [{label}]");
    state.stats = stats_rows(m, label);
}

/// hashicorp gauge labels are Leader/Follower/Candidate/Shutdown/Unknown;
/// openraft's Learner has no Go counterpart and maps to Follower.
fn state_label(s: ServerState) -> &'static str {
    match s {
        ServerState::Leader => "Leader",
        ServerState::Follower | ServerState::Learner => "Follower",
        ServerState::Candidate => "Candidate",
        ServerState::Shutdown => "Shutdown",
    }
}

/// Go hashicorp `raft.Stats()` rows in deterministic order. `term` +
/// `commit_index` are string-concatenated into the cluster epoch
/// elsewhere, so both stay plain decimal strings.
fn stats_rows(m: &RaftMetrics<NodeId, Node>, label: &str) -> Vec<(String, String)> {
    let membership = m.membership_config.membership();
    let voters: Vec<String> = m
        .membership_config
        .voter_ids()
        .filter_map(|id| membership.get_node(&id).cloned())
        .collect();
    let num_peers = m
        .membership_config
        .voter_ids()
        .filter(|id| *id != m.id)
        .count();
    let commit = m.last_applied.as_ref().map(|l| l.index).unwrap_or(0);
    let latest_configuration = format!(
        "[{}]",
        voters
            .iter()
            .map(|addr| format!("{{Suffrage:Voter ID:{addr} Address:{addr}}}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let config_index = m
        .membership_config
        .log_id()
        .as_ref()
        .map(|l| l.index)
        .unwrap_or(0);
    [
        ("state", label.to_string()),
        ("term", m.current_term.to_string()),
        ("commit_index", commit.to_string()),
        ("last_log_index", m.last_log_index.unwrap_or(0).to_string()),
        ("applied_index", commit.to_string()),
        ("num_peers", num_peers.to_string()),
        ("protocol_version", "3".to_string()),
        ("latest_configuration", latest_configuration),
        ("latest_configuration_index", config_index.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// Build the P1 single-node stub: immediately leader of itself. Kept for
/// `testutil` unit tests; main.rs uses real raft.
pub fn stub_raft(conf: &conf::Config) -> RaftState {
    let mut state = RaftState {
        is_leader: true,
        leader_addr: conf.raft_tcp_address.clone(),
        state_label: "Leader".to_string(),
        node_desc: format!("{} [Leader]", conf.raft_tcp_address),
        stats: Vec::new(),
        kv: BTreeMap::new(),
        apply_count: 0,
        live_kv: None,
        apply_tx: None,
    };
    refresh_stub_stats(&mut state);
    state
}

fn refresh_stub_stats(state: &mut RaftState) {
    let voter = format!("[{{Suffrage:Voter ID:{0} Address:{0}}}]", state.leader_addr);
    let commit = state.apply_count.to_string();
    state.stats = vec![
        ("state".to_string(), state.state_label.clone()),
        ("term".to_string(), "1".to_string()),
        ("commit_index".to_string(), commit.clone()),
        ("last_log_index".to_string(), commit.clone()),
        ("applied_index".to_string(), commit),
        ("num_peers".to_string(), "0".to_string()),
        ("protocol_version".to_string(), "3".to_string()),
        ("latest_configuration".to_string(), voter),
        (
            "latest_configuration_index".to_string(),
            state.apply_count.to_string(),
        ),
    ];
}

/// Everything a RESP listener needs. One instance per listener (normal +
/// optional backup); `topology`, `raft` and `monitor` are shared across both,
/// mirroring Go's single conf.Content.
pub struct Shared {
    pub conf: conf::Config,
    pub mode: Mode,
    pub store: Arc<store::Store>,
    pub topology: Arc<RwLock<topology::Topology>>,
    pub raft: Arc<RwLock<RaftState>>,
    pub monitor: Arc<monitor::Collector>,
    /// Per-key latches serializing typed read-modify-write sequences.
    pub latch: ds::latch::Latch,
    /// Blocking-command wait queue (BLPOP/... parking spots).
    pub wait_hub: ds::wait::WaitHub,
    /// Lite Mode runtime: offset cache, pick counters, stats.
    pub lite: std::sync::Arc<crate::lite::Runtime>,
}

#[cfg(test)]
pub mod testutil {
    use super::*;

    /// Shared state with a tempdir-backed store and the P1 raft stub.
    pub fn shared_with(conf: conf::Config) -> Shared {
        let dir = std::env::temp_dir().join(format!("rdb-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = store::data_path(dir.to_str().unwrap(), &conf.bind);
        let st = store::open(path.to_str().unwrap()).unwrap();
        Shared {
            mode: Mode::Normal,
            store: Arc::new(st),
            topology: Arc::new(RwLock::new(topology::empty())),
            raft: Arc::new(RwLock::new(stub_raft(&conf))),
            monitor: Arc::new(monitor::new_collector()),
            latch: ds::latch::Latch::new(),
            wait_hub: ds::wait::WaitHub::new(),
            lite: std::sync::Arc::new(crate::lite::new_runtime()),
            conf,
        }
    }

    pub fn test_config() -> conf::Config {
        conf::Config {
            bind: "127.0.0.1:32681".to_string(),
            store_path: "/tmp/".to_string(),
            raft_tcp_address: "127.0.0.1:22681".to_string(),
            raft_token: "test-token".to_string(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use rtypes::RaftLogEntryData;

    /// RaftState plus the stalled loop's receiver: alive (so the send
    /// succeeds) but never draining (so the reply never arrives).
    fn leader_with_stalled_apply_loop(
    ) -> (RaftState, tokio::sync::mpsc::UnboundedReceiver<ApplyReq>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            RaftState {
                apply_tx: Some(tx),
                ..stub_raft(&testutil::test_config())
            },
            rx,
        )
    }

    /// A dead apply loop surfaces as `apply timeout` without blocking a
    /// worker thread for the whole window: tokio's paused clock
    /// auto-advances through the timeout.
    #[tokio::test(start_paused = true)]
    async fn apply_await_reports_timeout_on_dead_loop() {
        let (mut state, _stalled_rx) = leader_with_stalled_apply_loop();
        let entry = RaftLogEntryData {
            key: "k".to_string(),
            value: "v".to_string(),
        };
        let ticket = raft_apply_start(&mut state, &entry).expect("queueing succeeds");
        assert_eq!(
            raft_apply_await(ticket).await.expect_err("must time out"),
            "apply timeout",
            "dropped receiver reads the same as a timeout"
        );
    }

    /// Non-leader start fails fast: no ticket, no wait.
    #[tokio::test(start_paused = true)]
    async fn apply_start_rejects_follower_immediately() {
        let (mut state, _stalled_rx) = leader_with_stalled_apply_loop();
        state.is_leader = false;
        let entry = RaftLogEntryData {
            key: "k".to_string(),
            value: "v".to_string(),
        };
        let err = match raft_apply_start(&mut state, &entry) {
            Ok(_) => panic!("follower start must fail"),
            Err(e) => e,
        };
        assert_eq!(err, "not leader");
    }

    /// While a ticket is pending (loop stalled), `shared.raft` stays
    /// readable: the guard covering `raft_apply_start` is dropped before
    /// the await, so a stalled apply can no longer freeze the control-
    /// plane view for up to 6s.
    #[tokio::test(start_paused = true)]
    async fn pending_apply_does_not_hold_raft_guard() {
        let (state, _stalled_rx) = leader_with_stalled_apply_loop();
        let raft: Arc<RwLock<RaftState>> = Arc::new(RwLock::new(state));
        let entry = RaftLogEntryData {
            key: "k".to_string(),
            value: "v".to_string(),
        };
        let ticket = {
            let mut guard = raft.write().unwrap();
            raft_apply_start(&mut guard, &entry).expect("queueing succeeds")
        }; // guard dropped HERE, before any waiting
        assert!(
            raft.try_read().is_ok(),
            "raft lock must be free while the apply is in flight"
        );
        let _ = raft_apply_await(ticket).await;
    }
}
