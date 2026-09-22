#!/usr/bin/env bash
# scenario_kafka_bench.sh - e2e: the kafka front under the repo's own
# load generator (rdb-bench P5c workloads kafka-prod / kafka-fetch).
#
# scenario_kafka_sdk.sh proves the wire against a REAL client library
# (librdkafka); this scenario drives it with the bench's hand-rolled
# Produce v2 / Fetch v4 client at load -- the same frames a minimal
# producer/consumer pair sends, closed-loop:
#   1. rdb up (RESP bind + kafka_bind); the bench itself pre-creates
#      topic bench1/q0 over RESP (XADD seed) before each run
#   2. kafka-prod: 2 clients x 15s x 100 records/request (acks=1, one
#      fsync per request server-side)
#   3. kafka-fetch: 1 client x 30s tails partition 0 from offset 0
#      (500ms long-poll at the tail) and must catch up with the log;
#      fetch gets 2x produce duration because tailing measures ~84k
#      records/s vs ~72k/s on the reference box
# Assertions: both bench runs exit 0 (exit 1 means kafka error replies /
# base_offset regressions), produce ops > 0, and fetch reads >= 90% of
# the produced records (ops counts RECORDS on both workloads).
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_ROOT="$(cd "$HERE/../.." && pwd)"
RDB_BIN="${RDB_BIN:-$E2E_ROOT/target/release/rdb}"
BENCH_BIN="${RDB_BENCH_BIN:-$E2E_ROOT/target/release/rdb-bench}"
SCENARIO="kafka_bench"
WORK="$(mktemp -d /tmp/rdb-kafka-bench-XXXXXX)"
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; wait "$p" 2>/dev/null; done
  rm -rf "$WORK"
}
trap cleanup EXIT

FAILS=0
fail() {
  FAILS=$((FAILS + 1))
  echo "FAIL: $1" >&2
  echo "--- rdb log tail ---" >&2
  tail -n 10 "$WORK/out.log" >&2 2>/dev/null
  for f in "$WORK"/*.out; do
    [ -f "$f" ] || continue
    echo "--- $f ---" >&2
    cat "$f" >&2
  done
}

[ -x "$RDB_BIN" ] || { echo "FATAL: $RDB_BIN missing (cargo build --release)" >&2; exit 1; }
[ -x "$BENCH_BIN" ] || { echo "FATAL: $BENCH_BIN missing (cargo build --release -p rdb-bench)" >&2; exit 1; }

# ---- free ports + scratch single node (bootstrap mode) ----
PORTS="$(python3 - <<'PY'
import socket
ss = [socket.socket() for _ in range(5)]
for s in ss:
    s.bind(('127.0.0.1', 0))
print(' '.join(str(s.getsockname()[1]) for s in ss))
PY
)"
read -r RESP_PORT RAFT_TCP RAFT_HTTP MON_PORT KAFKA_PORT <<< "$PORTS"
TOKEN="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
mkdir -p "$WORK/data"
cat > "$WORK/conf.yaml" <<YAML
bind: "127.0.0.1:$RESP_PORT"
store_path: "$WORK/data"
raft_bind_address: "127.0.0.1:$RAFT_TCP"
raft_http_bind_address: "127.0.0.1:$RAFT_HTTP"
monitor_addr: "127.0.0.1:$MON_PORT"
raft_token: "$TOKEN"
kafka_bind: "127.0.0.1:$KAFKA_PORT"
YAML
RAFT_BOOTSTRAP=true "$RDB_BIN" -config "$WORK/conf.yaml" >"$WORK/out.log" 2>&1 &
RDB_PID=$!
PIDS+=("$RDB_PID")

for _ in $(seq 1 100); do
  python3 - "$RESP_PORT" <<'PY' 2>/dev/null && break
import socket, sys
socket.create_connection(('127.0.0.1', int(sys.argv[1])), 1).close()
PY
  sleep 0.2
done
kill -0 "$RDB_PID" 2>/dev/null || { echo "FATAL: rdb exited early"; tail -5 "$WORK/out.log"; exit 1; }
python3 - "$KAFKA_PORT" <<'PY' 2>/dev/null || { echo "FATAL: kafka front not accepting"; tail -5 "$WORK/out.log"; exit 1; }
import socket, sys
socket.create_connection(('127.0.0.1', int(sys.argv[1])), 1).close()
PY

# ---- bench round helper: run one workload, capture its stats block ----
bench_ops () { # out-file -> the `ops=` line's record count
  awk -F'ops=' '/^ops=/{split($2, a, " "); print a[1]; exit}' "$1"
}

echo "== kafka-prod: 2 clients x 15s x 100 records/request (acks=1) =="
"$BENCH_BIN" --addr "127.0.0.1:$RESP_PORT" --token "$TOKEN" \
  --host "127.0.0.1:$KAFKA_PORT" --workload kafka-prod \
  --clients 2 --duration 15 --batch 100 >"$WORK/prod.out" 2>"$WORK/prod.err"
RC=$?
if [ $RC -ne 0 ]; then
  fail "kafka-prod exited $RC (error replies or client failure)"
else
  echo "ok   - kafka-prod exit 0 (zero kafka error replies)"
fi
PROD_OPS="$(bench_ops "$WORK/prod.out")"
PROD_RATE="$(awk -F'ops/s=' '/^ops\/s=/{split($2, a, " "); print a[1]; exit}' "$WORK/prod.out")"
echo "      produce records=$PROD_OPS ops/s=$PROD_RATE"
if awk -v v="${PROD_OPS:-0}" 'BEGIN{exit !(v > 0)}'; then
  echo "ok   - produce throughput > 0 ($PROD_OPS records)"
else
  fail "kafka-prod produced nothing (ops=${PROD_OPS:-unparsed})"
fi

echo "== kafka-fetch: 1 client x 30s tailing bench1/0 from offset 0 =="
"$BENCH_BIN" --addr "127.0.0.1:$RESP_PORT" --token "$TOKEN" \
  --host "127.0.0.1:$KAFKA_PORT" --workload kafka-fetch \
  --clients 1 --duration 30 >"$WORK/fetch.out" 2>"$WORK/fetch.err"
RC=$?
if [ $RC -ne 0 ]; then
  fail "kafka-fetch exited $RC (error replies or client failure)"
else
  echo "ok   - kafka-fetch exit 0 (zero kafka error replies)"
fi
FETCH_OPS="$(bench_ops "$WORK/fetch.out")"
echo "      fetch records=$FETCH_OPS"
# ops counts records on both workloads: the fetcher must catch up with
# at least 90% of what produce appended (plus the XADD seed entries).
if [ -n "${FETCH_OPS:-}" ] && awk -v f="$FETCH_OPS" -v p="${PROD_OPS:-0}" \
     'BEGIN{exit !(f > 0 && f * 10 >= p * 9)}'; then
  echo "ok   - fetch read $FETCH_OPS >= 90% of produced ${PROD_OPS:-0}"
else
  fail "fetch read ${FETCH_OPS:-unparsed} records, expected >= 0.9 x ${PROD_OPS:-0}"
fi

if [ "$FAILS" -eq 0 ]; then
  echo "PASS $SCENARIO"
  exit 0
fi
echo "FAIL $SCENARIO ($FAILS assertion(s) failed)"
exit 1
