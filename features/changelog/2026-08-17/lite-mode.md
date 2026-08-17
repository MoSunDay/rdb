Commit: df62640f13b0de23e340ee9abc00e443c2d3b401
# Lite Mode：RocketMQ 风格父主题 + 动态队列（Streams 动词）

## 背景 / Context
对齐 RocketMQ 5.5 Lite Mode 的使用形态：以“父主题”为寻址单元、队列动态增删、
消费组按已提交水位推进。rdb 以 Redis Streams 动词暴露该模型（XADD/XLEN/XRANGE/
XTRIM/XDEL/XIDLE/XREAD/XREADGROUP/XACK/XGROUP/XINFO/XPICK），定位为 rdb 扩展而非
Streams 模拟器，差异见 [COMPAT.md](../../../rust/COMPAT.md)。

## 变更摘要 / Change Summary
新增 `rust/rdb/src/lite/` 模块族（纯函数 + 显式 Runtime 传递，无类）：
- `mod.rs`：Lite 运行时装配与后台任务（组水位 200ms 刷盘 `spawn_background`）。
- `model.rs`：主题名解析与物理布局——slot 前缀取父主题名 CRC16，同族队列共置。
- `select.rs`：队列挑选 round_robin / hash / least_backlog。
- `append.rs`：XADD（裸父主题名自动选队列）/XRANGE/XTRIM/XDEL/XIDLE。
- `read.rs`：XLEN/XREAD/XREADGROUP（恰好单流；BLOCK 跨连接唤醒）。
- `ack.rs`：XACK 同步持久化组已提交水位。
- `group.rs`：XGROUP 创建/销毁/重置。
- `offset.rs`：组水位内存缓存 + 200ms 批量落盘（kind-0x0E 记录）。
- `info.rs`：XINFO（含 TOPICS/LITE 扩展）与 XPICK。
- `entries.rs`：条目扫描公共件。
- 空闲 TTL 复用统一过期信封（xadd 重置、xtrim/xdel 保留、xidle 设置），到期整流回收。

接线：
- 命令注册 `rust/rdb/src/command/mod.rs:105-116`；路由白名单 `rust/rdb/src/router.rs:65-76`
  （成员单测 `rust/rdb/src/router.rs:204-222`）。
- 状态装配：`rust/rdb/src/state.rs:296,318`、`rust/rdb/src/resp/mod.rs:73`、
  `rust/rdb/src/main.rs:314,343,349`、`rust/rdb/src/lib.rs:11`、`rust/rdb/tests/common/mod.rs:20`。
- 指标 `rust/rdb/src/monitor.rs:41-51`：`rdb_lite_messages{op=add|read|ack}`、
  `rdb_lite_streams{kind=live|reaped}`、`rdb_lite_offset_dirty`。

## 测试覆盖 / Test Coverage
| 测试 | 覆盖点 | 文件 |
|------|--------|------|
| xadd_autopick_xpick_and_info | 裸父主题自动选队列、XPICK 三策略、XINFO TOPICS/LITE | `rust/rdb/tests/lite_e2e.rs:16` |
| group_lifecycle_and_catchup | 组创建/消费/水位推进/重建组从 0 追平 | `rust/rdb/tests/lite_e2e.rs:61` |
| restart_resumes_from_committed_watermark | 进程内重启自已提交水位恢复，不重投已 ACK 条目 | `rust/rdb/tests/lite_e2e.rs:153` |
| idle_ttl_reaps_whole_stream | XIDLE 到期整流回收（条目+组状态），XADD 续活 | `rust/rdb/tests/lite_e2e.rs:208` |
| block_wakes_on_xadd_over_wire | XREAD BLOCK 跨连接被 XADD 唤醒（tokio 双连接） | `rust/rdb/tests/lite_e2e.rs:261` |
| lite_metrics_series_exposed | /metrics 暴露三条 Lite 序列 | `rust/rdb/tests/lite_e2e.rs:348` |
| xrange_bounds_count_xtrim_and_xdel | XRANGE 全程/- 与 `(` 排他、COUNT 截断、missing 空数组；XTRIM MAXLEN/`~` 裁剪后 XLEN；XDEL 命中/missing/非法 id | `rust/rdb/tests/lite_streams_e2e.rs:10` |
| process_kill9_restart_resumes | kill -9 后重启自已提交水位恢复（进程级） | `rust/rdb/tests/lite_proc_e2e.rs:11` |
| select 单测 ×3 | 策略解析/轮转回绕/空默认/hash 稳定 | `rust/rdb/src/lite/select.rs:128` |
| offset 单测 ×2 | ACK 计数与钳制/位置设置与移除 | `rust/rdb/src/lite/offset.rs:198` |

- 全量回归：`cargo test --workspace` → 240 passed / 0 failed（0 ignored）。
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → 零警告；`cargo fmt --check` 通过。
- 行数：新增文件均 ≤400（最大 `append.rs` 374、`lite_e2e.rs` 371、`lite_streams_e2e.rs` 89）；迭代文件均 ≤800
  （最大 `tests/common/mod.rs` 401）。

## Impact Surface
- 新增可感知命令面：XADD…XPICK（路由白名单内，本节点直答，物理前缀按父主题 slot）。
- 语义边界：XREAD/XREADGROUP 恰好单流；无 PEL，at-least-once（重启自已提交水位恢复）；
  XIDLE 为整流 TTL；XPICK / XINFO TOPICS / XINFO LITE 为 rdb 扩展。
- 不影响：既有 RESP 命令语义、Raft 控制面、MOVED 路由行为与既有 e2e；
  Lite 元数据不经 raft 复制（节点本地）。

## Related Docs
- [rust/COMPAT.md](../../../rust/COMPAT.md)
- [agents/rust](../../../agents/rust/index.md)
- [既有 Rust 重写 changelog](./rust-rewrite.md)
