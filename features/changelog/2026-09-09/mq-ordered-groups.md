Commit: (working-tree, 随本提交入库)

# Lite MQ 有序消费组 + Kafka 校准提交语义（P0/P1/P2）

## 背景
按 Kafka 语义校准清单收口 Lite MQ 顺序语义：P1 提交语义（committed offset =
连续前缀）、P0 顺序消费（队列独占所有权 + 有序投递 + INFLIGHT 旋钮）、
P2 有序接管（只认 PEL 头 + epoch 隔离）。P3（同 key 同队列，`XPICK pick_hash`）
前批已落地，本批不变。

## 变更摘要
- **P1 连续前缀提交**（`rust/src/lite/`）：
  - `pel.rs`：`first_pending` / `head_after_ack`（从 `succ(committed)` 起探查
    PEL 首条幸存行，上限 `max_acked`）；`PendState` 增 `epoch`（serde default，
    兼容旧记录）。
  - `offset.rs`：`ack` 改造为前缀语义（候选 = 首条幸存 pending 行以下的
    最大已 ACK id）；`GroupState` 增 `ordered` / `inflight_max`，`load` 时从
    PEL 精确重算 `pending` 并归一 inflight；刷盘批次携带新字段。
  - `ack.rs`：latch 下计算 `head_after`，**仅水位实际推进时**同步持久化组
    记录（kind 0x0E）；跨洞 ACK 不记忆、重投兜底（at-least-once）。
- **P0 有序消费组**：
  - `group.rs`：`XGROUP CREATE ... [ORDERED [INFLIGHT <n>]]` 选项解析
    （INFLIGHT 依赖 ORDERED；`model.rs` `normalize_inflight` 最小归一为 1）；
    DESTROY/DELCONSUMER 释放所有权。
  - 新增 `ordered.rs`：所有权表（OwnerMap：流×组 → Owner{consumer, epoch,
    lease_ms}）、`acquire`（含租约过期接管）/`force_takeover`/`release`/
    清理钩子；默认租约 30s，测试钩子 `set_lease_ms`。
  - `read.rs`：ORDERED 组投递门控（窗口 = `inflight_max - pending`，满窗/
    非所有者空回，BLOCK 读者 park；接管唤醒流 meta 键）；PEL 行盖
    所属 epoch 戳。
- **P2 有序接管只认头**：`claim.rs` / `autoclaim.rs` —— ORDERED 组只从
  PEL 头转移所有权（min-idle 预检失败不翻转所有权；FORCE 越头抑制；
  XAUTOCLAIM 单轮只认领头一行），成功接管 epoch 递增 + 唤醒等待者。
- **可观测性**：`XINFO GROUPS` 7 对字段（name / last-delivered-id /
  committed-id / ordered / inflight / owner / epoch）。
- **清理路径**：FLUSHDB（`ordered::clear`）、流空闲回收、组销毁、消费者
  删除同步清理所有权。
- **测试**：新增 `tests/lite_ordered_e2e.rs`（6 例：严格串行默认与 INFLIGHT
  旋钮、独占所有权与空闲迁移/epoch 隔离、XCLAIM 只认头、XAUTOCLAIM 只认头、
  连续前缀提交 + 重启重投尾部、有序配置跨重启存活）；`offset.rs` 单测改写；
  `lite` 单测 31 例全绿。

## 语义边界
- 所有权为内存态：重启随连接一起消失（无跨进程僵尸）；不落盘、不复制。
- 满窗口不靠 `>` 迁移（卡死工作走 XCLAIM/XAUTOCLAIM）；空闲租约到期才由
  下个申领者接管。
- 越过空隙的 ACK id 不记忆：水位补齐时重投（重复而非丢失）。
- P3（`XPICK pick_hash` 同 key 同队列）不变。

## 关联
- 规范：[mq-lite.md](../../mq-lite.md)「有序消费组与 Kafka 校准语义」
- 偏差总表：[COMPAT.md](../../../COMPAT.md) Lite Mode 条目
