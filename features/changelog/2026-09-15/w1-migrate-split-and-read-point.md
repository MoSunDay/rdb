Commit: e3895f3 / f75c617

# W1 开工：migrate 模块按职责拆分、自提交 SELECT 读点折叠（mysql_orders 排查防御修复）

## 背景
M0 批次关闭（复评 go-live，见
[../2026-09-14/m0-hardening.md](../2026-09-14/m0-hardening.md)）后按评审 TODO 进入 W1：
W1.0 把 772 行的 `src/command/migrate.rs` 按职责拆分（复制通道改造铺路）；同时对 M0
期间登记为 HEAD 既有缺陷的 `scenario_mysql_orders` 跨节点 2PC"丢写"完成根因排查，
落地其中的防御性修复。

## 变更
### W1.0 migrate 模块拆分（`e3895f3`）
- `src/command/migrate.rs`（772 行）→ `src/command/migrate/`：
  - `data.rs`（227 行）：数据面——`MIGRATE host port ... KEYS` 的 dump→ASKING→RESTORE
    传输 + `RESTORE` 目标侧；
  - `task.rs`（353 行）：管理面——`migrate task/list`、`run_migration` reshard 编排、
    wire 助手（`cmd_ok`/`keys_in_slot`）、任务 JSON 经 raft 键 `migrate_task` 复制；
  - `mod.rs`（74 行）：仅 dispatch（`handle`）与 usage；`test_support.rs`（158 行）：
    `#[cfg(test)]` 共享测试基建。
- 纯机械搬移零行为变更：函数名全部保留、仅调可见性（`pub(super)`）；
  `command/mod.rs` 注册表经 `pub use data::restore` 无需改动；git rename 保留历史。
  验证：`--lib command::migrate` 7 pass、`migrate_e2e` 1 pass。

### mysql_orders 排查结论 + 防御修复（`f75c617`）
根因链（静态证据链闭合，置信度 ~85%；数据未丢，属**可见性丢失**，可自愈）：
1. `CLUSTER INIT` 后 `sql_nodes` 注册是 3s ticker 且被 `cluster_ready` 门控
   （`sql/tx/nodes.rs`），leader 自注册最坏 ~6s；窗口内协调者（follower）块 refill
   每 200ms 失败（日志 `sql ts: block refill failed ...`，`sql/tx/global.rs`）；
2. `reserve_write_frontier` fetch 失败被**静默吞掉**（`sql/tx/session.rs` →
   `sql/tx/global.rs` 的 fallback），提交落入 GAP fallback ts（≈+2M，"无游标可载"）；
3. 2PC 照常全票通过并返回 OK，但该 ts 对**非参与者节点**的读点（`now()`）不可见 →
   场景断言读回旧值。cargo e2e 不踩坑是因为其 harness 显式 poll `raft get sql_nodes`
   收敛（`tests/common/mod.rs`），shell 场景的 `wait_routing` 只等拓扑不等注册表。

已落地（防御性，修读点脱钩的常在窗口）：自提交 SELECT 在钉 `now()` 前先
`sync_cursor_frontier()`（`sql/exec/select.rs`），对齐 BEGIN（`exec/mod.rs`）与写路径
（`exec/write.rs`）——否则刚 COMMIT 的写在跨连接立读中最多隐身一个 refill tick
（200ms）+ FSM 复制延迟。该折叠为本地 try_read 镜像读（无 RPC 不阻塞），读热路径安全。

**未修（语义取舍，移交决策）**：修复点 2（2PC 对 GAP fallback fail-fast，可重试
1213）与修复点 3（`CLUSTER INIT` 同笔 raft 写种子化 `sql_nodes`，消灭触发窗口）；
过渡期可先将场景断言改 poll。带日志复现一次场景可把"本次失败即机制 A"置信度
~70% 封顶。

## 验证
- `cargo fmt --check` / `cargo build --workspace` /
  `cargo clippy --workspace --all-targets -- -D warnings` 干净；
- `cargo test --lib`：876 pass（含 `command::migrate` 7、sql 模块 329）；
  e2e：`migrate_e2e` 1、`sql_dist_read_e2e` + `sql_txn_e2e` 8 全过；
- `cargo test --workspace --no-fail-fast`（共享宿主机 load ~140 下执行）：除 3 个目标
  外全绿——`process_sigterm_e2e`（30s 启动饥饿）、`sql_txn_e2e`（固定端口
  Address already in use，慢拆除残留）、`process_cluster_e2e::drill_py_scenario`
  （同窗口受扰）。三者**隔离重跑全部通过**（8/8、2/2、4/4），与 2026-09-05 /
  M0 门禁登记的同类环境噪声一致；本批改动路径（migrate 搬移、SELECT 读点）与
  失败面无交集。
