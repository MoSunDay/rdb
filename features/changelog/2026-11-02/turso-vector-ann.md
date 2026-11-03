# 2026-11-02 · SQL 向量能力（V1 精确 KNN + P4 ANN 索引）与查询性能批次

来源计划：`plans/2026-11-02-turso-ref/`（以 tursodatabase/libsql pin d8bc862d 为只读
参考）。契约与偏差台账：`COMPAT.sql.md` "Vector capability & deviations"。

## V1 — VECTOR 类型与精确 KNN

- `VECTOR(N)`（1..=4096，DESCRIBE 渲染 `vector(n)`；不可作 pk/普通索引键，wire 以
  文本 `'[..]'` 往返，f32 最短渲染）。
- 函数四件套：`STRING_TO_VECTOR`/`TO_VECTOR`、`VECTOR_TO_STRING`/`VEC_TO_TEXT`、
  `VECTOR_DIM`（别名成对等价，`VECTOR_DIM` 也收字面量）。
- 精确 KNN：`ORDER BY DISTANCE(v,q,'cosine'|'euclidean'|'dot') LIMIT k`；dot=负内积、
  零向量 cosine=1.0；错误面 1292（坏字面量/维度不符）与 1582（参数个数）。
- e2e：`tests/sql_vector_e2e.rs::vector_surface_end_to_end`（DDL/元数据/往返/序/错误矩阵）。

## P4 — ANN 索引（SQ8/SPANN 复用）

- DDL：`CREATE [IF NOT EXISTS] VECTOR INDEX n ON t(col) [WITH (METRIC 'x', NPROBE n)]`
  / `DROP INDEX`；pre-parser 重写 + marker 提升回 AST（`parse/vector_ddl.rs`）。
- 物理：meta/centroids/postings 三 kind（0x24–0x26）；kmeans（seed 7×8 轮，质心
  isqrt 截 1..=64）+ 每质心 SQ8 postings；CREATE 全量回填、DROP 分页清扫。
- DML 同 batch 同步维护（`index/maintain.rs` → `transition_ops` RMW）；未建索引
  no-op、metric 不匹配跳过。
- 探测（`exec/vector_ann.rs`）：质心排序 → nprobe 分区 → SQ8 粗排 → 短名单
  cap=max(4·window, window+16) → `rows_by_pks` 回表按 MVCC 裁决可见性；形状或
  metric 不匹配自动回退精确扫描（召回偏差见台账）。
- 单元：`src/sql/index/vector_tests.rs`（codec 往返、建维保计数、簇可分数据
  ANN==精确、DDL 解析矩阵）；e2e：`vector_ann_index_end_to_end`。

## P1 — 查询路径性能批次

- 表前缀 seek 回归修复：可见版本计数按 `estimate-num-keys` 自适应（>65_536 走
  区间法，小库保留原 walk 字节等价），双路径一致性测试钉死。
- `WHERE pk=?` PkLookup 直取；`LIMIT n+ORDER BY` select_nth 截断 top-N；
  等值 INNER hash join（保守启用，nested loop 兜底）。
- bench 基线与复测记录：`bench/results/2026-11-02-mysql-baseline.md`。

- DML/回填/清扫的 posting 读-改-写窗口按索引经 `Shared.latch` 串行化
  （`vector.rs::lock_schema_indexes`；DML 于 `write.rs`/`tx/session.rs`，
  CREATE 目录提交后、DROP 目录窗口前取锁，id 升序防 ABBA）；复现测试
  `concurrent_dml_keeps_every_posting`（修复前 posting 15/48）。

## 验证

- `cargo test --lib` 1393 passed / 0 failed；`--test sql_vector_e2e` 2 passed。
