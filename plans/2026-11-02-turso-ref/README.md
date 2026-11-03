# turso(libsql) 深度参考 → SQL 面向量能力 + 性能批次

- 日期：2026-11-02 ｜ 状态：**执行中**
- 参考 pin：`tursodatabase/libsql` @ `d8bc862d20bedc72945a1da27c8b7045414c0261`（`link_repos/turso`，blob:none 部分克隆，gitignore 纯本地参考）

## 目标与原则

1. **线协议不变**：始终 MySQL wire；向量语法锚定 **MySQL 9.0 VECTOR 面**（`VECTOR(N)` 列、`STRING_TO_VECTOR`/`VECTOR_TO_STRING`/`DISTANCE(v1,v2,'cosine'|'euclidean'|'dot')`/`VECTOR_DIM`、字面量 `'[1,2,3]'`）；libsql 的 `vector()`/`vector_distance_*`/`vector_top_k` 面仅作语义对照。
2. **参考 ≠ 移植**：libsql 核心是 C（SQLite fork），产出为 Rust 语义对齐实现；不引入 turso 私有协议。
3. **复用家底**：向量底层复用 `src/search/{vecmath,quant,ann}`（SQ8/SPANN），不另造轮子。

## 文档索引

| 文档 | 内容 |
|---|---|
| [00-turso-map.md](./00-turso-map.md) | libsql 架构地图与 rdb 对应物 |
| [01-vector-surface.md](./01-vector-surface.md) | libsql ↔ MySQL 9.0 ↔ rdb 向量 SQL 面 1:1 对照与落地决策（D1–D13） |
| [02-vector-internals.md](./02-vector-internals.md) | libsql 向量索引内部机制（DiskANN、同事务同步维护）→ rdb SQ8/SPANN 复用方案与可见性决策 |
| [03-perf-study.md](./03-perf-study.md) | SQLite 性能技术清单 → rdb 差距矩阵（10 项）+ P1-1 前缀 seek 具体方案 + bench 设计 |
| [04-impl-milestones.md](./04-impl-milestones.md) | V1/P1/V2 批次任务分解与落地状态（已回写：全量 landed） |

## 里程碑

| 阶段 | 内容 | 状态 |
|---|---|---|
| Phase 1 | 研究文档 00–03 | ✅ 完成 |
| Phase 2 (V1) | VECTOR 类型基座、函数四件套、精确 KNN、协议/e2e | 🔄 基座已落地（storage/vector.rs、tag 0x09、DDL/write/render 全链路），函数/KNN/协议进行中 |
| Phase 3 (P1) | mysql bench 基线 + 表前缀 seek / pk 点查 / top-N / hash join | 🔄 进行中 |
| Phase 4 (V2) | `CREATE/DROP VECTOR INDEX` + SQ8/SPANN ANN 扫描 + 召回 e2e | ⏳ 待 02 结论（已出：同 batch 同 commit_ts 派生 + read_ts 回表裁决 + 精确兜底） |

## 关键研究结论（详见各文档）

- libsql pin d8bc862d 下 `MATCH` 虚表已废弃，现行形态为 `vector_top_k` 表值函数 + `CREATE INDEX ... libsql_vector_idx(col,'type=diskann',...)`；向量索引唯一实现 DiskANN，**同事务同步维护**，无后台/最终一致窗口。
- rdb 维度上限取 **4096**（MySQL 16383 收窄进台账）；wire 文本协议先行（VAR_STRING `'[..]'`），`MYSQL_TYPE_VECTOR`(疑 242) 未实测前不启用。
- 性能 P1 预期前三：表前缀 seek（消灭全库 walk）> pk 谓词点查（现状无索引即全库 walk）> top-N 堆排序；随后 hash join。
