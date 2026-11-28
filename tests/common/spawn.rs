//! Process-launch helpers of the e2e harness, split out of `mod.rs` (batch-14 WP4;
//! `mod.rs` sat at the 800-line cap): launch the REAL `rdb` binary against a conf.yaml
//! and apply the two respawn rules -- EADDRINUSE (fresh ports, unchanged) and the fatal
//! `/join` failure (`src/main.rs` exits 1 with no retry; a transient head timeout under
//! load is retried here instead). The two `spawn_node*` fns are re-exported from `mod.rs`.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{free_ports, write_config, ProcNode};

pub(super) fn spawn_child(
    config_path: &Path,
    bootstrap: bool,
    join_http: Option<&str>,
    stderr_path: &Path,
    append: bool,
) -> Child {
    let stderr = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(stderr_path)
            .expect("open stderr log")
    } else {
        std::fs::File::create(stderr_path).expect("create stderr log")
    };
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rdb"));
    cmd.arg("-config").arg(config_path);
    // Start from a clean env regardless of what the test runner carries.
    cmd.env_remove("RAFT_BOOTSTRAP")
        .env_remove("RAFT_JOIN_ADDR")
        .env_remove("RDB_BEACON")
        .env_remove("RDB_DEBUG_REPL")
        .env_remove("RUST_LOG");
    if bootstrap {
        cmd.env("RAFT_BOOTSTRAP", "true"); // exactly "true" enables it
    }
    if let Some(join) = join_http {
        cmd.env("RAFT_JOIN_ADDR", join);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    cmd.spawn().expect("spawn rdb binary")
}

/// Launch attempts per node: free-:0 port probes race with sibling tests
/// binding them first (the child needs ~0.5s to reach its binds), so a
/// node that dies instantly on EADDRINUSE is respawned on fresh ports.
pub(super) const SPAWN_ATTEMPTS: usize = 4;

/// `src/main.rs` prints this prefix on every `/join` failure and exits 1 with
/// no retry. Transient shape: the `/join` head timeout of a loaded leader
/// (`src/rcache/http.rs` HEAD_TIMEOUT / `join.rs` JOIN_TIMEOUT, 10s each; a
/// flake since 2026-09-17). Those production defaults stay (H4-3); retry here.
const JOIN_FAILED_TEXT: &str = "join raft cluster failed:";
/// Launch budget of the join-failure respawn path (own counter; the
/// EADDRINUSE path keeps `SPAWN_ATTEMPTS` launches, unchanged).
const JOIN_ATTEMPTS: usize = 3;
/// Backoff before a join-failure respawn (gives the loaded leader room).
const JOIN_BACKOFF: Duration = Duration::from_millis(300);
/// Deadline of the join-peer liveness probe.
const JOIN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Watch window past the 200ms probe: `src/main.rs` joins BEFORE binding anything,
/// so a timed-out join kills the child ~10s in; RESP accepting ends it early.
const JOIN_WINDOW: Duration = Duration::from_secs(20);
/// One RESP connect attempt during the window (also the poll interval).
const READY_PROBE: Duration = Duration::from_millis(100);

/// None while the child is alive and startup proceeds; Some(true) when it
/// died of a port collision (respawn on fresh ports); Some(false) for any
/// other early exit (intentional in some failover tests -- surface it).
pub(super) fn early_exit_kind(
    child: &mut std::process::Child,
    stderr_path: &std::path::Path,
) -> Option<bool> {
    std::thread::sleep(std::time::Duration::from_millis(200));
    if matches!(child.try_wait(), Ok(None)) {
        return None;
    }
    let tail = std::fs::read_to_string(stderr_path).unwrap_or_default();
    Some(tail.contains("Address already in use"))
}

/// True when the join peer's raft-http port still accepts connections.
/// Only a live peer makes a join failure plausibly transient: a dead target
/// (`failed_first_join_is_retried_on_restart` points a joiner at `127.0.0.1:1`
/// on purpose) is surfaced like any other early exit, never retried.
fn join_peer_alive(join_http: &str) -> bool {
    match join_http.parse::<SocketAddr>() {
        Ok(addr) => std::net::TcpStream::connect_timeout(&addr, JOIN_PROBE_TIMEOUT).is_ok(),
        Err(_) => false,
    }
}

/// The fatal `/join` stderr line when the launch died of it against a LIVE peer,
/// inside the 200ms probe or later before RESP came up; None while the child is up,
/// outlives `JOIN_WINDOW`, or died of anything else (the caller's readiness poll /
/// exit fast-fail then takes over exactly as before).
fn fatal_join(child: &mut Child, stderr: &Path, join: Option<&str>, resp: &str) -> Option<String> {
    let join = join?;
    let deadline = Instant::now() + JOIN_WINDOW;
    let addr = resp.parse::<SocketAddr>().ok();
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        if addr.is_some_and(|a| std::net::TcpStream::connect_timeout(&a, READY_PROBE).is_ok()) {
            return None; // up: the caller's readiness poll takes over
        }
        std::thread::sleep(READY_PROBE);
    }
    if !matches!(child.try_wait(), Ok(Some(_))) {
        return None;
    }
    let tail = std::fs::read_to_string(stderr).unwrap_or_default();
    let line = tail.lines().find(|l| l.contains(JOIN_FAILED_TEXT))?;
    join_peer_alive(join).then(|| line.to_owned())
}

/// Spawn node `id` below the per-test `dir`: node dir + conf.yaml + 4 fresh
/// ports, child launched per `bootstrap`/`join_http`.
pub fn spawn_node(dir: &Path, id: usize, bootstrap: bool, join_http: Option<&str>) -> ProcNode {
    spawn_node_yaml(dir, id, bootstrap, join_http, "")
}

/// [`spawn_node`] with extra top-level yaml lines appended to the node's conf.yaml
/// (e.g. a `lite:` section arming the idle redelivery sweep); ports, respawn retries
/// and the stderr log are identical. Respawn rules, both on fresh ports with a
/// rewritten conf.yaml: EADDRINUSE within the 200ms probe (`SPAWN_ATTEMPTS` launches);
/// a fatal `/join` against a LIVE peer, watched until RESP is up (`JOIN_ATTEMPTS`
/// launches, 300ms backoff, one stdout line per retry so flakes stay attributable).
/// Any other early exit returns the dead child as before.
pub fn spawn_node_yaml(
    dir: &Path,
    id: usize,
    bootstrap: bool,
    join_http: Option<&str>,
    extra_yaml: &str,
) -> ProcNode {
    let name = format!("node{id}");
    let node_dir = dir.join(&name);
    std::fs::create_dir_all(&node_dir).expect("create node dir");
    let mut addr_in_use = 0usize;
    let mut join_failed = 0usize;
    loop {
        let [resp, raft, http, monitor] = free_ports();
        let config_path = node_dir.join("conf.yaml");
        write_config(
            &config_path,
            &node_dir,
            &resp,
            &raft,
            &http,
            &monitor,
            "",
            extra_yaml,
        );
        let stderr_path = node_dir.join("stderr.log");
        let mut child = spawn_child(&config_path, bootstrap, join_http, &stderr_path, false);
        if matches!(early_exit_kind(&mut child, &stderr_path), Some(true)) {
            addr_in_use += 1;
            if addr_in_use < SPAWN_ATTEMPTS {
                continue;
            }
            panic!("rdb kept dying at startup; see stderr.log in {node_dir:?}");
        }
        // Fatal `/join` against a live peer (transient head timeout):
        // retry on fresh ports; a dead peer or any other exit is returned as-is.
        if let Some(line) = fatal_join(&mut child, &stderr_path, join_http, &resp) {
            join_failed += 1;
            if join_failed < JOIN_ATTEMPTS {
                println!(
                    "[harness] node {name} join failed (attempt {join_failed}/{JOIN_ATTEMPTS}), \
                     respawning on a new port: {line}"
                );
                std::thread::sleep(JOIN_BACKOFF);
                continue;
            }
            panic!(
                "rdb join kept failing against {} (attempt {JOIN_ATTEMPTS}/{JOIN_ATTEMPTS}): {line}; \
                 see stderr.log in {node_dir:?}",
                join_http.unwrap_or_default()
            );
        }
        return ProcNode {
            dir: node_dir,
            config_path,
            stderr_path,
            child,
            resp,
            raft,
            http,
            monitor,
            mysql: String::new(),
            sql_rpc: String::new(),
            es: String::new(),
        };
    }
}
