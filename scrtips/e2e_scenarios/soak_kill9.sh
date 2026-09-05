#!/usr/bin/env bash
# soak_kill9.sh - release-binary durability soak: kill -9 + same-store
# respawn between load windows (the SIGKILL lands after the healthy
# bench window completes -- only the serial acked-INCR writer is still
# in flight when it lands).
#
# Single bootstrap node (same topology as the kv_newcmds_proc_e2e kill -9
# contract: no CLUSTER INIT, all keys local). The 3-node env.sh yaml with
# backup_target_map auto-assigns slot bands, which would MOVED-route the
# soak keys; deliberately out of scope here -- HA failover has its own
# e2e. Flow:
#   1. healthy-window mixed load via rdb-bench (must exit 0 = zero
#      client errors) while a serial INCR writer tracks its last ACKed
#      value;
#   2. SIGKILL the node between windows, respawn it on the SAME
#      store/config
#      (no bootstrap env, persistent raft state);
#   3. prove durability: the counter equals the last ACKed value or at
#      most one more (the single in-flight INCR), the counter continues
#      monotonically, the monitor plane answered /metrics under load,
#      and a post-restart bench round stays error-free.
#
# Usage: RDB_SOAK_SECS=180 RDB_E2E_PORT_BASE=32900 bash soak_kill9.sh
# (not named scenario_*: deliberately outside run_all.sh's default set)
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=env.sh
source "$HERE/env.sh"

RDB_SOAK_SECS="${RDB_SOAK_SECS:-180}"      # total load window
RDB_SOAK_HALF=$((RDB_SOAK_SECS / 2))       # kill point
BENCH="$E2E_ROOT/target/release/rdb-bench"
[ -x "$BENCH" ] || { echo "FAIL: $BENCH missing (build bench first)"; exit 1; }

e2e_init soak_kill9
LAST_ACK="$E2E_WORKDIR/last_acked"
: >"$LAST_ACK"
# bench artifacts live OUTSIDE the workdir so failures survive teardown
BENCH_OUT_PREFIX="/tmp/rdb_soak_bench_$$"

# Scratch ports can race other sessions on a shared box (an EADDRINUSE
# kills the child at startup); retry on a fresh port band before giving
# up. One node spans base..base+6, so step the base by 7.
E2E_WAIT_TIMEOUT_SAVED=$E2E_WAIT_TIMEOUT
E2E_WAIT_TIMEOUT=10
up=0
for _try in 1 2 3; do
    e2e_gen_yaml 0 >/dev/null
    _e2e_spawn 0 bootstrap
    if _e2e_wait_ping 0; then
        up=1
        break
    fi
    echo "-- startup attempt $_try failed (port band $E2E_PORT_BASE); retrying"
    kill -9 "${E2E_PIDS[0]}" 2>/dev/null
    wait "${E2E_PIDS[0]}" 2>/dev/null
    E2E_PIDS[0]=""
    E2E_PORT_BASE=$((E2E_PORT_BASE + 7))
    sleep 1
done
E2E_WAIT_TIMEOUT=$E2E_WAIT_TIMEOUT_SAVED
NODE0="$E2E_HOST:$(node_resp 0)"
if [ "$up" != 1 ]; then
    _e2e_fail "node0 never answered PING on 3 port bands"
    e2e_finish soak_kill9
fi
echo "ok   - node0 up (bootstrap, all-local topology) on $NODE0 (band $E2E_PORT_BASE)"

# Serial acked-INCR writer: one command in flight, every ACKed value
# appended (line-buffered). Dies with the node's socket; its last line
# is the last value the server had committed AND confirmed.
python3 - "$E2E_HOST" "$(node_resp 0)" "$RDB_E2E_TOKEN" "$LAST_ACK" <<'PY' &
import socket, sys

host, port, token, path = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
out = open(path, "w", buffering=1)


def line(f):
    buf = b""
    while not buf.endswith(b"\r\n"):
        chunk = f.recv(4096)
        if not chunk:
            raise ConnectionError("node closed the socket")
        buf += chunk
    return buf


s = socket.create_connection((host, port), timeout=10)
s.sendall(b"*2\r\n$4\r\nAUTH\r\n$%d\r\n%s\r\n" % (len(token), token.encode()))
assert line(s).startswith(b"+OK"), "AUTH failed"
i = 0
while True:
    s.sendall(b"*2\r\n$4\r\nINCR\r\n$8\r\nsoak:ctr\r\n")
    r = line(s)
    assert r.startswith(b":"), r
    i += 1
    out.write("%d\n" % i)
PY
WRITER_PID=$!

bench_round () { # label secs -> 0 on zero-error run
    local label=$1 secs=$2
    local out="${BENCH_OUT_PREFIX}_${label}.out"
    echo "-- bench $label (${secs}s, mixed, 8x16) against $NODE0"
    if "$BENCH" --addr "$NODE0" --token "$RDB_E2E_TOKEN" --workload mixed \
        --clients 8 --pipeline 16 --duration "$secs" >"$out" 2>&1; then
        echo "ok   - bench $label: zero client errors ($out)"
        return 0
    fi
    _e2e_fail "bench $label reported errors (see $out)"
    return 1
}

# ---- 1. healthy window ----
kill -0 "${E2E_PIDS[0]}" 2>/dev/null || _e2e_fail "node0 died before load started"
if curl -sf "http://$E2E_HOST:$(node_monitor 0)/metrics" >/dev/null 2>&1; then
    echo "ok   - monitor /metrics answers before load"
else
    _e2e_fail "monitor /metrics unreachable before load"
fi
bench_round healthy "$RDB_SOAK_HALF"
curl -sf "http://$E2E_HOST:$(node_monitor 0)/metrics" >/dev/null 2>&1 \
    && echo "ok   - monitor /metrics answered under load" \
    || _e2e_fail "monitor /metrics unreachable under load"

# ---- 2. kill -9 after the healthy window, respawn on the same store ----
# Gates: the durability verdict below is only meaningful if the healthy
# window actually ran -- the writer must still be alive AND have acked a
# sane number of INCRs by now (a dead/starved writer would make
# "counter == last acked" vacuously true).
ACK_COUNT=$(wc -l <"$LAST_ACK")
if ! kill -0 "$WRITER_PID" 2>/dev/null; then
    _e2e_fail "writer died during the healthy window"
    e2e_finish soak_kill9
fi
if [ "$ACK_COUNT" -lt 10 ]; then
    _e2e_fail "writer only acked ${ACK_COUNT} INCRs in the healthy window"
    e2e_finish soak_kill9
fi
echo "ok   - healthy window confirmed (writer alive, ${ACK_COUNT} acked INCRs)"

kill -9 "${E2E_PIDS[0]}" 2>/dev/null
wait "${E2E_PIDS[0]}" 2>/dev/null
echo "ok   - SIGKILLed node0 (writer socket breaks here)"
sleep 2 # let the writer observe the reset and flush its last ack
kill -0 "$WRITER_PID" 2>/dev/null && kill "$WRITER_PID" 2>/dev/null
wait "$WRITER_PID" 2>/dev/null
LAST_ACKED="$(tail -n 1 "$LAST_ACK")"
[ -n "$LAST_ACKED" ] || { _e2e_fail "writer never acked an INCR"; e2e_finish soak_kill9; }

# Respawn like tests/common ProcNode::respawn: same config, no bootstrap
# env, no join (persistent raft state + unchanged address). Same port
# race guard as the initial spawn: retry a failed respawn on a fresh
# port band (stepping the base by 7), regenerating the yaml into the
# SAME workdir so the persistent store is preserved across retries.
E2E_WAIT_TIMEOUT_SAVED=$E2E_WAIT_TIMEOUT
E2E_WAIT_TIMEOUT=10
up=0
for _try in 1 2 3; do
    [ "$_try" -gt 1 ] && e2e_gen_yaml 0 >/dev/null
    RAFT_BOOTSTRAP= RAFT_JOIN_ADDR= \
        "$RDB_BIN" -config "$E2E_WORKDIR/conf_node0.yaml" \
        >>"$E2E_WORKDIR/node0.log" 2>&1 &
    E2E_PIDS[0]=$!
    if _e2e_wait_ping 0; then
        up=1
        break
    fi
    echo "-- respawn attempt $_try failed (port band $E2E_PORT_BASE); retrying"
    kill -9 "${E2E_PIDS[0]}" 2>/dev/null
    wait "${E2E_PIDS[0]}" 2>/dev/null
    E2E_PIDS[0]=""
    E2E_PORT_BASE=$((E2E_PORT_BASE + 7))
    sleep 1
done
E2E_WAIT_TIMEOUT=$E2E_WAIT_TIMEOUT_SAVED
if [ "$up" != 1 ]; then
    _e2e_fail "respawned node0 never answered PING on 3 port bands"
    e2e_finish soak_kill9
fi
echo "ok   - node0 respawned on the same store and answers PING (band $E2E_PORT_BASE)"
NODE0="$E2E_HOST:$(node_resp 0)" # port band may have shifted on retry

# ---- 3. durability + continuation ----
CTR=$(redis-cli -h "$E2E_HOST" -p "$(node_resp 0)" GET soak:ctr)
case "$CTR" in
"$LAST_ACKED" | "$((LAST_ACKED + 1))")
    echo "ok   - counter after kill -9: $CTR (last acked $LAST_ACKED)"
    ;;
"")
    _e2e_fail "GET soak:ctr empty after respawn"
    ;;
*)
    _e2e_fail "counter regression: got $CTR, last acked $LAST_ACKED"
    ;;
esac

NEXT=$(redis-cli -h "$E2E_HOST" -p "$(node_resp 0)" INCR soak:ctr)
assert_eq "counter continues monotonically" "$((CTR + 1))" "$NEXT"

# Plain-KV durability of the same store: bench client 0 SET its key in
# the healthy window; it must survive the SIGKILL. Read it BEFORE the
# post-restart bench round overwrites it.
BENCH0=$(redis-cli -h "$E2E_HOST" -p "$(node_resp 0)" GET bench_0)
[ -n "$BENCH0" ] \
    && echo "ok   - bench_0 survived the SIGKILL (plain KV durable)" \
    || _e2e_fail "GET bench_0 empty after respawn (plain KV lost?)"

bench_round post_restart "$RDB_SOAK_HALF"

e2e_finish soak_kill9
