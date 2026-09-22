#!/usr/bin/env bash
# scenario_kafka_sdk.sh - e2e: the Kafka front exercised by a REAL Kafka
# client SDK (confluent-kafka / librdkafka), not hand-rolled frames.
#
# Why: raw-frame e2e (tests/kafka_*_e2e.rs) cannot prove version
# negotiation actually satisfies librdkafka -- it negotiates feature
# versions itself (Produce v3 unlocks MSGVER2 RecordBatch, Fetch v10,
# JoinGroup v4, OffsetFetch v7 flexible framing, ...). This scenario
# pins the whole stack against the real client library.
#
# Steps (single rdb node, RESP seeds the topic, then SDK-only):
#   1. metadata        AdminClient.list_topics sees 1 broker + the topic
#   2. produce         10 keyed messages, delivery callbacks, offsets 1..10
#   3. assign_consume  manual assign partition 0 from offset 0: 11 msgs
#   4. group_consume   subscribe() full group protocol: 11 msgs
#   5. commit_resume   commit offset 5, new consumer resumes at 5
#   6. rebalance       2nd consumer in the group forces revoke/assign
#   7. compression     gzip/snappy/lz4 produce (compressed RecordBatches)
#                      through the real SDK, then a consume roundtrip
#
# SKIP policy (exit 0): no confluent-kafka importable AND no network to
# pip-install it. The rdb binary itself must exist (same as other
# scenarios). Step 7 additionally self-skips when the broker answers
# UNSUPPORTED_COMPRESSION_TYPE(76) for every codec -- that is the
# DEFAULT build (the kafka-codecs cargo feature is opt-in); the step
# only FAILS on a feature build that rejects/round-trips wrongly.
#
# Semantics pinned by this scenario (empirically confirmed 2026-09):
# - librdkafka flexible request header = classic i16 client_id + tagged
#   tail (NOT compact client_id) -- matches src/kafka/frame.rs.
# - ApiVersions v3 response keeps the CLASSIC response header and puts
#   throttle_time_ms AFTER the api array (v0 back-compat).
# - Empty member_id JoinGroup must answer MEMBER_ID_REQUIRED(79)
#   (KIP-394); 25 makes librdkafka drop the id and retry forever.
# - FetchResponse order: throttle v1+, error_code v7+ BEFORE session_id,
#   preferred_read_replica only v11+.
# - librdkafka only compresses once a full batch forms, so step 7 forces
#   batching (batch.num.messages=10 + linger.ms=200, 30 msgs => >=1 full
#   compressed batch per codec). This SDK build ships zstd produce
#   UNCOMPRESSED (attributes=0 on the wire), so zstd cannot be probed
#   via the SDK; its 76 rejection is pinned by tests/kafka_codec_e2e.rs.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_ROOT="$(cd "$HERE/../.." && pwd)"
RDB_BIN="${RDB_BIN:-$E2E_ROOT/target/release/rdb}"
SCENARIO="kafka_sdk"
WORK="$(mktemp -d /tmp/rdb-kafka-sdk-XXXXXX)"
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; wait "$p" 2>/dev/null; done
  rm -rf "$WORK"
}
trap cleanup EXIT

[ -x "$RDB_BIN" ] || { echo "FATAL: $RDB_BIN missing (cargo build --release)" >&2; exit 1; }

# ---- pick the SDK interpreter (or SKIP) ----
SDK_PY="${KAFKA_SDK_PY:-}"
if [ -n "$SDK_PY" ] && ! "$SDK_PY" -c 'import confluent_kafka' 2>/dev/null; then
  echo "NOTE: KAFKA_SDK_PY=$SDK_PY cannot import confluent_kafka; falling back"
  SDK_PY=""
fi
if [ -z "$SDK_PY" ]; then
  for cand in /tmp/kfkvenv/bin/python python3; do
    if "$cand" -c 'import confluent_kafka' 2>/dev/null; then SDK_PY="$cand"; break; fi
  done
fi
if [ -z "$SDK_PY" ]; then
  echo "SKIP: confluent-kafka not importable; trying a quick pip install (60s cap)..."
  if timeout 60 python3 -m pip install --quiet --disable-pip-version-check \
        --no-input confluent-kafka 2>/dev/null && \
     python3 -c 'import confluent_kafka' 2>/dev/null; then
    SDK_PY=python3
  else
    echo "SKIP: no confluent-kafka SDK and no way to install one; scenario vacuous"
    exit 0
  fi
fi
echo "SDK: $SDK_PY ($("$SDK_PY" -c 'import confluent_kafka as k; print(k.version(), k.libversion())'))"

# ---- free ports + scratch node (bootstrap mode, like the e2e fixture) ----
PORTS="$(python3 - <<'PY'
import socket
ss = [socket.socket() for _ in range(5)]
for s in ss: s.bind(('127.0.0.1', 0))
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
RAFT_BOOTSTRAP=true "$RDB_BIN" -config "$WORK/conf.yaml" \
  >"$WORK/out.log" 2>&1 &
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

# ---- the 6-step SDK probe ----
KAFKA_BOOT="127.0.0.1:$KAFKA_PORT" RESP_ADDR="127.0.0.1:$RESP_PORT" \
RESP_TOKEN="$TOKEN" "$SDK_PY" - "$SCENARIO" <<'PY'
import os, sys, time, socket
BOOT = os.environ['KAFKA_BOOT']
RESP_HOST, RESP_PORT = os.environ['RESP_ADDR'].split(':')
TOKEN = os.environ['RESP_TOKEN']

# minimal RESP client: AUTH + XADD to seed the topic pre-SDK
def resp_cmd(sock, *args):
    out = b'*%d\r\n' % len(args)
    for a in args:
        a = a.encode() if isinstance(a, str) else a
        out += b'$%d\r\n%s\r\n' % (len(a), a)
    sock.sendall(out)
    t = sock.recv(1)
    def line():
        buf = b''
        while not buf.endswith(b'\r\n'):
            buf += sock.recv(1)
        return buf[:-2]
    if t in b'+-': return (t.decode(), line().decode())
    if t == b':': return int(line())
    if t == b'$':
        n = int(line())
        if n == -1: return None
        data = b''
        while len(data) < n + 2: data += sock.recv(n + 2 - len(data))
        return data[:-2]
    raise ValueError(t)

s = socket.create_connection((RESP_HOST, int(RESP_PORT)))
assert resp_cmd(s, 'AUTH', TOKEN) == ('+', 'OK'), 'AUTH gate'
assert resp_cmd(s, 'XADD', 'sdk1/q0', '*', 'hello') is not None, 'XADD seed'
s.close()

from confluent_kafka import Consumer, Producer, TopicPartition
from confluent_kafka.admin import AdminClient

results = []
def step(name, fn):
    try:
        detail = fn()
        if detail == 'SKIP':   # environment/feature not present: not a failure
            print(f'SKIP {name}')
            return
        results.append(True)
        print(f"OK   {name} {detail or ''}")
    except Exception as e:
        results.append(False)
        print(f"FAIL {name} :: {type(e).__name__}: {e}")

def s1_metadata():
    md = AdminClient({'bootstrap.servers': BOOT}).list_topics(timeout=10)
    topics = sorted(t for t in md.topics if not t.startswith('__'))
    brokers = sorted((b.host, b.port) for b in md.brokers.values())
    assert brokers == [('127.0.0.1', int(BOOT.split(':')[1]))], brokers
    assert topics == ['sdk1'], topics
    return f'brokers={brokers} topics={topics}'

def s2_produce():
    p = Producer({'bootstrap.servers': BOOT, 'socket.timeout.ms': 10000})
    delivered = []
    def dr(err, msg):
        delivered.append((err.code() if err else None,
                          msg.offset() if msg else -1))
    for i in range(10):
        p.produce('sdk1', f'k{i}'.encode(), f'v{i}'.encode(), on_delivery=dr)
    if p.flush(15): raise RuntimeError('messages not delivered')
    errs = [d for d in delivered if d[0] is not None]
    if errs: raise RuntimeError(f'delivery errors: {errs}')
    offs = sorted(d[1] for d in delivered)
    assert offs == list(range(1, 11)), offs
    return f'delivered=10 offsets=1..10'

def s3_assign_consume():
    c = Consumer({'bootstrap.servers': BOOT, 'group.id': 'probe-assign',
                  'enable.auto.commit': False, 'auto.offset.reset': 'earliest',
                  'socket.timeout.ms': 10000})
    c.assign([TopicPartition('sdk1', 0, 0)])
    got = []
    t0 = time.time()
    while len(got) < 11 and time.time() - t0 < 15:
        m = c.poll(0.5)
        if m is None: continue
        if m.error(): raise RuntimeError(f'consumer error {m.error()}')
        got.append(m.value())
    c.close()
    assert len(got) == 11, f'got {len(got)}/11: {got[:3]}'
    return f'first={got[0]} last={got[-1]}'

def s4_group_consume():
    c = Consumer({'bootstrap.servers': BOOT, 'group.id': 'probe-group',
                  'auto.offset.reset': 'earliest', 'enable.auto.commit': False,
                  'session.timeout.ms': 6000, 'socket.timeout.ms': 10000})
    c.subscribe(['sdk1'])
    got, t0 = [], time.time()
    while len(got) < 11 and time.time() - t0 < 25:
        m = c.poll(0.5)
        if m is None: continue
        if m.error(): raise RuntimeError(f'{m.error()}')
        got.append(m.value())
    c.close()
    assert len(got) == 11, f'got {len(got)}/11'
    return f'consumed {len(got)} via group protocol'

def s5_commit_resume():
    c = Consumer({'bootstrap.servers': BOOT, 'group.id': 'probe-commit',
                  'enable.auto.commit': False, 'auto.offset.reset': 'earliest',
                  'session.timeout.ms': 6000, 'socket.timeout.ms': 10000})
    c.subscribe(['sdk1'])
    seen, t0 = [], time.time()
    while len(seen) < 11 and time.time() - t0 < 25:
        m = c.poll(0.5)
        if m is None or m.error(): continue
        seen.append(m.offset())
        if len(seen) == 5:
            c.commit(m)  # commit offset of the 5th message -> next is 5
    c.close()
    c2 = Consumer({'bootstrap.servers': BOOT, 'group.id': 'probe-commit',
                   'auto.offset.reset': 'earliest', 'enable.auto.commit': False,
                   'session.timeout.ms': 6000, 'socket.timeout.ms': 10000})
    c2.subscribe(['sdk1'])
    first, n, t0 = None, 0, time.time()
    while n < 6 and time.time() - t0 < 25:
        m = c2.poll(0.5)
        if m is None or m.error(): continue
        if first is None: first = m.offset()
        n += 1
    c2.close()
    assert first == 5, f'resumed at {first}, expect 5'
    return f'committed@5 first_resumed={first} n={n}'

def s6_rebalance():
    cfg = {'bootstrap.servers': BOOT, 'group.id': 'probe-rb',
           'auto.offset.reset': 'earliest', 'enable.auto.commit': False,
           'session.timeout.ms': 6000, 'socket.timeout.ms': 10000}
    ev1, ev2 = [], []
    def sub(c, ev):
        c.subscribe(['sdk1'],
                    on_revoke=lambda _, ps: ev.append(('revoke', [p.partition for p in ps])),
                    on_assign=lambda _, ps: ev.append(('assign', [p.partition for p in ps])))
    c1 = Consumer(dict(cfg)); sub(c1, ev1)
    t0 = time.time()
    while not any(e[0] == 'assign' for e in ev1) and time.time() - t0 < 25:
        c1.poll(0.3)
    c2 = Consumer(dict(cfg)); sub(c2, ev2)
    t0 = time.time()
    while not (any(e[0] == 'assign' for e in ev2) and
               any(e[0] == 'revoke' for e in ev1)) and time.time() - t0 < 30:
        c1.poll(0.1); c2.poll(0.1)
    c1.close(); c2.close()
    assert ev1 and ev2, f'no rebalance events: {ev1} {ev2}'
    return f'c1={ev1} c2={ev2}'

def s7_compression():
    # Compressed produce through the real SDK. librdkafka only compresses
    # once a batch actually forms, so force batching: 30 msgs per codec
    # with batch.num.messages=10 + linger.ms=200 => full compressed
    # RecordBatches on the wire. A default (feature-less) build answers
    # 76 for every message -> self-SKIP the whole step.
    def produce_codec(codec, n=30):
        p = Producer({'bootstrap.servers': BOOT, 'socket.timeout.ms': 10000,
                      'compression.type': codec, 'batch.num.messages': 10,
                      'linger.ms': 200})
        drs = []
        def dr(err, msg):
            drs.append((err.code() if err else None,
                        msg.offset() if msg else -1))
        for i in range(n):
            # confluent-kafka arg order is produce(topic, VALUE, key=...):
            # pass value positionally + key by keyword so the wire really
            # carries key=ck-*/value=cv-* (a positional pair silently
            # swaps them and the roundtrip assert below would chase that).
            p.produce('sdk1', f'cv-{codec}-{i}'.encode(),
                      key=f'ck-{codec}-{i}'.encode(), on_delivery=dr)
        if p.flush(30): raise RuntimeError(f'{codec}: flush incomplete')
        return drs

    def consume_from(off, want):
        c = Consumer({'bootstrap.servers': BOOT, 'group.id': 'probe-codec',
                      'enable.auto.commit': False,
                      'socket.timeout.ms': 10000})
        c.assign([TopicPartition('sdk1', 0, off)])
        got, t0 = [], time.time()
        while len(got) < len(want) and time.time() - t0 < 20:
            m = c.poll(0.5)
            if m is None or m.error(): continue
            got.append((m.key(), m.value()))
        c.close()
        assert got == want, f'roundtrip {len(got)}/{len(want)} first={got[:1]}'
        return True

    parts, all76 = [], True
    for codec in ('gzip', 'snappy', 'lz4'):
        drs = produce_codec(codec)
        codes = {c for c, _ in drs}
        if codes == {76}:
            continue                      # feature-less build: broker said no
        all76 = False
        assert None in codes and len(codes) == 1, f'{codec} dr codes {codes}'
        offs = sorted(o for _, o in drs)
        assert offs == list(range(offs[0], offs[0] + len(offs))), \
            f'{codec} offsets not contiguous: {offs}'
        consume_from(offs[0], [(f'ck-{codec}-{i}'.encode(),
                                f'cv-{codec}-{i}'.encode()) for i in range(30)])
        parts.append(f'{codec}@{offs[0]}..{offs[-1]}')
    if all76:
        return 'SKIP'                     # default build: kafka-codecs off
    return 'produced+consumed ' + ' '.join(parts)

step('1.metadata', s1_metadata)
step('2.produce', s2_produce)
step('3.assign_consume', s3_assign_consume)
step('4.group_consume', s4_group_consume)
step('5.commit_resume', s5_commit_resume)
step('6.rebalance', s6_rebalance)
step('7.compression', s7_compression)
ok = sum(results)
print(f'== {ok}/{len(results)} steps OK ==')
sys.exit(0 if ok == len(results) else 1)
PY
RC=$?
if [ $RC -ne 0 ]; then
  echo "--- rdb log tail ---"; tail -8 "$WORK/out.log"
fi
exit $RC
