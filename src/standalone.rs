//! `rdb standalone`: single-instance quick-start -- no yaml, no env
//! vars, no manual `CLUSTER INIT`, restart-idempotent. One command
//! derives EVERY protocol front from the base port (offsets follow
//! `scrtips/e2e_scenarios/env.sh`: resp +0, +1 reserved for the disabled
//! backup plane, monitor +2, raft-http +3, raft-tcp +4, mysql +5,
//! sql-rpc +6, kafka +7, rocksmq +8, es +9, s3 +10) and persists ONE
//! random token reused by every authenticated plane (RESP `AUTH`, MySQL
//! password, Kafka SASL PLAIN, ES/S3/RocksMQ Bearer). The node force-
//! bootstraps a single-member raft and seeds the one-node topology +
//! `sql_nodes` registry once it leads; multi-instance mode (yaml +
//! `RAFT_BOOTSTRAP`/`RAFT_JOIN_ADDR`) is intact.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rdb::{conf, rtypes, sql, state};

/// Raft FSM key of the stable instance list (same literal as
/// `cluster init` writes and main's topology sync reads).
pub(crate) const TOPOLOGY_KEY: &str = "cluster_slots_stable_instances";
/// Token file below the data dir (mode 0600, stable across restarts).
pub(crate) const TOKEN_FILE: &str = "standalone_token";
/// Leader wait bound for the topology seed; exceeding it is fatal.
const LEADER_WAIT: Duration = Duration::from_secs(60);
/// Seed poll cadence while waiting for leadership.
const POLL: Duration = Duration::from_millis(500);

const USAGE: &str = "usage: rdb standalone [--host H] [--port P] [--dir PATH] [--token X]";

/// Parsed `standalone` CLI flags (plain data).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StandaloneOpts {
    pub host: String,
    pub port: u16,
    pub dir: PathBuf,
    /// Explicit `--token`; overrides and rewrites the persisted file.
    pub token: Option<String>,
}

/// What the standalone boot still owes after the config is built:
/// carried from `conf_from_args` to the post-refusal `persist_token`.
#[derive(Debug, Clone)]
pub(crate) struct Boot {
    pub opts: StandaloneOpts,
    pub token: TokenChoice,
}

/// `--flag v` and `--flag=v` forms; last one wins, unknown flags fail.
pub(crate) fn parse_args(args: &[String]) -> Result<StandaloneOpts, String> {
    let mut opts = StandaloneOpts {
        host: "127.0.0.1".to_string(),
        port: 6379,
        dir: PathBuf::from("./rdb-standalone"),
        token: None,
    };
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (name, inline) = match arg.strip_prefix("--").and_then(|s| s.split_once('=')) {
            Some((n, v)) => (n, Some(v)),
            None => (arg.trim_start_matches('-'), None),
        };
        let value = |i: &mut usize| -> Result<String, String> {
            if let Some(v) = inline {
                return Ok(v.to_string());
            }
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))
        };
        match name {
            "host" => opts.host = value(&mut i)?,
            "port" => {
                let p: u16 = value(&mut i)?
                    .parse()
                    .map_err(|_| format!("invalid --port (expected 1-65535)\n{USAGE}"))?;
                // +10 derivation must stay inside the u16 port range.
                if p > 65525 {
                    return Err(format!("--port {p} too high: +10 derivation would overflow\n{USAGE}"));
                }
                opts.port = p;
            }
            "dir" => opts.dir = PathBuf::from(value(&mut i)?),
            "token" => opts.token = Some(value(&mut i)?),
            _ => return Err(format!("unknown standalone flag: {arg}\n{USAGE}")),
        }
        i += 1;
    }
    Ok(opts)
}

/// Every listener address derived from the base port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DerivedBinds {
    pub resp: String,
    pub monitor: String,
    pub raft_http: String,
    pub raft_tcp: String,
    pub mysql: String,
    pub sql_rpc: String,
    pub kafka: String,
    pub rocksmq: String,
    pub es: String,
    pub s3: String,
}

pub(crate) fn derived_binds(host: &str, port: u16) -> DerivedBinds {
    let at = |off: u16| format!("{host}:{}", port + off);
    DerivedBinds {
        resp: at(0),
        monitor: at(2),
        raft_http: at(3),
        raft_tcp: at(4),
        mysql: at(5),
        sql_rpc: at(6),
        kafka: at(7),
        rocksmq: at(8),
        es: at(9),
        s3: at(10),
    }
}

/// Ports below 1024 need root/CAP_NET_BIND_SERVICE; surfaced as a
/// warning, not an error (root may legitimately want 80/443).
pub(crate) fn port_warning(port: u16) -> Option<&'static str> {
    (port < 1024).then_some(
        "--port below 1024: privileged ports usually require root or CAP_NET_BIND_SERVICE",
    )
}

/// Resolved auth credential plus what must still be written to disk.
/// `resolve_token` is a pure read (no dir create, no file write); the
/// one side effect -- persisting `{dir}/standalone_token` -- waits in
/// `persist_token` until the multi-node refusal check has passed.
/// No secret ever lives in source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenChoice {
    pub token: String,
    /// `Some(t)`: the token file must be (re)written with `t` (fresh
    /// dir or explicit `--token`); `None`: existing file is current.
    pub persist: Option<String>,
    /// Randomly generated this boot (log wording only).
    pub generated: bool,
}

pub(crate) fn resolve_token(dir: &Path, explicit: Option<&str>) -> Result<TokenChoice, String> {
    if let Some(t) = explicit {
        let t = t.trim();
        if t.is_empty() {
            return Err("--token must not be empty".to_string());
        }
        return Ok(TokenChoice { token: t.to_string(), persist: Some(t.to_string()), generated: false });
    }
    if let Ok(existing) = std::fs::read_to_string(dir.join(TOKEN_FILE)) {
        let t = existing.trim().to_string();
        if !t.is_empty() {
            return Ok(TokenChoice { token: t, persist: None, generated: false });
        }
    }
    let t = random_token()?;
    Ok(TokenChoice { token: t.clone(), persist: Some(t), generated: true })
}

/// The one write side effect, deliberately deferred past the refusal
/// check so a refused boot mutates nothing under `dir`.
pub(crate) fn persist_token(dir: &Path, choice: &TokenChoice) -> Result<(), String> {
    if let Some(t) = &choice.persist {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        let path = dir.join(TOKEN_FILE);
        write_token(&path, t)?;
        if choice.generated {
            eprintln!("[standalone] generated auth token (saved to {}): {t}", path.display());
        }
    }
    Ok(())
}

fn write_token(path: &Path, token: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(token.as_bytes()))
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// 32 hex chars from 16 bytes of `/dev/urandom`.
fn random_token() -> Result<String, String> {
    use std::io::Read;
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("open /dev/urandom: {e}"))?
        .read_exact(&mut buf)
        .map_err(|e| format!("read /dev/urandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Full runtime config: store under `dir`, every protocol front on its
/// derived port, the one credential wired into every auth plane.
pub(crate) fn build_conf(opts: &StandaloneOpts, token: &str) -> conf::Config {
    let b = derived_binds(&opts.host, opts.port);
    conf::Config {
        store_path: opts.dir.to_string_lossy().into_owned(),
        bind: b.resp,
        monitor_addr: b.monitor,
        raft_tcp_address: b.raft_tcp,
        http_address: b.raft_http,
        raft_token: token.to_string(),
        mysql_bind: b.mysql,
        mysql_user: "root".to_string(),
        mysql_password: token.to_string(),
        sql_rpc_bind: b.sql_rpc,
        kafka_bind: b.kafka,
        kafka_token: token.to_string(),
        rocksmq_bind: b.rocksmq,
        rocksmq_token: token.to_string(),
        es_bind: b.es,
        es_token: token.to_string(),
        s3_bind: b.s3,
        s3_token: token.to_string(),
        ..Default::default()
    }
}

/// One-line endpoint summary for the startup log.
pub(crate) fn endpoints(c: &conf::Config) -> String {
    format!(
        "endpoints: resp={} monitor={} raft-http={} raft-tcp={} mysql={} sql-rpc={} \
         kafka={} rocksmq={} es={} s3={}",
        c.bind, c.monitor_addr, c.http_address, c.raft_tcp_address, c.mysql_bind,
        c.sql_rpc_bind, c.kafka_bind, c.rocksmq_bind, c.es_bind, c.s3_bind
    )
}

/// `standalone` subcommand entry (called from main's `startup_conf`):
/// `Ok(None)` = not a standalone invocation, caller falls through to
/// the yaml path. Resolves (but does not persist) the token; the file
/// write happens in `persist_token` only after the refusal check.
pub(crate) fn conf_from_args(args: &[String]) -> Result<Option<(conf::Config, Boot)>, String> {
    if args.first().map(String::as_str) != Some("standalone") {
        return Ok(None);
    }
    if !conf::raft_join_addr().is_empty() {
        return Err(
            "standalone mode cannot join an existing cluster: unset RAFT_JOIN_ADDR".to_string(),
        );
    }
    let opts = parse_args(&args[1..])?;
    if let Some(w) = port_warning(opts.port) {
        eprintln!("[standalone] warning: {w}");
    }
    let token = resolve_token(&opts.dir, opts.token.as_deref())?;
    eprintln!("[standalone] host={} port={} dir={}", opts.host, opts.port, opts.dir.display());
    let conf = build_conf(&opts, &token.token);
    eprintln!("[standalone] {}", endpoints(&conf));
    Ok(Some((conf, Boot { opts, token })))
}

/// What the topology seed must do with the persisted value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TopologyPlan {
    /// Empty store: seed topology = own bind + the `sql_nodes` registry.
    Seed,
    /// Already exactly this node (restart): nothing to do.
    Done,
    /// Non-empty and different: this store belongs to a multi-node
    /// cluster and must not be silently downgraded.
    Refuse(String),
}

pub(crate) fn topology_plan(current: &str, own_bind: &str) -> TopologyPlan {
    let list: Vec<&str> = current
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if list.is_empty() {
        TopologyPlan::Seed
    } else if list.len() == 1 && list[0] == own_bind {
        TopologyPlan::Done
    } else {
        TopologyPlan::Refuse(current.to_string())
    }
}

/// The `sql_nodes` seed entry (logic mirrored from `cluster_init`):
/// merge this node's binds into the raft-replicated registry; `None`
/// when already current.
fn sql_nodes_seed(c: &conf::Config, current_registry: &str) -> Option<rtypes::RaftLogEntryData> {
    let binds = sql::tx::nodes::NodeBinds {
        resp: c.bind.clone(),
        raft: c.raft_tcp_address.clone(),
        http: c.http_address.clone(),
        mysql: c.mysql_bind.clone(),
        sql_rpc: c.sql_rpc_bind.clone(),
    };
    sql::tx::nodes::merged_registry(current_registry, &binds).map(|value| rtypes::RaftLogEntryData {
        key: sql::tx::nodes::SQL_NODES_KEY.to_string(),
        value,
    })
}

/// Pre-listener downgrade guard: the FSM map is readable the moment the
/// raft store opens, so a store that ever belonged to a multi-node
/// cluster is refused before ANY data-plane listener binds (only the
/// earlier-started monitor endpoint is ever up briefly). The seed task
/// re-checks, so this is an optimization of the failure point, not the
/// only guard.
pub(crate) fn refuse_multi_node_store(kv: &rdb::rcache::fsm::KvMap, own_bind: &str) {
    let current = kv.read().unwrap().get(TOPOLOGY_KEY).cloned().unwrap_or_default();
    if let TopologyPlan::Refuse(v) = topology_plan(&current, own_bind) {
        eprintln!(
            "[standalone] refusing to start: this store holds a multi-node \
             topology ({v}) that does not match single instance {own_bind}"
        );
        std::process::exit(1);
    }
}

/// Background task: refuse a multi-node store immediately (its own raft
/// log carries the full topology, visible pre-leadership), then poll
/// until this force-bootstrapped node leads and seed the one-node
/// topology + `sql_nodes`. After the seed, the existing 3s topology
/// sync flips `cluster_ready`. Any miss past [`LEADER_WAIT`] exits 1.
pub(crate) fn spawn_ensure_topology(raft_state: Arc<RwLock<state::RaftState>>, c: conf::Config) {
    tokio::spawn(async move {
        let read_key = |k: &str| state::raft_get(&raft_state.read().unwrap(), k);
        let deadline = tokio::time::Instant::now() + LEADER_WAIT;
        loop {
            match topology_plan(&read_key(TOPOLOGY_KEY), &c.bind) {
                TopologyPlan::Done => {
                    eprintln!("[standalone] topology already initialized, nothing to do");
                    return;
                }
                TopologyPlan::Refuse(v) => {
                    eprintln!(
                        "[standalone] refusing to start: this store holds a multi-node \
                         topology ({v}) that does not match single instance {}",
                        c.bind
                    );
                    std::process::exit(1);
                }
                TopologyPlan::Seed => {}
            }
            if raft_state.read().unwrap().is_leader {
                let entry = rtypes::RaftLogEntryData {
                    key: TOPOLOGY_KEY.to_string(),
                    value: c.bind.clone(),
                };
                let started = state::raft_apply_start(&mut raft_state.write().unwrap(), &entry);
                let applied = match started {
                    Ok(ticket) => state::raft_apply_await(ticket).await,
                    Err(e) => Err(e),
                };
                match applied {
                    Ok(()) => {
                        // Best-effort seed, exactly like `cluster init`:
                        // the 3s registration loop re-covers a failure.
                        if let Some(seed) = sql_nodes_seed(&c, &read_key(sql::tx::nodes::SQL_NODES_KEY)) {
                            let started =
                                state::raft_apply_start(&mut raft_state.write().unwrap(), &seed);
                            let seeded = match started {
                                Ok(ticket) => state::raft_apply_await(ticket).await,
                                Err(e) => Err(e),
                            };
                            if let Err(e) = seeded {
                                eprintln!("[standalone] sql_nodes seed failed: {e} (registration loop retries)");
                            }
                        }
                        eprintln!("[standalone] single-node topology initialized");
                        return;
                    }
                    Err(e) => {
                        eprintln!("[standalone] topology apply failed: {e}, retrying")
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                eprintln!(
                    "[standalone] this node did not become the raft leader within {}s",
                    LEADER_WAIT.as_secs()
                );
                std::process::exit(1);
            }
            tokio::time::sleep(POLL).await;
        }
    });
}
