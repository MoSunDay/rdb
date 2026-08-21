Commit: c0cce389e75f34cf39c06ac4b56f22cde7efd1f3
# SQL 数据面三阶段落地（M1–M3）

## 主题
在 Rust 实现上新增 MySQL 协议 SQL 数据面，并完成单机→集群两级语义：

- **M1（单机引擎）**：MySQL 接入（native-password）、MVCC 版本行存
  （`<slot>/ 0x20 …` 键序=新版本在前）、节点本地时间戳 oracle、执行器
  （DDL/DML/SELECT 代数/EXPLAIN/预编译）、raft 复制表目录。
- **M2（事务与索引）**：BEGIN/COMMIT/ROLLBACK 快照隔离（写集暂存 + own-write
  叠合 + 首提交者胜 1213）、水位驱动 GC、单列二级/唯一索引（回填、提交期维护、
  1062）、sargable 访问路径计划器。
- **M3（分布式）**：raft 全局时间戳（块授权，游标先持久）、`sql_rpc_bind` 节点间
  RPC、跨 slot 2PC 写（prepare/decide + 在疑恢复，presumed-abort 60s）、
  scatter-gather 单表读（band 并发、失败即整查报错）。

## 影响面
- 新模块族 `rust/src/sql/`（front/parse/exec/storage/tx/index/plan/dist）；
  配置新增 `mysql_bind`/`mysql_user`/`mysql_password`/`sql_rpc_bind`。
- 测试新增 6 个 e2e（sql_e2e/sql_txn_e2e/sql_index_e2e/sql_oracle_cluster_e2e/
  sql_2pc_e2e/sql_dist_read_e2e）与配套单元测试；全量套件 773 通过。
- 详见 [features/sql-dataplane.md](../../sql-dataplane.md) 与 `rust/COMPAT.md`。
