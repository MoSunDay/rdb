# E2E scenario results -- rdb real-client acceptance

Last full green run: **2026-09-14**, git `c74def1`,
binary `target/release/rdb`.
Runner: `run_all.sh` -> **4 pass, 0 fail**.

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
| scenario_mysql_orders.sh        | orders + 2PC transactions   |         64 | PASS   |
| scenario_starrocks_analytics.sh | DDL compatibility/analytics |         52 | PASS   |
| scenario_vector_search.sh       | vector + FT.* search        |         60 | PASS   |
| **total**                       |                             |  **215**   | **4/4**|

Per-scenario coverage (see each script header for source-verified
semantics):

- **redis_session**: SET/GET (spaces, non-ASCII), TTL expiry, HASH
  profile, ZSET leaderboard ordering, hash-tag slot sharing, MOVED
  redirect (raw text + `redis-cli -c` following), MULTI/EXEC batch.
- **mysql_orders**: mysql-wire DDL/DML, prepared-ish flows, BEGIN/
  COMMIT/ROLLBACK, cross-node 2PC commit and abort paths, fail-closed
  conflict behavior (`ts >= conflict_ts`), information_schema/SHOW
  visibility (incl. `Non_unique=0` for secondary index rows).
- **starrocks_analytics**: MySQL-compatible DDL surface used by
  StarRocks-style clients, columnar scan/aggregates, SHOW INDEX /
  SHOW CREATE TABLE compatibility details.
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
