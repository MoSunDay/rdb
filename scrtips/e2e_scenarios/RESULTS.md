# E2E scenario results -- rdb real-client acceptance

Last full green run: **2026-09-16**, git `5b67c47`,
binary `target/release/rdb`.
Runner: `run_all.sh` -> **4 pass, 0 fail**.

2026-09-18: `scenario_kafka_sdk.sh` grew step 7 (compression, P5b) --
gzip/snappy/lz4 x 30 msgs each via the real SDK (`batch.num.messages=10`
+ `linger.ms=200` so librdkafka actually forms compressed batches),
then a key+value consume roundtrip. Build-feature aware: on a default
binary every delivery callback comes back 76 UNSUPPORTED_COMPRESSION_
TYPE -> the step self-SKIPs (6/6, exit 0); on
`cargo build --release --features kafka-codecs` it runs and passes
(7/7: gzip@11..40 snappy@41..70 lz4@71..100). zstd is not probed via
the SDK (this build ships zstd produce uncompressed, attributes=0);
its 76 rejection is pinned by `tests/kafka_codec_e2e.rs`.

2026-09-18: kafka bench workloads added (P5c) --
`scenario_kafka_bench.sh` drives the kafka front with the repo's own
load generator instead of a client library: rdb-bench grew
`kafka-prod` (Produce v2, acks=1, 100 records/request, 2 clients) and
`kafka-fetch` (Fetch v4, 1 client, 500ms long-poll tail) workloads
from a hand-rolled client in `bench/src/kafka_wire.rs` /
`kafka_reply.rs` / `kafka.rs` (no rdb-lib link). Standalone, NOT part
of `run_all.sh` (~55s). Result: **PASS** -- produce 1,159,800 records
@ ~77k/s zero error replies, then the fetcher reads 1,159,802
(= produced + the 2 XADD topic-seed entries) inside its 30s window;
base_offset monotonicity asserted client-side. Usage of the two
workloads against a live node (kafka front needs `kafka_bind`; the
bench pre-creates `bench1/q0` over RESP itself):

    # produce: 2 clients, 15s, 100 records per request
    ./target/release/rdb-bench --addr 127.0.0.1:6379 --token <raft_token> \
        --host 127.0.0.1:9092 --workload kafka-prod \
        --clients 2 --duration 15 --batch 100
    # fetch: 1 client tails topic bench1 partition 0 from offset 0
    ./target/release/rdb-bench --addr 127.0.0.1:6379 --token <raft_token> \
        --host 127.0.0.1:9092 --workload kafka-fetch \
        --clients 1 --duration 30

On the kafka workloads `ops` counts RECORDS (so ops/s = records/s) and
a `records_bytes=` line carries payload volume; kafka-only flags
(`--host/--topic/--batch`) are rejected on RESP workloads.
Reference-box numbers: produce ~72-77k rec/s (acks=1, one fsync per
request), tail ~61-84k rec/s; the scenario's 2x fetch window covers
the gap.

2026-09-17: `upgrade_rehearsal.sh` added -- W2.2 catalog co-upgrade
rehearsal (standalone drill, NOT part of `run_all.sh`; ~3 min cold,
builds both binaries itself). Proves the stop-the-world same-batch
swap across this iteration's catalog-shape change (`TableSchema.pk`
string -> column array + DECIMAL `SqlType` variant; COMPAT.md gates
ROLLING as unsafe there): old binary `c22ff37` (detached worktree
`.upgrade-old`, auto-built/removed) seeds the old feature surface
(single-col BIGINT pk + unique/secondary index, StarRocks single-col
PRIMARY KEY model, AUTO_INCREMENT, UPDATE/DELETE for multi-versions +
tombstones) and snapshots rows/COLUMNS/INDEX; SIGTERM-all; the NEW
binary (`8372d90`) restarts the same data dirs and must match every
snapshot, run the one-time boot floor scan exactly once, carry the
AUTO_INCREMENT counter across (65 -> 129), keep 1062/txn semantics,
and unlock the new surface (DECIMAL(10,2) + index, composite pk,
StarRocks multi-column PRIMARY KEY); then kill -9 all + restart: no
second floor scan, snapshot stable again, counter at 193. Result:
**94 assertions, PASS** -- twice consecutively with full cold builds
and automatic scratch/worktree cleanup; no upgrade bug found (the
`de_string_or_vec` compat + boot floor scan held). Semantics notes:
AUTO_INCREMENT allocation is leader-only at BOTH versions (auto
inserts route via the raft leader; followers still refuse), and
follower-coordinated PK-model upserts can 1213 fail-fast on tight
timing (pre-existing M2 ts-ordering caveat, not upgrade-related).
Env: `RDB_UPGRADE_OLD_COMMIT`, port band 32900, `RDB_E2E_KEEP_WORKDIR=1`
keeps scratch for iteration.

2026-09-16: DECIMAL(p,s) + composite-PK assertion coverage (W2.2):
`scenario_mysql_orders.sh` gains orders_amt (exact 0.1+0.2, half-up
rounding read-back, SUM/AVG scale, decimal secondary-index point
lookup, DESCRIBE decimal(10,2), DECIMAL(5,2) 1292 edge) and orders_item
(composite pk upsert == single-column upsert, tuple point lookup/
UPDATE, both PRI flags + PRIMARY index rows, AUTO_INCREMENT 1075);
`scenario_starrocks_analytics.sh` gains sr_mpk (multi-column PK model
upsert on the full tuple, exact decimal v, SUM + GROUP BY, DESCRIBE)
plus DOUBLE-in-composite-pk and columnar-DECIMAL rejections. Found &
fixed a real wire bug while doing so: `result_type` labeled
SUM(decimal) / AVG(decimal) / decimal arithmetic result columns as
DOUBLE (or INT), so SDK clients parsed the exact text cells as floats
(mycli showed 0.6 for "0.60") and the binary protocol rejected the
decimal cells outright; metadata now mirrors the value shapes
(src/sql/exec/select.rs, unit + sql e2e suites re-run green).

2026-09-14: re-validated around the ts-authority fail-fast + CLUSTER
INIT seeding fix (commits `525777c`/`c74def1`). `scenario_mysql_orders.sh`
PASS x5 with the fixed binary; the pre-fix binary (`f75c617`, built in a
scratch worktree) also passed 5/5 on this quiet host -- the historical
silent-lost-update failure is load/timing dependent (prior changelog
notes host load ~140), so the scenario run here serves as regression
coverage only; the correctness proof lives in unit + integration tests
(`reserve_write_frontier` strict/lenient, `sql_2pc_e2e`,
`cluster_init_seeds_leader_binds_in_sql_nodes`).

2026-08-31: `scenario_vector_search.sh` re-run PASS after extracting
`vector_helpers.sh` (line-budget split, logic moved verbatim); release
binary rebuilt from the same working tree first.

## Prerequisites

| client     | version   | used by                     |
|------------|-----------|-----------------------------|
| redis-cli  | 6.0.16    | all scenarios               |
| iredis     | 1.16.1    | redis_session, vector       |
| python     | 3.13.3    | vector (redis-py SDK block) |
| redis-py   | 7.4.1     | vector section I            |
| mysql      | 8.0.44    | mysql_orders, starrocks     |

The harness generates per-run yaml whose raft token comes from
`RDB_E2E_TOKEN` (env or freshly random); no token from `config/` is
ever copied. Default port base 32700, 3 nodes at base+idx*10.

## Reproduce

```sh
cargo build --release                 # default features (full)
scrtips/e2e_scenarios/run_all.sh      # exit code = # failed scenarios
# single scenario:
scrtips/e2e_scenarios/run_all.sh scenario_vector_search.sh
# workspace unit/integration tests:
cargo test --workspace
```

Latest results (assertions = `^ok` lines in the per-scenario log):

| scenario                        | story                       | assertions | result |
|---------------------------------|-----------------------------|-----------:|--------|
| scenario_redis_session.sh       | session cache / leaderboard |         39 | PASS   |
| scenario_mysql_orders.sh        | orders + 2PC transactions   |        117 | PASS   |
| scenario_starrocks_analytics.sh | DDL compatibility/analytics |         76 | PASS   |
| scenario_vector_search.sh       | vector + FT.* search        |         60 | PASS   |
| **total**                       |                             |  **292**   | **4/4**|

Per-scenario coverage (see each script header for source-verified
semantics):

- **redis_session**: SET/GET (spaces, non-ASCII), TTL expiry, HASH
  profile, ZSET leaderboard ordering, hash-tag slot sharing, MOVED
  redirect (raw text + `redis-cli -c` following), MULTI/EXEC batch.
- **mysql_orders**: mysql-wire DDL/DML, prepared-ish flows, BEGIN/
  COMMIT/ROLLBACK, cross-node 2PC commit and abort paths, fail-closed
  conflict behavior (`ts >= conflict_ts`), information_schema/SHOW
  visibility (incl. `Non_unique=0` for secondary index rows),
  DECIMAL(p,s) exact arithmetic/rounding/SUM/AVG + secondary index
  point lookup + 1292 out-of-range edge, composite-PK upsert/lookup/
  UPDATE + PRI/SHOW INDEX + AUTO_INCREMENT 1075 rejection.
- **starrocks_analytics**: MySQL-compatible DDL surface used by
  StarRocks-style clients, columnar scan/aggregates, SHOW INDEX /
  SHOW CREATE TABLE compatibility details, multi-column PRIMARY KEY
  model (full-tuple upsert, exact decimal values, GROUP BY) and
  composite-pk / columnar-DECIMAL DDL rejections.
- **vector_search**: VADD/VSIM (FP16 via stdin, similarity in [0,1]),
  FT.CREATE/ADD/BUILD, BM25 text, exact KNN pre/post FT.BUILD, term
  prefilter + KNN, FT.INFO, then the same KNN query answered
  identically by redis-cli, iredis AND redis-py.

## Wire contracts validated by real SDKs

- **FT.SEARCH reply is a flat array**: `[total:int, docid, score, ...]`
  (NOCONTENT WITHSCORES). redis-py's strict RESP typing asserts this
  shape (`scenario_vector_search.sh` section I): element 0 must parse
  as `int` and every pair as `bytes`.
- **CLUSTER SLOTS**: redis-py consumes the topology; the ranges tile
  slots 0..16383 exactly once (asserted in-section).
- **AUTH gate**: every client authenticates with the raft token
  (`REDISCLI_AUTH`, `password=`) before any command.

## Known gaps (not exercised by this suite)

- `RedisCluster()` (redis-py) cannot connect yet: its handshake issues
  the `COMMAND` command, which rdb does not implement. The topology
  call `CLUSTER SLOTS` itself parses fine; direct `Redis()` clients
  are fully functional.
- `config/conf_3268*.yaml` still carry plaintext raft tokens (pre-
  existing P3 debt; the e2e harness does not use them).
