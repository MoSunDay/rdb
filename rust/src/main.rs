//! Process entry (Rust port of Go `cmd/rdb/main.go` + `server.NewRDB`):
//! monitor endpoint, rcache raft node, topology sync, RESP listener(s).

mod beacon;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rdb::rcache::fsm::KvMap;
use rdb::rcache::RdbRaft;
use rdb::{conf, ds, monitor, rcache, resp, sql, state, store, topology};

const TOPOLOGY_KEY: &str = "cluster_slots_stable_instances";

/// Go `flag.String("config", ...)`: `-config`/`--config` (or `=X` forms);
/// last one wins, every other argument is ignored.
fn config_path_arg(args: &[String]) -> String {
    let mut path = conf::DEFAULT_CONFIG_PATH.to_string();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "-config" || arg == "--config" {
            if i + 1 < args.len() {
                path = args[i + 1].clone();
                i += 1;
            }
        } else if let Some(v) = arg.strip_prefix("-config=") {
            path = v.to_string();
        } else if let Some(v) = arg.strip_prefix("--config=") {
            path = v.to_string();
        }
        i += 1;
    }
    path
}

/// Go `conf.init`: `ioutil.ReadFile` fails fast (`Fatalf`) when the config
/// file is unreadable, so a dangling `-config <path>` never reaches the
/// listener stage with default bind ports. Pure check: `None` = file exists.
fn config_missing_error(path: &str) -> Option<String> {
    if std::path::Path::new(path).is_file() {
        None
    } else {
        Some(format!("read config file {path}: no such file"))
    }
}

/// Open the store for one listener (Go `newDB`: `filepath.Join(path, bind)`).
fn open_store(store_path: &str, bind: &str) -> Result<store::Store, String> {
    let path = store::data_path(store_path, bind);
    let path_str = path
        .to_str()
        .ok_or_else(|| format!("non-UTF8 store path: {}", path.display()))?;
    store::open(path_str)
}

/// D6b: an empty `raft_token` must never reach the listener stage: the
/// control plane's token check passes when BOTH sides are empty, so a
/// tokenless config silently starts an unauthenticated cluster. Pure
/// check: `None` = token present.
fn empty_raft_token_error(conf: &conf::Config, config_path: &str) -> Option<String> {
    if conf.raft_token.is_empty() {
        Some(format!(
            "raft_token is empty in {config_path}: an empty token disables control-plane auth; set a raft_token"
        ))
    } else {
        None
    }
}

/// Go `opts.DataDir = StorePath + "/" + Bind + "/raft"`.
fn raft_data_dir(conf: &conf::Config) -> String {
    format!("{}/{}/raft", conf.store_path, conf.bind)
}

/// Go 3s ticker: refresh the `RaftState.kv` dump from the live FSM map
/// and resync the routing tables from the raft FSM.
fn spawn_topology_sync(
    raft: Arc<RwLock<state::RaftState>>,
    topo: Arc<RwLock<topology::Topology>>,
    kv: KvMap,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(3));
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            {
                let mut st = raft.write().unwrap();
                st.kv = kv
                    .read()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
            }
            let val = state::raft_get(&raft.read().unwrap(), TOPOLOGY_KEY);
            *topo.write().unwrap() = topology::refresh(&val);
        }
        // Unreachable today (loop never breaks); proves exit if it ever does.
        #[allow(unreachable_code)]
        {
            eprintln!("[task-exit] topology_sync");
        }
    });
}

/// Mirror openraft metrics into RaftState (polling at most every 500ms);
/// the raft_stats gauge reads the refreshed label.
fn spawn_metrics_sync(
    raft_state: Arc<RwLock<state::RaftState>>,
    raft: Arc<RdbRaft>,
    self_addr: String,
) {
    let debug_repl = std::env::var("RDB_DEBUG_REPL").is_ok();
    tokio::spawn(async move {
        let mut rx = raft.metrics();
        loop {
            let m = rx.borrow_and_update().clone();
            state::sync_from_metrics(&mut raft_state.write().unwrap(), &m, &self_addr);
            // Debug-only dump (RDB_DEBUG_REPL=1): per-peer replication progress.
            if debug_repl {
                eprintln!(
                    "[repl-debug] state={:?} term={} last_log={:?} applied={:?} leader={:?} repl={:?}",
                    m.state, m.current_term, m.last_log_index, m.last_applied, m.current_leader, m.replication
                );
            }
            let changed = tokio::time::timeout(Duration::from_millis(500), rx.changed()).await;
            // Raft shut down; metrics frozen.
            if matches!(changed, Ok(Err(_))) {
                eprintln!("[task-exit] metrics_sync (metrics channel closed)");
                break;
            }
        }
        eprintln!("[task-exit] metrics_sync (loop end)");
    });
}

/// Go 5s ticker: export the raft state as the `raft_stats` gauge.
fn spawn_raft_stats(raft: Arc<RwLock<state::RaftState>>, collector: Arc<monitor::Collector>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            let label = raft.read().unwrap().state_label.clone();
            monitor::refresh_state_gauge(&collector, &label);
        }
        // Unreachable today (loop never breaks); proves exit if it ever does.
        #[allow(unreachable_code)]
        {
            eprintln!("[task-exit] raft_stats");
        }
    });
}

/// E1: bounded one-shot flush of the Lite group-offset watermarks. Runs
/// exactly one round of the 200ms background loop (see
/// `lite::flush_offsets_once`), so the committed watermark is durably on
/// disk before the process exits. Bounded: a hung flush gives up after 5s
/// and the caller still exits 0.
async fn shutdown_flush_lite_offsets(shared: &Arc<state::Shared>) {
    const BOUND: Duration = Duration::from_secs(5);
    match tokio::time::timeout(BOUND, rdb::lite::flush_offsets_once(shared)).await {
        Ok(Ok(())) => eprintln!("[shutdown] lite offset flush done"),
        Ok(Err(e)) => eprintln!("[shutdown] lite offset flush failed: {e}"),
        Err(_) => eprintln!("[shutdown] lite offset flush timed out after 5s, giving up"),
    }
}

/// E1: SIGTERM/SIGINT watcher. On the FIRST signal: log one line, flush
/// the Lite offsets (bounded), exit 0. Stopping accepts: there is no
/// accept-loop cancellation handle (`resp::serve` owns the listener inside
/// its own task and never returns), so the accept loops -- and every other
/// spawned task -- die with the process; the OS closes the sockets on
/// exit. Only the normal listener's Lite runtime is flushed (the backup
/// listener is read-only and holds no dirty offsets).
fn spawn_signal_shutdown(shared: Arc<state::Shared>) {
    tokio::spawn(async move {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[shutdown] cannot watch SIGTERM: {e}");
                    return;
                }
            };
        let mut interrupt =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[shutdown] cannot watch SIGINT: {e}");
                    return;
                }
            };
        tokio::select! {
            _ = term.recv() => eprintln!("received SIGTERM, shutting down gracefully"),
            _ = interrupt.recv() => eprintln!("received SIGINT, shutting down gracefully"),
        }
        shutdown_flush_lite_offsets(&shared).await;
        std::process::exit(0);
    });
}

fn main() {
    // RDB_CURRENT_THREAD=1: diagnostics-only single-threaded runtime.
    let mut builder = if std::env::var("RDB_CURRENT_THREAD").is_ok() {
        tokio::runtime::Builder::new_current_thread()
    } else {
        tokio::runtime::Builder::new_multi_thread()
    };
    builder.enable_all();
    // CRITICAL: disable the multi-thread scheduler's LIFO slot. With it
    // enabled, wakeups for LIFO-slotted tasks are lost under load and the
    // whole runtime freezes (workers parked, runnable tasks queued) for
    // 6s+ at a time. Requires --cfg tokio_unstable (rust/.cargo/config).
    #[cfg(tokio_unstable)]
    builder.disable_lifo_slot();
    if let Ok(n) = std::env::var("RDB_WORKER_THREADS") {
        if let Ok(n) = n.parse::<usize>() {
            if n > 0 {
                builder.worker_threads(n);
            }
        }
    }
    let runtime = builder.build().expect("build tokio runtime");
    // D8: one-line startup status of the LIFO-slot hazard (see COMPAT.md
    // "tokio LIFO slot freeze").
    #[cfg(tokio_unstable)]
    eprintln!("tokio LIFO slot: disabled (tokio_unstable cfg set)");
    #[cfg(not(tokio_unstable))]
    eprintln!(
        "tokio LIFO slot: ENABLED (DANGER: rebuild with RUSTFLAGS=\"--cfg tokio_unstable\" -- see COMPAT.md)"
    );
    runtime.block_on(do_main());
}

async fn do_main() {
    // Surface panics on stderr (with a backtrace) instead of dying silently.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[PANIC] {info}");
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("[PANIC] backtrace:\n{bt}");
    }));

    // Debug-only tracing (RUST_LOG=openraft=trace etc.); silent unless set.
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .init();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = config_path_arg(&args);
    // Fail fast on a dangling -config path (Go's conf.init Fatalf).
    if let Some(e) = config_missing_error(&config_path) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    let conf = match conf::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    // D6b: refuse to run without control-plane auth, before ANY listener
    // (monitor included) is bound.
    if let Some(e) = empty_raft_token_error(&conf, &config_path) {
        eprintln!("{e}");
        std::process::exit(1);
    }

    println!("Start..");
    println!("Bind: {}", conf.bind);
    println!("Path: {}", conf.store_path);

    let collector = Arc::new(monitor::new_collector());
    let monitor_addr = conf.monitor_addr.clone();
    let serve_collector = collector.clone();
    tokio::spawn(async move {
        if let Err(e) = monitor::serve(&monitor_addr, serve_collector).await {
            eprintln!("monitor serve failed: {e}");
            std::process::exit(1);
        }
    });

    // --- rcache assembly (Go server.newRCache) ---
    let data_dir = raft_data_dir(&conf);

    let node = match rcache::new_raft_node(&data_dir, &conf.raft_tcp_address).await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("new raft node failed:{e}");
            std::process::exit(1);
        }
    };
    let raft = node.raft.clone();

    // D7b: join only when the node has NO persisted raft state. Go (and
    // the old port) tested data-dir existence, but new_raft_node creates
    // the dir (and its RocksDB CURRENT marker) BEFORE the join RPC: a
    // first join that failed left a created-but-empty dir, and the retry
    // then silently skipped joining. openraft's is_initialized() is the
    // real signal -- initialize() and a successful join both persist a
    // membership log entry (or a vote) to the log store, which a failed
    // join never leaves behind. A read error falls back to "not
    // initialized" so the join is retried rather than skipped.
    let join_addr = match raft.is_initialized().await {
        Ok(true) => String::new(),
        Ok(false) => conf::raft_join_addr(),
        Err(e) => {
            eprintln!("check raft initialization failed:{e}");
            conf::raft_join_addr()
        }
    };

    // Diagnostic heartbeat (silent task-death canary), opt-in: RDB_BEACON=1.
    if beacon::enabled() {
        beacon::spawn_beacon(raft.clone());
    }

    // Raft RPC listener (Go transport bind is part of NewRaftNode).
    let serve_addr = conf.raft_tcp_address.clone();
    let serve_raft = raft.clone();
    tokio::spawn(async move {
        if let Err(e) = rcache::service::serve(serve_addr, serve_raft).await {
            eprintln!("new raft node failed:{e}");
            std::process::exit(1);
        }
    });

    // HTTP control API (Go http.Serve(l, httpServer.Mux)).
    let http_listener = match tokio::net::TcpListener::bind(&conf.http_address).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("listen {} failed: {}", conf.http_address, e);
            std::process::exit(1);
        }
    };
    let http_raft = raft.clone();
    let http_kv = node.kv.clone();
    let http_token = conf.raft_token.clone();
    let http_mux = rcache::http::membership_mux();
    tokio::spawn(async move {
        let _ =
            rcache::http::serve_on(http_listener, http_raft, http_kv, http_token, http_mux).await;
    });

    // Cluster join (Go JoinRaftCluster; any non-"ok" reply is fatal).
    if !join_addr.is_empty() {
        let joined =
            rcache::join::join_cluster(&join_addr, &conf.raft_tcp_address, &conf.raft_token).await;
        if let Err(e) = joined {
            eprintln!("join raft cluster failed:{e}");
            std::process::exit(1);
        }
    }

    // Shared control-plane state: apply loop + live FSM reads + sync tasks.
    // D5: BOUNDED queue (capacity 1024): a stalled raft can no longer
    // buffer unbounded applies; overflow answers fast with
    // `internal error err: apply queue full` (see raft_apply_start).
    let (apply_tx, apply_rx) =
        tokio::sync::mpsc::channel::<state::ApplyReq>(state::APPLY_CHANNEL_CAPACITY);
    state::spawn_apply_loop(raft.clone(), apply_rx);

    let raft_state = Arc::new(RwLock::new(state::RaftState {
        is_leader: false,
        leader_addr: String::new(),
        state_label: "Follower".to_string(),
        node_desc: format!("{} [Follower]", conf.raft_tcp_address),
        stats: Vec::new(),
        kv: std::collections::BTreeMap::new(),
        apply_count: 0,
        live_kv: Some(node.kv.clone()),
        apply_tx: Some(apply_tx),
    }));

    let topo = Arc::new(RwLock::new(topology::empty()));

    // Go NewRDB performs one immediate topology read at startup.
    {
        let val = state::raft_get(&raft_state.read().unwrap(), TOPOLOGY_KEY);
        *topo.write().unwrap() = topology::refresh(&val);
    }
    spawn_topology_sync(raft_state.clone(), topo.clone(), node.kv.clone());
    spawn_metrics_sync(
        raft_state.clone(),
        raft.clone(),
        conf.raft_tcp_address.clone(),
    );
    spawn_raft_stats(raft_state.clone(), collector.clone());

    // HA observer/failover (Go starts these unconditionally; they self-gate).
    rcache::ha::spawn_backup_map_init(raft.clone(), node.kv.clone(), &conf);
    let ha_mux = rcache::ha::observer_mux();
    rcache::ha::spawn_leader_probe(
        raft.clone(),
        node.kv.clone(),
        topo.clone(),
        conf.raft_tcp_address.clone(),
        ha_mux,
    );

    // Optional read-only backup listener (Go `BackupServer`, mode "backup").
    if !conf.backup_bind.is_empty() {
        let mut backup_conf = conf.clone();
        backup_conf.bind = conf.backup_bind.clone();
        backup_conf.store_path = conf.backup_store_path.clone();
        let store = match open_store(&backup_conf.store_path, &backup_conf.bind) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                eprintln!("start backup store failed: {e}");
                std::process::exit(1);
            }
        };
        let shared = Arc::new(state::Shared {
            conf: backup_conf.clone(),
            mode: state::Mode::Backup,
            store,
            topology: topo.clone(),
            raft: raft_state.clone(),
            monitor: collector.clone(),
            latch: rdb::ds::latch::Latch::new(),
            wait_hub: rdb::ds::wait::WaitHub::new(),
            lite: std::sync::Arc::new(rdb::lite::new_runtime()),
            sql_ts: std::sync::Arc::new(rdb::sql::tx::Oracle::new()),
        });
        let listener = match resp::bind(&backup_conf.bind) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
        tokio::spawn(resp::serve(listener, shared));
    }

    // Normal listener (Go `Server.KV.ListenAndServe`, fatal on error).
    let store = match open_store(&conf.store_path, &conf.bind) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("start store failed: {e}");
            std::process::exit(1);
        }
    };
    let shared = Arc::new(state::Shared {
        conf: conf.clone(),
        mode: state::Mode::Normal,
        store,
        topology: topo,
        raft: raft_state,
        monitor: collector,
        latch: ds::latch::Latch::new(),
        wait_hub: ds::wait::WaitHub::new(),
        lite: std::sync::Arc::new(rdb::lite::new_runtime()),
        sql_ts: std::sync::Arc::new(rdb::sql::tx::Oracle::new()),
    });
    // Active expiration loop (data-plane background task; sees the normal
    // listener's store -- the backup listener is read-only by design).
    ds::expire::spawn_active_expire(Arc::clone(&shared));
    // Lite Mode: periodic group-offset flush + stream gauges.
    rdb::lite::spawn_background(Arc::clone(&shared));
    let listener = match resp::bind(&conf.bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    // M1: MySQL-protocol SQL frontend on the normal listener's engine
    // state (empty mysql_bind = disabled; user/password from the same
    // config, empty user means "root").
    if !conf.mysql_bind.is_empty() {
        let listener = match sql::front::bind(&conf.mysql_bind) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
        tokio::spawn(sql::front::serve(listener, Arc::clone(&shared)));
    }
    // E1: install signal handling AFTER every listener/task is up; the
    // watcher flushes the Lite offsets and exits 0 (see spawn_signal_shutdown).
    spawn_signal_shutdown(Arc::clone(&shared));
    resp::serve(listener, shared).await // -> !, never returns
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> String {
        config_path_arg(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn config_arg_forms() {
        assert_eq!(parse(&[]), conf::DEFAULT_CONFIG_PATH);
        assert_eq!(parse(&["-config", "a.yml"]), "a.yml");
        assert_eq!(parse(&["--config", "b.yml"]), "b.yml");
        assert_eq!(parse(&["-config=c.yml"]), "c.yml");
        assert_eq!(parse(&["--config=d.yml"]), "d.yml");
        // Last occurrence wins, like Go flag.
        assert_eq!(parse(&["-config", "a.yml", "--config=e.yml"]), "e.yml");
        // Unrelated args ignored; dangling -config keeps the default.
        assert_eq!(
            parse(&["-other", "x", "-config"]),
            conf::DEFAULT_CONFIG_PATH
        );
        assert_eq!(parse(&["stray"]), conf::DEFAULT_CONFIG_PATH);
    }

    #[test]
    fn missing_config_file_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yml");
        std::fs::write(&path, "bind: 127.0.0.1:32681\n").expect("write");
        assert_eq!(config_missing_error(path.to_str().unwrap()), None);
        // Dangling -config path: named like conf::load's read error.
        let missing = dir.path().join("nope.yml");
        let err = config_missing_error(missing.to_str().unwrap()).expect("err");
        assert!(err.contains("read config file"), "got: {err}");
        assert!(err.contains("no such file"), "got: {err}");
        // A directory is not a readable config file either.
        assert!(config_missing_error(dir.path().to_str().unwrap()).is_some());
    }

    /// D6b: a config without raft_token never reaches the listener stage.
    #[test]
    fn empty_raft_token_is_reported() {
        let tokenless = conf::Config {
            bind: "127.0.0.1:32681".to_string(),
            ..Default::default()
        };
        let err = empty_raft_token_error(&tokenless, "conf.yaml").expect("err");
        assert!(err.contains("raft_token is empty"), "got: {err}");
        assert!(err.contains("conf.yaml"), "names the config: {err}");
        // Any non-empty token passes.
        let with_token = conf::Config {
            raft_token: "some-token".to_string(),
            ..tokenless
        };
        assert_eq!(empty_raft_token_error(&with_token, "conf.yaml"), None);
    }

    #[test]
    fn data_dir_matches_go_join() {
        let conf = conf::Config {
            store_path: "/data".to_string(),
            bind: "127.0.0.1:32681".to_string(),
            ..Default::default()
        };
        assert_eq!(raft_data_dir(&conf), "/data/127.0.0.1:32681/raft");
    }
}
