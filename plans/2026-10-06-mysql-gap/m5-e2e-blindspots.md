# M5 — e2e 盲区与测试基建收敛

对应 `gap-matrix.md` 的覆盖面问题（非功能差距，而是**已实现但零覆盖**与**样板重复**两类）。
可与 M4 并行；但 `tests/common/mysql.rs` 是 M1/M2/M3/M4 新 e2e 文件的共同依赖——若 M4 先行，
其 e2e 允许临时本地样板，基建合入后统一切换（或接受 M4 稍后合入 e2e 部分）。

## 目标

1. 新增 `tests/common/mysql.rs`：把 16 个 SQL e2e 文件各自重复的 ~100 行样板收敛为单一共享
   模块；后续所有新 SQL e2e（M0–M4 共 5 个新文件）只写用例本体。
2. 新增 `tests/sql_outer_join_e2e.rs`：LEFT/RIGHT OUTER JOIN 已实现但 e2e 零覆盖。
3. 新增 `tests/sql_failover_e2e.rs`：真实三进程、杀 leader → 新 leader 接受 DDL/INSERT、
   ts-block 恢复、客户端重连恢复；现有集群 e2e 从不杀 leader（failover 只覆盖 RESP 侧）。
4. 既有 13 个标量函数中的 UPPER/LOWER/ABS/CHAR_LENGTH 由 M1 的 `tests/sql_funcs_e2e.rs` 补
   e2e（本里程碑不重复建文件）；M2 落地时同步更新旧 ON-DUP 拒绝断言（见 m2 文档 e2e 节）。

## 现状与证据

- 样板重复：16 个 SQL e2e 文件（12 个 `tests/sql_*_e2e.rs` + `mysql_compat_e2e.rs` +
  `starrocks_model_e2e.rs` + `auto_increment_e2e.rs` + `columnar_e2e.rs`）；其中 14 个各自
  定义 `async fn connect`（见 grep 计数），`tests/sql_e2e.rs` 头部 ~90 行（connect / ddl /
  rows / rows_ordered / int 等 helper，:18-100）被近复制到各文件。`tests/common/mod.rs` 目前
  只有 RESP 侧进程基建（spawn/kill/resp_ready），无 MySQL 客户端 helper。
- Outer join：`src/sql/parse/ast.rs` `JoinKind::{Left, Right}`（:198-199，注释"OUTER sides
  null-extend"）与 `src/sql/exec/select.rs` 连接执行已实现；全 `tests/*.rs` grep
  `LEFT JOIN|RIGHT JOIN` 零命中——纯盲区（含 `sql_join_cluster_e2e.rs` 只测内连接类形态）。
- 集群 failover：`tests/process_failover_e2e.rs` / `backup_failover_e2e.rs` 走 RESP 面；
  SQL 集群 e2e（`sql_2pc_e2e.rs`、`sql_join_cluster_e2e.rs`、`sql_oracle_cluster_e2e.rs`、
  `sql_setop_cluster_e2e.rs`、`sql_dist_read_e2e.rs`）从不杀 leader——SQL 面 leader 切换
  （raft 选举 + SQL ts 授权迁移）无任何测试证据。
- ts-block：`src/sql/dist/recover.rs`（329 行）实现 leader 授块恢复；`/sql/ts?n=` HTTP 授块
  在 COMPAT.md SQL 章节记录——杀 leader 后新 leader 必须能继续授块（`sql_ts_cursor` 持久化
  经 raft 先行），该路径同样零 e2e。

## 实现拆分（文件与职责，标注行数预算）

- `tests/common/mysql.rs`（新增，预算 ≤400，纯 helper 函数集、无状态对象）：
  - `connect(node, user, pass)`：端口解析 + 15s 重试（吸收 `sql_e2e.rs` :18-42 的
    `async fn connect`）；
  - `ddl(conn, sql)`：leader 就绪重试（同 `sql_e2e.rs` `ddl` helper 语义）；
  - `grid(conn, sql) -> Vec<Vec<MVal>>` / `col(conn, sql, i)`：行集/单列读取；
  - `errno(sql_err) -> u16`：MySQL 错误码抽取（负矩阵断言共用）；
  - `wait_table(conn, table)` / `wait_mysql_ready(node)`：DDL 可见性与端口就绪；
  - `int/str/nullable` 值构造小工具。
  - 迁移策略：新文件直接用；**存量 16 个文件分批机械替换**（每文件删本地副本改
    `use common::mysql::*`），单独 PR、不夹带行为变更。
- `tests/sql_outer_join_e2e.rs`（新增，预算 ≤350）：
  - LEFT JOIN 右侧空补 NULL、RIGHT JOIN 左侧空补 NULL、匹配行的字段值、
    与 WHERE（on-clause vs where-clause 语义差）、多表链式、JOIN + GROUP BY 聚合
    （COUNT(右表列) 的 NULL 不计数）、`USING` 形态；
  - 集群形态可后置（内连接集群行为已有 `sql_join_cluster_e2e.rs` 佐证 join 下推边界）。
- `tests/sql_failover_e2e.rs`（新增，预算 ≤400）：
  - 起 3 个真实进程（`tests/common/mod.rs` 的 spawn 基建 + MySQL 面端口），确认 leader
    （DDL 成功者即 leader，或经 HTTP 探测）；
  - 建表 + 写入基线 → `kill_now()` 杀 leader → 等待新 leader（DDL 重试退避收敛）→
    DDL（CREATE/ALTER INDEX）与 INSERT 均成功；
  - ts-block 恢复：failover 后持续写入直至触发新 ts 块申请（或直接断言 `/sql/ts?n=` 在
    新 leader 上 200），写入不断流；
  - 客户端恢复：旧连接报错后重连新 leader 读回基线数据（MVCC 读一致性）；
  - 参考 `process_failover_e2e.rs` / `raft_cluster_e2e.rs` 的进程编排与日志取证
    （`stderr_tail`/`all_ctx`）。

## 单测

- 本里程碑以 e2e 为主，无新 src 单测；`tests/common/mysql.rs` 自身用首个消费方
  （`sql_outer_join_e2e.rs`）验证，不写独立测试二进制。

## e2e

即上述三个交付物本身。迁移存量 16 文件后跑全量
`cargo test --workspace --no-fail-fast` 确认零行为漂移（helper 语义必须逐行等价迁移，
`rows_ordered` 的稳定排序行为保留）。

## 文档同步

- `features/e2e-coverage.md`：登记 `sql_outer_join_e2e.rs`、`sql_failover_e2e.rs`、
  `tests/common/mysql.rs` 基建及存量迁移完成状态；
- `agents/rust/sql.md`：e2e 基建约定（新 SQL e2e 一律 `use common::mysql`，禁止再本地复制
  connect/ddl 样板）；
- `COMPAT.md`：如 failover e2e 暴露 leader 切换窗口语义差异（授块间隙写失败的重试语义），
  补记录；`features/changelog/<日期>/`。

## 验收

- 16 个存量 SQL e2e 文件不再含本地 `async fn connect` 副本（grep 计数归零），每个文件
  净减 ~80-100 行；
- LEFT/RIGHT OUTER JOIN 与 SQL-leader-failover 在 `features/e2e-coverage.md` 有对应条目；
- `cargo test --workspace --no-fail-fast` 全绿（failover e2e 需稳定：leader 等待用充足
  退避上限，避免 CI 抖动）。

## 风险

- 杀 leader 的时序不确定（选举时长受环境负载影响）：用 deadline + 轮询 DDL 探活，不 sleep
  固定值；失败时附 `all_ctx` 日志（既有基建）；
- ts-block 恢复断言依赖写入量触发授块，e2e 可直接调 `/sql/ts?n=` 缩短路径（HTTP 面已有
  测试先例）；
- 存量文件批量迁移的机械错误：分批 PR（每次 3-4 个文件），每批全量跑对应 e2e；
- MySQL 面端口冲突：沿用 `tests/common/mod.rs` 的 `free_addr` 端口分配惯例。
