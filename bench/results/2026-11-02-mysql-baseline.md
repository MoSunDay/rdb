# MySQL workload baseline (pre-optimization)

- rdb commit: b7378c0 + turso-ref working tree (V1 vector landed, P1 perf not yet)
- profile: **debug** (`cargo build`, opt-level 0) — absolute numbers are
  conservative; before/after deltas use the same profile
- topology: single bootstrap node, fresh store per workload, mysql front
  (`root`/native password), 4 closed-loop clients, 10s measured window,
  10k rows pre-filled (mysql-join: 1k — see note)
- harness: `bench/src/mysql*.rs` (hand-rolled MySQL text-protocol client,
  03-perf-study.md §5.2 shapes)

| workload | rows | ops/s | p50 ms | p99 ms | rows/query |
|---|---|---|---|---|---|
| mysql-point (`WHERE pk = ?`) | 10000 | 16.7 | 110.0 | 124.8 | 1 |
| mysql-range (`pk BETWEEN ? ?` span 100) | 10000 | 17.4 | 107.3 | 128.5 | ~100 |
| mysql-knn-scan (`ORDER BY ABS(pk-?) LIMIT 10`) | 10000 | 15.9 | 113.0 | 147.0 | 10 |
| mysql-join (a 100 × b 1000, nested loop) | 1000 | 14.5 | 113.0 | 138.1 | ~457 |

Reading: every shape sits at ~110 ms/query — scan-bound, not
result-size-bound. `WHERE pk = ?` costs the same as a 100-row range and
the same as the full ORDER BY pass, which is the full-keyspace walk of
`exec::scan` (03-perf-study.md gap #1/#2) dominating everything. The
join ran at 1/10 scale (1k rows) because a 1k×10k nested loop exceeds
minutes per query under a debug build; its baseline is only comparable
to a re-run at the same scale.

## Rerun

```sh
cargo build && cargo build -p rdb-bench
# per workload, fresh store (see tests/common/mysql.rs config shape):
RAFT_BOOTSTRAP=true ./target/debug/rdb -config <cfg-with-mysql_bind> &
./target/debug/rdb-bench --addr 127.0.0.1:18155 --token t \
  --mysql-user root --mysql-pass e2e-sql-pass --workload mysql-point \
  --rows 10000 --key-range 10000 --span 100 --clients 4 --duration 10
```

Fix landed with this baseline: `mysql::query_first_field` now drains the
result-set EOF (a stale trailing EOF used to surface as the next
statement's "bad column count packet [254,0,0,0,0]").

## Post-optimization rerun (2026-11-03, calm host load 1m ≈ 2.7)

Same commit-window (turso-ref working tree with P1+P3+P4 landed: PkLookup,
adaptive prefix seek, top-N, hash join), same profile/topology/harness as
above; fresh store per workload.

| workload | rows | pre ops/s | post ops/s | pre p50 | post p50 | delta |
|---|---|---|---|---|---|---|
| mysql-point (`WHERE pk = ?`) | 10000 | 16.7 | **28.2** | 110.0 | **83.0** | +69% ops/s, −25% p50 |
| mysql-range (`pk BETWEEN ? ?` span 100) | 10000 | 17.4 | 17.1 | 107.3 | 110.4 | flat |
| mysql-knn-scan (`ORDER BY ABS(pk-?) LIMIT 10`) | 10000 | 15.9 | 16.2 | 113.0 | 111.1 | flat |
| mysql-join (a 100 × b 1000) | 1000 | 14.5 | **23.2** | 113.0 | **92.8** | +60% ops/s, −18% p50 |

Reading:

- **point**: the PkLookup arm eliminates the full-keyspace walk — the win
  holds at exactly the predicted shape (one version row fetched directly).
- **join**: hash join replaces 100×1000 nested-loop comparisons with one
  build + 100 probes; p99 drops below the old p50.
- **range / knn-scan flat is expected at this scale**: the adaptive
  `visible_versions_between` switches to the interval method only when the
  store estimates >65_536 keys; a 10k-row store keeps the byte-identical
  small-store walk (regression-safety path). At this row count both shapes
  remain dominated by the version walk + filter, and knn-scan is top-N on
  top of a full pass anyway. The seek wins materialize on larger stores —
  re-run at ≥100k rows when a release profile baseline exists.

All four numbers were taken back-to-back on an idle host (1m load 2.1–2.7);
the earlier partial rerun under foreign compile load (8.4/5.5/3.3 ops/s) is
invalid and superseded by this table.
