# 04 · 实施里程碑与落地状态（turso-ref）

- 日期：2026-11-02 · 状态：landed（V1 + P1 + P2 + P3 + P4 全量落地）
- 参考：00–03 研究文档；实现散布 `src/sql/`（见下映射）。

## 1. 落地清单

### V1 — VECTOR 类型基座与精确 KNN

| 项 | 落点 | 状态 |
|---|---|---|
| `VECTOR(N)` 1..=4096、DESCRIBE 渲染 `vector(n)` | `storage/schema.rs`、`exec/ddl.rs` | ✅ |
| 字面量/`STRING_TO_VECTOR`/`TO_VECTOR`/`VECTOR_TO_STRING`/`VEC_TO_TEXT`/`VECTOR_DIM` | `storage/vector.rs`、`exec/scalar/*` | ✅ |
| wire 文本 `'[..]'`（f32 最短往返渲染） | `storage/codec.rs` tag 0x09 | ✅ |
| 精确 KNN：`ORDER BY DISTANCE(v, q, 'cosine'|'euclidean'|'dot') + LIMIT` | `exec/vector_knn.rs` | ✅ |
| 错误面 1292/1582、VECTOR 非 key/索引 | `exec/ddl_alter.rs` 校验 | ✅ |
| e2e（DDL/函数/KNN 序/错误矩阵） | `tests/sql_vector_e2e.rs::vector_surface_end_to_end` | ✅ |

### P1 — mysql bench 基线 + 查询路径优化

| 项 | 落点 | 状态 |
|---|---|---|
| 4 工况基线（point/range/knn-scan/join，pre-opt） | `bench/results/2026-11-02-mysql-baseline.md` | ✅（数值见文件） |
| 表前缀 seek：可见版本计数按 `rocksdb.estimate-num-keys` 自适应（>65_536 走区间法，否则原 walk，字节等价） | `exec/scan.rs::visible_versions_between` | ✅ 回归修复 + 双路径一致性测试 |
| pk 点查：`WHERE pk = ?` 走 PkLookup 直取 | `exec/scan.rs` | ✅ |
| top-N：`LIMIT n + ORDER BY` 用 `select_nth_unstable_by` 截断 + 局部排序 | `exec/select.rs` | ✅ |
| hash join：等值 INNER 两侧建哈希（INNER-only 保守启用，退路 nested loop） | `exec/join_hash.rs`、`exec/scan.rs::join_sources` | ✅ |

### V2/P4 — ANN 索引（SQ8/SPANN 复用）

| 项 | 落点 | 状态 |
|---|---|---|
| DDL：`CREATE [IF NOT EXISTS] VECTOR INDEX n ON t (col) [WITH (METRIC 'x', NPROBE n)]`、`DROP INDEX` | `parse/vector_ddl.rs`（pre-parser 重写）、`exec/ddl_alter.rs` | ✅ |
| 物理层：meta/centroids/postings 三类 kind（0x24–0x26），isqrt 质心数、kmeans×8、SQ8 标定 | `index/vector.rs`、`index/vector_build.rs` | ✅ |
| 回填：CREATE 时全量 build（本批本地 apply）；DROP 分页 4096 kind 过滤清扫 | `index/vector_build.rs::{build_ops,drop_ops}` | ✅ |
| DML 同步维护：INSERT/UPDATE/DELETE 走 `transition_ops` RMW（未建索引=空操作，metric 不匹配跳过） | `index/maintain.rs` 分发 | ✅ |
| 探测：质心排序 + nprobe 分区 + SQ8 粗排 + 短名单 cap=max(4·window, window+16)，回表 `rows_by_pks` 裁决可见性 | `exec/vector_ann.rs`（select.rs 挂钩） | ✅ |
| metric 字面量不匹配 / 多表 / 非单列 → 自动回退精确扫描 | `exec/vector_ann.rs::ann_source` | ✅ |
| 单元（编解码/建维保/召回等价）+ e2e（DDL 矩阵/DML 可见/召回/清扫） | `index/vector_tests.rs`、`tests/sql_vector_e2e.rs::vector_ann_index_end_to_end` | ✅ |

## 2. 已知偏差（回写 COMPAT.sql.md）

- ANN 为**近似**：短名单截断下 top-k 与精确结果可能不同（召回偏差已文档化；测试覆盖"簇可分数据上等价"）。
- `CREATE/DROP VECTOR INDEX` 回填/清扫为**执行节点本地批量写**（`batch_write_async`），未走分布式 2PC 路由（对比 `backfill_index`）；多节点部署需在各自节点执行或后续补分布式回填。
- EXPLAIN 不含 ANN 使用的独立行（access_line 已含 `ann(...)` 访问标注，见 select.rs）。
- NPROBE 运行期不可调（DDL 固化于 meta；无 query hint 面）。
- **并发维护已串行化（C-1 修复，2026-11-03）**：posting 是读-改-写状态，DML/
  回填/清扫并发窗口会互相覆盖丢条目（复现：8 并发任务 INSERT 后 posting 15/48）。
  现按索引维度经 `Shared.latch`（per-key FIFO 信号量）串行化：DML 在
  `apply_writes`/事务 commit 的 ops 计算→落库全程持锁（`write.rs`、
  `tx/session.rs`），CREATE 在目录提交后、DROP 在目录窗口前取同名锁
  （`vector.rs::lock_schema_indexes`，索引 id 升序防 ABBA）。回归测试
  `concurrent_dml_keeps_every_posting`。

## 3. 验证

- `cargo test --lib`：1392 passed / 0 failed（含 vector_tests 5 项、join_hash 5 项、双路径一致性等）。
- `cargo test --test sql_vector_e2e`：2 passed（面 + ANN e2e）。
- bench：post-opt 复测已完成（空载 1m≈2.7）：point 16.7→28.2 ops/s（−25% p50）、join 14.5→23.2 ops/s；range/knn-scan 持平（10k 行未触发区间法阈值，属预期，见结果文件"Post-optimization rerun"节）。
