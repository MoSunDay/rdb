# 分布式事务 2PC 契约修复与全仓缺陷清扫

## Context
对仓库做一轮正确性排查（2PC 分布式事务、WATCH 键空间、表达式求值、向量检索），
确认 5 个缺陷与 2 个同族实例，集中修复并补回归测试。

## Change Summary
- **2PC 提交 fail-closed**（`sql/dist/twopc.rs`）：协调者 outcome 持久化失败不再
  广播 commit Decide（原实现会照常广播，造成协调者未记录/参与者已提交的分叉）；
  改为尽力广播 abort 并向客户端返回可重试的 WriteConflict。决议序列化改为可失败，
  `broadcast_decides` 拆出消除递归 async。
- **status 按节点服务**（`sql/dist/recover.rs`、`participant.rs`）：`/sql2pc/status`
  与 `TxnStatus` 只返回请求节点名下映射的索引切片；`sweep_once` 改按自身 bind 取
  映射（原实现读 `own_ops`，协调者记录该字段为空 → 切片丢失）；参与者自身的
  `own_ops` 仅限本地重放，永不过线（原实现会把整份切片泄漏给任意请求节点，
  造成跨参与者索引重放）。
- **WATCH 键空间**（`tx/watch.rs`）：`USER_KINDS` 补 SEARCH 家族 `0x13..=0x18`，
  FT.* 写现在会正确 abort MULTI/EXEC。
- **表达式极值**（`sql/exec/expr.rs`）：Div/Mod 对 `i64::MIN / -1`、`i64::MIN % -1`
  采用 wrapping 语义，不再 panic。
- **ANN 排序**（`search/ann/mod.rs`）：NaN 距离用 `total_cmp` 排序，不再 panic。
- **回归测试**：`recover.rs` 新增 3 个 store-backed 测试（per-node 映射切片、
  own_ops 不外泄、status 按节点服务）；`tests_2pc.rs` 适配 decide 新签名；
  watch/expr/ann 各补 1 个回归。

## Impact Surface
- `sql/dist`：2PC 提交失败语义与 status 应答契约（行为契约变化，见 Notes）。
- `tx/watch`：WATCH 命中范围扩大（SEARCH 写）。
- `sql/exec`、`search/ann`：极值/异常输入不再 panic。
- 对 RESP 数据面、控制面、存储布局无影响。

## Notes / Compatibility
- `/sql2pc/status` 对非映射节点现在返回空 ops（原为泄漏参与者全量切片）——旧行为
  是缺陷而非兼容面；依赖它做跨节点重放的调用方本就在制造错误索引。
- commit outcome 落库失败时客户端从"假成功"变为 WriteConflict（可重试）；abort
  路径落库失败仍按原语义仅记日志（presumed-abort 兜底不变）。
- 验证备注：`tests/blocking_concurrency_e2e.rs` 的时延边界断言（600 并发 BLPOP
  打满 512 park 池后 SET < 10s、BZPOPMIN 5s 内唤醒）在共享高负载机器上可超时甚至
  挂起，同一测试二进制多次运行结果可在失败/挂起/通过间变化（负载 112 全量跑 6 例
  失败；隔离与负载 ~98 复跑全部通过，含 lib 799 全绿）。判定需隔离低负载复跑，
  不作为代码回归依据。
- 已知残留：commit 决议的"落库失败"路径缺直接单测（RocksDB 注入故障不现实），
  靠 persist gate 代码评审 + 既有套件兜底；"已提交但本节点无映射切片"时 marker-only
  提交会缺二级索引条目，属既有设计缺口，暂不在本轮范围。

## Related Docs
- [SQL 数据面](../../sql-dataplane.md)
- [rust/sql 模块地图](../../../agents/rust/sql.md)
- 契约：`COMPAT.md` "2PC writes" 节
