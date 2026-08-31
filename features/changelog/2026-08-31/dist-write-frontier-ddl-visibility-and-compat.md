Commit: (working-tree, pre-initial-commit)

# 分布式写前沿、DDL 可见性屏障与客户端兼容加固

## 背景
承接 08-29 的 2PC 契约修复（[../2026-08-29/sql-2pc-and-bug-sweep.md](../2026-08-29/sql-2pc-and-bug-sweep.md)），
本轮消除三类分布式正确性缺口：写时间戳可能落后于本节点已读版本（newest-ts-wins
静默吞写）、UPDATE/DELETE 只匹配本地切片、DDL ack 与 follower 目录可见性之间的
窗口；同时落地真实客户端 e2e 场景库与 store-only 嵌入构建。

## 变更
### 分布式写正确性
- **`src/sql/tx/ts.rs`、`src/sql/tx/global.rs`**：`now()` 改骑集群游标前沿
  （`sync_cursor_frontier`，try_read 不阻塞 runtime）；写路径先按读点
  `reserve_write_frontier`，`alloc_n_above` 作废过期/过短块尾重租，新增
  `observed_floor` 记录已知已提交上界——协调者 ts 视图滞后时多语句事务不再
  "读旧快照→匹配 0 行→空转 COMMIT"（第二个 UPDATE 起全部静默丢失）。
- **`src/sql/dist/gather.rs`、`src/sql/exec/write.rs`**：集群就绪时 UPDATE/DELETE
  行匹配走 `gatherable_by_name` + `gather_rows` 扇出全部 band，绝不只看本地切片。
- **`src/sql/dist/plan.rs`**：index-only 计划（目录回填经 2PC 路由）至少消耗 1 个
  ts，范围起点不再交给后续提交。

### DDL 可见性
- **`src/sql/storage/replicate.rs`（新增）、`src/sql/storage/catalog.rs`、
  `src/sql/exec/ddl.rs`**：CatalogTxn 记录 applied entries；DDL ack 前逐 peer 轮询
  `/get`（与目录读同一 FSM 视图）确认已服务（best-effort，每 peer 1.5s 上限）——
  新索引在 follower 立即可见，follower 协调的写不再用陈旧目录漏掉唯一索引强制。
- **`src/main.rs`**：metrics 镜像改 `try_write`——DDL 持有 raft 写守卫窗口时阻塞锁
  会冻结 tokio driver；跳拍由下一拍补齐。
- **`src/sql/exec/show.rs`**：SHOW INDEX 补 `Non_unique`（UNIQUE=0/二级=1，MySQL 语义）。
- **`src/sql/columnar/writer.rs`**：段号分配跳过已占用号（崩溃恢复重用不再撞号）。

### 客户端兼容
- **`src/sql/parse/session.rs`（新增）、`src/sql/parse/translate.rs`**：会话变量
  策略独立成模块；`SET autocommit=1`、`SET NAMES [COLLATE]` 按引擎真实模式接受为
  no-op，`autocommit=0`/isolation/time_zone/SESSION 参数维持大声拒绝——真实客户端
  握手（mysql CLI/JDBC/pymysql）不再被首屏 SET 打断。
- **`src/search/ft_search.rs`**：FT.SEARCH reply 补数组头（平铺 RESP 数组），
  redis-cli/iredis/python 不再在 total 后截断。

### 构建与 e2e
- **`Cargo.toml`、`src/lib.rs`**：`store` 特性只暴露 RocksDB KV 层（嵌入方免编译
  raft/SQL/search 全栈），`full`（默认）补齐；重依赖转 optional，bin 标注
  required-features。
- **`scrtips/e2e_scenarios/`**：真实客户端场景库——env.sh 三节点 bring-up（token
  随机生成，不复制 config/）+ redis 会话/榜单、mysql 订单+2PC、starrocks 分析、
  向量+FT 四个场景，run_all.sh 一键回归（215 断言 4/4，见 RESULTS.md）。

## 测试覆盖
| 功能 | 测试名 | 文件 |
|------|--------|------|
| 多语句事务全落地 | `multi_statement_txn_applies_every_update` | tests/sql_txn_e2e.rs |
| 跨 owner UPDATE/DELETE | `follower_coordinated_update_delete_reaches_all_slot_owners` | tests/sql_2pc_e2e.rs |
| 滞后块尾不落过期写 | `stale_block_follower_reads_and_writes_at_the_cluster_frontier` | tests/sql_2pc_e2e.rs |
| 写前沿预留语义 | `carve_never_serves_below_observed_commits` 等 7 个 | src/sql/tx/global_tests.rs |
| follower 立即可见+唯一强制 | `unique_index_visible_and_enforced_on_followers_immediately` | tests/sql_ddl_visibility_e2e.rs |
| FT.SEARCH reply 形状 | `ft_search_reply_is_a_flat_array` | tests/search_e2e.rs |
| 复制屏障容错 | `peer_has_treats_unreachable_as_stale` | src/sql/storage/replicate.rs |
| SET 握手接受/拒绝 | parse smoke（`src/sql/parse/mod.rs:213`） | src/sql/parse/mod.rs |
| 段号复用可见性 | `three_autocommit_inserts_keep_every_batch_visible` 等 | src/sql/columnar/tests_scan.rs |

- 全量回归：`cargo test --workspace` → 1019 passed / 0 failed
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → 零警告
- 构建：`cargo build --workspace` 与 `cargo build --release` → 干净
- 行数：新增 `session.rs` 50 / `replicate.rs` 116 / `sql_ddl_visibility_e2e.rs` 171 /
  `vector_helpers.sh` 78 ≤ 400；最大迭代文件 `translate.rs` 768 ≤ 800
  （`scenario_vector_search.sh` 拆出 helpers 后 345 ≤ 400，拆分后单场景复跑 PASS）

## Impact Surface
- 集群写路径：2PC 协调者的写 ts 与读点关系、UPDATE/DELETE 的 band 扇出——多语句
  事务与跨 owner 写从静默丢改变为全量生效。
- DDL 后 follower 的目录可见性窗口收窄到屏障轮询（毫秒级有界）；单节点世界无感。
- MySQL 客户端握手：首屏 SET 不再报错；FT.SEARCH 客户端读到的 reply 多一层总数
  数组头（对按协议解析的客户端是修复，非破坏）。
- 嵌入方：`--no-default-features --features store` 可只链 KV 层。
- 不影响：RESP 数据面命令语义、Raft 控制面协议、存储布局（键编码不变）。

## Related Docs
- [agents/rust/sql.md](../../../agents/rust/sql.md)（时间戳/集群模式不变量已同步）
- [COMPAT.md](../../../COMPAT.md) "2PC writes" 节
- [既有相关 changelog](../2026-08-29/sql-2pc-and-bug-sweep.md)
