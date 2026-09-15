# mysql_orders 终局修复：ts authority 不可达时的 fail-fast 与 CLUSTER INIT 种子

Commit: 525777c, c74def1

> **2026-09-14 复审补口**：复审发现 `commit_inner`（显式 BEGIN..COMMIT 路径）的
> strict 探测集只含行键，而 2PC 判定（`dist::plan::build`）同时路由索引键——
> 「行全本地、索引面落远端」的事务仍会以宽松预留走 2PC 盖 GAP。已于当日修复
>（探测集并入索引键 + 回归测试），见文末补记。

## 背景

`scrtips/e2e_scenarios/scenario_mysql_orders.sh` 压测下曾出现「主键更新静默丢失」。
w1 迁移收尾（2026-09-15/w1-migrate-split-and-read-point.md）已定位根因链并修复其中 1/3，
遗留 2、3 两项「语义取舍，移交决策」：

1. **跨节点提交无 TSO 纪律**：ts authority（raft leader 的 `sql_ts_cursor`）不可达时，
   2PC 提交降级为本地 GAP（在已知全局水位之上自增区间）。GAP 区间任何后续 cursor
   申请都无法覆盖——leader 重启后 cursor 从已持久化值继续，会直接越过 GAP 前沿；
   newest-ts-wins 的行平面随即把这笔写入永久掩埋。等价于 TiDB TSO 不可达时仍盖章提交。
2. **`CLUSTER INIT` 不种子 `sql_nodes`**：FSM 里 leader binds 起初为空，靠 3s 注册循环
   事后补齐；INIT 刚完成的窗口内 peer 无法解析 ts authority，恰好放大问题 1 的暴露面。

## 变更

### 525777c — 跨节点提交 fail-fast（TiDB TSO 纪律）

- `src/sql/tx/global.rs`：新增 `reserve_write_frontier(floor, want, strict)`——在提交/
  写路径进入 await 之前预留 ts 前沿：leader 可达则按 cursor 覆盖判定（纯函数
  `tail_covers`），不可达时 `strict=true`（写入跨越远端 slot 属主，即走 2PC）直接拒绝，
  `strict=false`（纯本地写）保留 GAP 降级（单调性优先于严格有序），同节点 refill 会
  重新锚定。
- `src/sql/tx/ts.rs`：Oracle 透传层把预约失败映射为 `ErrorCode::WriteConflict`
  （MySQL 1213，客户端可重试，复用既有重试管道）。
- `src/sql/dist/mod.rs`：`row_probe(table_id, pk)` + `any_remote_owner(shared, keys)`——
  用行平面 slot 探测写入集是否有远端属主，决定 strict 与否；含两组单测。
- 调用点：`tx/session.rs` commit_inner、`exec/write.rs` INSERT/UPDATE/DELETE、
  `exec/ddl.rs` backfill_index（补上此前缺失的预留）。
- 单测：`global_tests.rs` 新增 unreachable follower、strict 拒绝 GAP、lenient 保留降级
  三组；既有 4 组预留测试改为显式 `strict=false` 断言。

### c74def1 — CLUSTER INIT 种子 `sql_nodes`（region-metadata 风格）

- `src/command/cluster.rs`：`cluster_init` 在同一 `+done` 应答窗口内追加第二条 raft
  apply，把 leader binds（`NodeBinds`，与 main.rs 装配一致）直接种进 `sql_nodes`
  registry——FSM 条目复制的瞬间 peers 即可解析 ts authority。种子失败不令 INIT 失败
  （3s 注册循环兜底重覆盖）；`merged_registry` 命中即跳过，天然幂等。副作用：
  `cluster info` epoch 由 11 → 12（两次 apply 各计一次）。
- `src/sql/tx/nodes.rs`：抽取 `register_round`，`spawn_register` 在 3s 循环之前以
  250ms 快轮询 `cluster_ready` 边沿、立即注册一轮。

## 验证

- `cargo fmt` / `build` / `clippy --workspace --all-targets -- -D warnings` 全绿。
- `cargo test --lib sql` 333 通过；`--lib` 全量 881 通过；`command::cluster` 15 通过。
- e2e：`sql_2pc_e2e` 4×5 轮全过（首轮曾现一次冷启动失败，未复现）；`sql_txn_e2e` 8；
  `sql_oracle_cluster_e2e` 2；`sql_dist_read_e2e` 2；`process_cluster_e2e` 5（含新增
  `cluster_init_seeds_leader_binds_in_sql_nodes`）。
- **场景如实记录**：本机（安静负载）上 mysql_orders 场景修复前 f75c617 与修复后
  c74def1 均 5/5 PASS——未复现确定性的 FAIL→PASS 转变（历史失败依赖宿主机负载 ~140
  的时序窗口），本轮场景结果仅作回归覆盖，正确性证明由单测与 e2e 承担。全套
  run_all.sh 4/4 PASS。

## 补记：显式事务 strict 探测集并入索引键（复审发现）

复审（2026-09-14）发现 Part A 在显式事务路径上不闭合：`commit_inner` 的 strict
判定只探测**行键**，但决定是否 2PC 的 `dist::plan::build` 同时按各自 slot 路由
唯一索引预约与二级索引 ops（索引 slot = crc16(table_id++col_pos)，与行 slot 属主
可不同）。「行键全本地、索引键属远端」的显式事务因此拿到 `strict=false`，leader
不可达时照样以 GAP 巨型 ts 走 2PC 提交——恰是本修复要消灭的 bug 类别（三个
autocommit 路径与 backfill 探测集本就正确，仅此一处不一致，且原注释误称
"the same keys plan::build routes"）。

修复：`written_schemas`/`commit_index_ops` 提至预留之前（纯读，`try_plan_txn`
内部重算 idx 可容忍重复计算；本地路径复用提升后的结果、计算次数不变，2PC 路径
idx 由 1 次变 2 次——probes 一算 + `try_plan_txn` 内部一算），probes 并入 idx 键，
与 `exec/write.rs` 既有模式对齐；修正注释。

回归测试：`session_index_tests::commit_fails_fast_when_only_the_index_plane_is_remote`
（行本地/索引远端/authority 不可达 → 1213 且零落盘；修复前红、修复后绿）。
验证：`cargo test` 全量通过（lib 882 + 全部集成/e2e 套件 0 失败），
`clippy --all-targets` 无告警。

知悉项（2026-09-15 复审裁决非阻断）：错误序微变——提升后的 `commit_index_ops`
唯一冲突（`DupEntry`，MySQL 1062）现先于 strict 预留的不可达否决（1213）浮出：
「2PC + authority 不可达 + 唯一冲突」同现时客户端改见 1062 而非 1213，两条路径
均为零落盘的干净中止（等价于更早失败，语义无回归）。
