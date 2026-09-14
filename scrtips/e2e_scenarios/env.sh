#!/usr/bin/env bash
# env.sh - shared harness for real-CLI e2e scenarios (rdb Rust impl).
#
# Sourced by scenario_*.sh. Provides: cluster bring-up on scratch ports,
# per-scenario tmpdir store paths, generated yaml (token from env/random,
# NEVER from config/), readiness waits, teardown, assertion helpers.
#
# Port layout (base RDB_E2E_PORT_BASE, default 32700), node idx 0..2 at
# base + idx*10:
#   +0 RESP bind        +1 backup RESP     +2 monitor (/metrics)
#   +3 raft http        +4 raft tcp        +5 mysql wire   +6 sql rpc
set -uo pipefail

E2E_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RDB_BIN="${RDB_BIN:-$E2E_ROOT/target/release/rdb}"
RDB_E2E_TOKEN="${RDB_E2E_TOKEN:-$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')}"
# rdb gates every pre-AUTH command behind AUTH <raft_token> (src/resp/conn.rs),
# so make every redis-cli call in this harness authenticate transparently.
export REDISCLI_AUTH="$RDB_E2E_TOKEN"
E2E_HOST="${RDB_E2E_HOST:-127.0.0.1}"
E2E_NODES=3
E2E_WAIT_TIMEOUT="${RDB_E2E_WAIT_TIMEOUT:-90}"

# ---- state (set by e2e_init; arrays are the only globals we mutate) ----
E2E_WORKDIR=""
E2E_PORT_BASE="${RDB_E2E_PORT_BASE:-32700}"
E2E_PIDS=()
E2E_FAILS=0

# ---- addressing helpers (pure) ----
node_resp      () { echo $((E2E_PORT_BASE + $1 * 10));      }
node_backup    () { echo $((E2E_PORT_BASE + $1 * 10 + 1));  }
node_monitor   () { echo $((E2E_PORT_BASE + $1 * 10 + 2));  }
node_raft_http () { echo $((E2E_PORT_BASE + $1 * 10 + 3));  }
node_raft_tcp  () { echo $((E2E_PORT_BASE + $1 * 10 + 4));  }
node_mysql     () { echo $((E2E_PORT_BASE + $1 * 10 + 5));  }
node_sql_rpc   () { echo $((E2E_PORT_BASE + $1 * 10 + 6));  }

# gen one node yaml; prints the file path. Token comes from RDB_E2E_TOKEN
# (env or freshly random) so no secret from config/ is ever copied.
e2e_gen_yaml () {
    local idx=$1
    local f="$E2E_WORKDIR/conf_node$idx.yaml"
    cat > "$f" <<YAML
bind: $E2E_HOST:$(node_resp "$idx")
store_path: $E2E_WORKDIR/store$idx/
backup_bind: $E2E_HOST:$(node_backup "$idx")
backup_store_path: $E2E_WORKDIR/backup_store$idx/
raft_http_bind_address: $E2E_HOST:$(node_raft_http "$idx")
raft_bind_address: $E2E_HOST:$(node_raft_tcp "$idx")
monitor_addr: $E2E_HOST:$(node_monitor "$idx")
raft_token: "$RDB_E2E_TOKEN"
mysql_bind: $E2E_HOST:$(node_mysql "$idx")
mysql_user: root
mysql_password: ""
sql_rpc_bind: $E2E_HOST:$(node_sql_rpc "$idx")
allow_ip_list:
  - $E2E_HOST
backup_target_map:
  $E2E_HOST:$(node_raft_tcp "$idx"):
    src: $E2E_HOST:$(node_resp "$idx")
    target: $E2E_HOST:$(node_backup "$idx")
tx:
  enabled: true
YAML
    echo "$f"
}

# internal: spawn one node; sets E2E_PIDS[idx] via caller.
_e2e_spawn () {
    local idx=$1 boot=$2
    local yaml="$E2E_WORKDIR/conf_node$idx.yaml"
    local log="$E2E_WORKDIR/node$idx.log"
    if [ "$boot" = "bootstrap" ]; then
        RAFT_BOOTSTRAP=true RAFT_JOIN_ADDR= \
            "$RDB_BIN" -config "$yaml" >"$log" 2>&1 &
    else
        RAFT_BOOTSTRAP= RAFT_JOIN_ADDR="$E2E_HOST:$(node_raft_http 0)" \
            "$RDB_BIN" -config "$yaml" >"$log" 2>&1 &
    fi
    E2E_PIDS[$idx]=$!
}

_e2e_wait_ping () { # idx -> 0 when RESP port answers PING
    local port; port=$(node_resp "$1")
    for _ in $(seq 1 "$E2E_WAIT_TIMEOUT"); do
        [ "$(redis-cli -h "$E2E_HOST" -p "$port" PING 2>/dev/null)" = "PONG" ] && return 0
        sleep 1
    done
    return 1
}

# W0.4: every node's stderr must carry the "LIFO slot disabled" startup
# banner -- a binary built without --cfg tokio_unstable prints the DANGER
# variant instead (see COMPAT.md "tokio LIFO slot freeze").
_e2e_assert_lifo () {
    local i log
    for i in $(seq 0 $((E2E_NODES - 1))); do
        log="$E2E_WORKDIR/node$i.log"
        if grep -q 'DANGER' "$log" 2>/dev/null; then
            echo "FATAL: node$i built WITHOUT tokio_unstable cfg (see $log)" >&2
            return 1
        fi
        if ! grep -q 'tokio LIFO slot: disabled' "$log" 2>/dev/null; then
            echo "FATAL: node$i missing 'tokio LIFO slot: disabled' banner (see $log)" >&2
            return 1
        fi
    done
}

_e2e_wait_leader () { # any monitor /metrics shows raft_stats{status="Leader"}
    for _ in $(seq 1 "$E2E_WAIT_TIMEOUT"); do
        local i
        for i in $(seq 0 $((E2E_NODES - 1))); do
            if curl -sf "http://$E2E_HOST:$(node_monitor "$i")/metrics" 2>/dev/null \
                | grep -q 'status="Leader"'; then
                return 0
            fi
        done
        sleep 1
    done
    return 1
}

# print the idx of the current raft leader (probe `raft nodes`), else fail.
e2e_find_leader () {
    local i
    for _ in $(seq 1 "$E2E_WAIT_TIMEOUT"); do
        for i in 0 1 2; do
            if redis-cli -h "$E2E_HOST" -p "$(node_resp "$i")" raft nodes 2>/dev/null \
                | grep -q '\[Leader\]'; then
                echo "$i"
                return 0
            fi
        done
        sleep 1
    done
    return 1
}

# 2PC participants dial peers via sql_rpc_bind, and every node must be
# in the raft-replicated `sql_nodes` registry (src/sql/tx/nodes.rs)
# before cross-node SQL works. Wait until both followers show up.
_e2e_wait_sql_registry () {
    local want1="$E2E_HOST:$(node_sql_rpc 1)" want2="$E2E_HOST:$(node_sql_rpc 2)"
    local body
    for _ in $(seq 1 "$E2E_WAIT_TIMEOUT"); do
        body=$(curl -sf "http://$E2E_HOST:$(node_raft_http 0)/get?key=sql_nodes&raft-token=$RDB_E2E_TOKEN" 2>/dev/null) || { sleep 1; continue; }
        case "$body" in *"$want1"*) ;; *) sleep 1; continue ;; esac
        case "$body" in *"$want2"*) ;; *) sleep 1; continue ;; esac
        return 0
    done
    return 1
}

# leader-only: register the 3 RESP addrs so slot bands are assigned
# (`cluster_slots_stable_instances` via raft, src/command/cluster.rs).
# Scenarios may safely re-INIT later: same value -> same bands.
_e2e_cluster_init () {
    local leader addrs="" i reply
    leader=$(e2e_find_leader) || return 1
    for i in 0 1 2; do
        addrs+="$E2E_HOST:$(node_resp "$i"),"
    done
    addrs=${addrs%,}
    for _ in $(seq 1 "$E2E_WAIT_TIMEOUT"); do
        reply=$(redis-cli -h "$E2E_HOST" -p "$(node_resp "$leader")" \
            CLUSTER INIT "$addrs" 2>/dev/null)
        [ "$reply" = "done" ] && return 0
        sleep 1
    done
    return 1
}

# bring up 3 nodes: bootstrap + 2 join, then wait for readiness.
e2e_start_cluster () {
    local i
    for i in 0 1 2; do
        [ -f "$RDB_BIN" ] || { echo "FATAL: $RDB_BIN missing (build first)" >&2; return 1; }
        e2e_gen_yaml "$i" >/dev/null
    done
    _e2e_spawn 0 bootstrap
    sleep 2
    _e2e_spawn 1 join
    sleep 1
    _e2e_spawn 2 join
    for i in 0 1 2; do
        _e2e_wait_ping "$i" || {
            echo "FATAL: node$i RESP not ready" >&2; return 1; }
    done
    _e2e_assert_lifo || return 1
    _e2e_wait_leader || { echo "FATAL: no leader elected" >&2; return 1; }
    _e2e_cluster_init || { echo "FATAL: CLUSTER INIT failed" >&2; return 1; }
    _e2e_wait_sql_registry || { echo "FATAL: sql_rpc registry incomplete" >&2; return 1; }
    sleep 1   # settle: band assignment replication + instance registration
    echo "cluster up: resp $(node_resp 0)/$(node_resp 1)/$(node_resp 2) mysql $(node_mysql 0)"
}

# kill one node (default SIGKILL); args: idx [signal]
e2e_kill_node () {
    local idx=$1 sig="${2:-KILL}"
    local pid=${E2E_PIDS[$idx]:-}
    [ -n "$pid" ] && kill -"$sig" "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
}

# restart node idx: kill -9 if still running, wait for the process AND
# its ports to go away, then start a fresh instance on the same yaml.
# Self-contained on purpose: a half-dead node answers PING and would
# otherwise fool both the wait and the next bind.
e2e_restart_node () {
    local idx=$1 port
    port=$(node_resp "$idx")
    local pid=${E2E_PIDS[$idx]:-}
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid" 2>/dev/null
    fi
    [ -n "$pid" ] && wait "$pid" 2>/dev/null
    for _ in $(seq 1 "$E2E_WAIT_TIMEOUT"); do
        (exec 3<>"/dev/tcp/$E2E_HOST/$port") 2>/dev/null || break
        exec 3>&- 3<&-
        sleep 1
    done
    RAFT_BOOTSTRAP= RAFT_JOIN_ADDR="$E2E_HOST:$(node_raft_http 0)" \
        "$RDB_BIN" -config "$E2E_WORKDIR/conf_node$idx.yaml" \
        >>"$E2E_WORKDIR/node$idx.log" 2>&1 &
    E2E_PIDS[$idx]=$!
    _e2e_wait_ping "$idx"
}

# stop everything and remove scratch dirs. Safe to call twice.
e2e_teardown () {
    local pid
    for pid in "${E2E_PIDS[@]:-}"; do
        [ -n "$pid" ] && kill -TERM "$pid" 2>/dev/null
    done
    for pid in "${E2E_PIDS[@]:-}"; do
        [ -n "$pid" ] && wait "$pid" 2>/dev/null
    done
    E2E_PIDS=()
    # run_all.sh documents RDB_E2E_KEEP_WORKDIR (the RDB_E2E_* namespace);
    # keep honoring the short historical name too.
    if [ "${RDB_E2E_KEEP_WORKDIR:-${E2E_KEEP_WORKDIR:-0}}" != "1" ] && [ -n "$E2E_WORKDIR" ]; then
        rm -rf "$E2E_WORKDIR"
    fi
}

# per-scenario init: scratch dir + exit trap. args: scenario name
e2e_init () {
    local name=$1
    E2E_WORKDIR="$(mktemp -d "/tmp/rdb_e2e_${name}_XXXXXX")"
    E2E_PIDS=()
    E2E_FAILS=0
    trap e2e_teardown EXIT
}

# ---- assertions: each failure dumps the tail of every node log ----
_e2e_dump_logs () {
    local f
    for f in "$E2E_WORKDIR"/node*.log; do
        [ -f "$f" ] || continue
        echo "--- tail $f ---" >&2
        tail -n 15 "$f" >&2
    done
}

_e2e_fail () {
    E2E_FAILS=$((E2E_FAILS + 1))
    echo "FAIL: $1" >&2
    _e2e_dump_logs
}

assert_eq () { # desc expected actual
    if [ "$2" = "$3" ]; then
        echo "ok   - $1"
    else
        _e2e_fail "$1: expected [$2] got [$3]"
    fi
}

assert_contains () { # desc needle haystack
    case "$3" in
    *"$2"*) echo "ok   - $1" ;;
    *) _e2e_fail "$1: [$2] not found in [$3]" ;;
    esac
}

assert_not_contains () { # desc needle haystack
    case "$3" in
    *"$2"*) _e2e_fail "$1: [$2] unexpectedly found in [$3]" ;;
    *) echo "ok   - $1" ;;
    esac
}

# end of scenario: report and exit non-zero on any failure.
e2e_finish () {
    if [ "$E2E_FAILS" -eq 0 ]; then
        echo "PASS $1"
        exit 0
    fi
    echo "FAIL $1 ($E2E_FAILS assertion(s) failed)"
    exit 1
}
