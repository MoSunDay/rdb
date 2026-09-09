Commit: (working-tree, 随本提交入库)

# Lite MQ 有序组收尾：XACK 唤醒满窗 BLOCK 读者；SETID×ORDERED e2e

## 背景
有序组批次（见 [mq-ordered-groups](./mq-ordered-groups.md)）留的两个可选跟进：
一是 `xack` 持久化推进组记录/删 PEL 行后**未通知流 meta 键**——每个影响窗口的
操作（XADD / XCLAIM / XAUTOCLAIM / XGROUP 族）都已通知，唯独 ACK 缺席；二是
缺少 SETID 回卷 × ORDERED 组的端到端覆盖。

## 变更摘要
- **XACK 通知 meta 键**（`src/lite/ack.rs`）：latch 段内、durable 批次提交成功后，
  与"确有变更"（水位推进或 PEL 行删除）同条件地 `wait::notify` 流 meta 键——
  满窗 park 的 ORDERED `>` BLOCK 读者在窗口释放（ACK 腾位）时被唤醒重校验，
  而非睡满整个 BLOCK 预算。无 waiter 的 notify 为 no-op；`park_wait.rs` 的
  两处唤醒来源注释同步补上 XACK。
- **新增 e2e**：
  - `tests/lite_e2e.rs::ordered_block_wakes_on_xack_over_wire`：跨连接回归——
    严格串行组满窗后 owner 以 BLOCK 15000 park，另一连接 `XACK` 腾位，5s
    读界内必须送达 1-2（整测以 30s 外层 timeout 兜底）。
  - `tests/lite_ordered_e2e.rs::ordered_setid_rewind_replays_under_ownership`：
    SETID 回卷双水位到 1-1（XINFO GROUPS 松断言）；**不释放队列所有权**
    （c2 仍被隔离空回）；owner 重投时已 pending 的 1-2 行被**重新归属**且
    `times_delivered` 沿旧值 +1（XPENDING 行断言 `:2`）；连续 ACK 1-2/1-3 后
    committed 推进到 1-3；重启断言恢复点停在 1-3 且仍有序独占。

## 语义边界
- SETID 只改写组记录（delivered/committed，**自身批次同步落盘**，重启断言无
  需等刷盘）：不动 PEL 行、不腾窗口、不释放所有权；回卷后重投沿用同一 owner，
  已 pending 行重投不重复计 pending，投递计数结转递增。
- 满窗 ORDERED BLOCK 读者若日志中已有越过 delivered 水位的积压，
  `wait_targets` park 前的裸扫会立即带数据返回、由读循环重校验——即当前实现
  里这类读者实际处于紧密重校验循环而非真 park，ACK 腾位后本就被立刻拾起。
  XACK 的 notify 使"窗口释放必唤醒"成为显式契约（与其余窗口操作对齐），
  该循环日后若改为真 park 也不会退化成睡满预算。
- 空唤醒（信号到了但无新数据）由读侧吸收：XREADGROUP 循环头重校验，
  普通XREAD 对空结果继续 park——XACK 的 notify 不可能让 BLOCK 的 XREAD
  给客户端回空数组。

## 关联
- 规范：[mq-lite.md](../../mq-lite.md)「有序消费组与 Kafka 校准语义」
- 前批：[changelog 2026-09-09 mq-ordered-groups](./mq-ordered-groups.md)
