# AUTO_INCREMENT floor RMW 丢原子性：CATALOG_MUX 全程串行 + 死 API 清理

Commit: e159ccd

## 背景

8065e6e 把 DDL commit 改为 queue 化（守卫不跨 await）后，`exec::sequence::allocate`
的 floor 读-改-写丢了原子性：raft 写守卫窗口只剩"读 floor + assign + queue"，
commit await 在守卫之外；而 FSM（`live_kv`/`kv`）只见**已 apply** 的 bump，排队
未落地的 bump 对后续读不可见。于是两个并发 INSERT 可在 queue→apply 窗口内重读
同一个旧 floor、重发重叠 id——行存 PK 走静默 upsert 覆盖，等同丢行，属数据损坏级
缺陷（上方 `sequence.rs` 的旧契约注释"写守卫横跨 read+assign+raft apply"自 8065e6e
起即为失实描述）。

## 复现（确定性红）

单测（`sequence.rs` tests）注入真实 queue→apply 窗口：stub 装上 `apply_tx`，
后台 loop 对每条排队 entry 先 sleep 50ms 再写入 FSM 视图并回 oneshot。4 任务并发
（各 3 条单行 + 2 条双行 INSERT = 7 id，共 28）：未修复必现 `ids.len() == 7`
（`[1, 65, 129, 193, 194, 257, 258]`），28 个 id 里 21 个被静默覆盖。

## 修复

- `exec/ddl.rs`：`DDL_MUX` 改名 `pub(crate) CATALOG_MUX`，注释补明它同时串行
  `sequence::allocate` 的 floor RMW。
- `exec/sequence.rs` `allocate`：函数入口即 `CATALOG_MUX.lock().await`，横跨
  读 floor→queue→commit-await 全程（普通 tokio mutex 本就为跨 await 持有而设）；
  raft 写锁仍只在 read+assign+queue 瞬间持有、绝不跨 await（死锁规则不变，
  见 `catalog_apply`）。修复后复现单测绿。
- 重入检查：`allocate` 唯一调用方是 `write.rs` 的 INSERT 路径，
  `catalog_apply`/`catalog_txn` 体内不再下穿任何 allocate，无自锁。

## 清理（P2 死 API）

`storage/catalog.rs` 删除 `CatalogTxn::record`/`applied()`/`applied` 字段
（全仓零调用）；`queue_put`/`queue_entry` 注释改为如实描述：调用方在锁外 await
ticket 后自行重建 applied 列表用于复制 barrier（`exec::ddl` 手工重建即为唯一路径）。

## 新增哨兵

- 单测（确定性）：`concurrent_inserts_never_share_ids_when_apply_lags`，
  50ms 延迟 apply loop 下 4 任务 × 7 id 必须 28 个互异。
- 真进程 e2e：`auto_increment_e2e::concurrent_inserts_hand_out_unique_ids`，
  4 连接 × 10 条单行 INSERT = 40 个互异 id（race 窗口为 leader 上 queue→raft-apply；
  确定性复现以单测为准）。
