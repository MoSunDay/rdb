Commit: (working-tree, 随本提交入库)

# Lite MQ 有序组收尾：XACK 唤醒满窗 BLOCK 读者；SETID×ORDERED e2e

## 背景
有序组批次（见 [mq-ordered-groups](./mq-ordered-groups.md)）留的两个可选跟进：
一是 `xack` 持久化推进组记录/删 PEL 行后**未通知流 meta 键**——每个影响窗口的
操作（XADD / XCLAIM / XAUTOCLAIM / XGROUP 族）都已通知，唯独 ACK 缺席；二是
缺少 SETID 回卷 × ORDERED 组的端到端覆盖。

## 变更摘要
- **门控停靠（本批修复）**（`src/lite/park_wait.rs` / `src/lite/read.rs` /
  `src/lite/ordered.rs`）：满窗/被 fence 的 ORDERED `>` BLOCK 读者此前在
  `wait_targets` 的 park 前裸扫里每轮都命中积压、立即带数据返回，实为
  紧密重校验循环（BLOCK 0 烧满一核；XACK/接管 notify 在该路径上是死代码）。
  现在这类目标以 `gated` 标记停靠：**不参与数据扫描**（越过水位的积压正是
  背压本身），改为**注册 → 门控复查 → park**（复查与既有"先注册后终读"
  同形，关闭"循环头与注册之间发生腾位/接管、notify 落在无 waiter 键上"的
  丢通知竞态）；且存在 gated 目标时 park 切片加 cap 到**租约粒度**——租约
  过期是被动事件（无任何 notify），被 fence 的读者按租约切片重试接管而非
  睡满预算。门控谓词 `ordered_gate_closed` 与 deliver_new 同源（窗口算术 +
  `ordered::lease_live`/`ownership_open` 与 acquire 的过期判定逐位一致，无
  acquire 副作用）；复查谓词只读缓存与所有权表（无盘 IO、无 latch）。
  每轮循环头重算 gated 标记（跨轮不缓存）。
- **XACK 通知 meta 键**（`src/lite/ack.rs`）：latch 段内、durable 批次提交成功后，
  与"确有变更"（水位推进或 PEL 行删除）同条件地 `wait::notify` 流 meta 键——
  满窗 park 的 ORDERED `>` BLOCK 读者在窗口释放（ACK 腾位）时被唤醒重校验，
  而非睡满整个 BLOCK 预算。无 waiter 的 notify 为 no-op；`park_wait.rs` 的
  两处唤醒来源注释同步补上 XACK。（门控停靠落地后，本 notify 由死代码变为
  生效路径。）
- **新增 e2e**：
  - `tests/lite_e2e.rs::ordered_block_wakes_on_xack_over_wire`：跨连接回归——
    严格串行组满窗后 owner 以 BLOCK 15000 park，另一连接 `XACK` 腾位，5s
    读界内必须送达 1-2（整测以 30s 外层 timeout 兜底）。
  - `tests/lite_ordered_e2e.rs::ordered_setid_rewind_replays_under_ownership`：
    SETID 回卷双水位到 1-1（XINFO GROUPS 松断言）；**不释放队列所有权**
    （c2 仍被隔离空回）；owner 重投时已 pending 的 1-2 行被**重新归属**且
    `times_delivered` 沿旧值 +1（XPENDING 行断言 `:2`）；连续 ACK 1-2/1-3 后
    committed 推进到 1-3；重启断言恢复点停在 1-3 且仍有序独占。
  - `tests/lite_ordered_e2e.rs::full_window_block_reader_wakes_on_ack` /
    `fenced_block_reader_takes_over_on_lease_expiry`（44317/44318）：进程内
    专用线程跑阻塞 `call`（park 为 Condvar 停靠，不依赖 tokio timer）——
    前者证明满窗 owner 真 park（300ms 内无回复）且由 XACK notify 唤醒送达
    （5s 界内）；后者设 150ms 租约，c2 被隔离且**此后无任何命令**，只能靠
    租约粒度切片自行醒来接管并送达 1-2（无切片 cap 则睡满 20s → 测试必挂）。

## 语义边界
- SETID 只改写组记录（delivered/committed，**自身批次同步落盘**，重启断言无
  需等刷盘）：不动 PEL 行、不腾窗口、不释放所有权；回卷后重投沿用同一 owner，
  已 pending 行重投不重复计 pending，投递计数结转递增。
- 满窗/被 fence 的 ORDERED BLOCK 读者现在**真正 park**（此前的潜在缺陷已修）：
  门控目标不参与"数据落地"扫描（积压即背压，扫必命中、命中即旋转）；唤醒源
  为流 meta 键信号（XADD/XACK/接管/SETID/DESTROY）**加租约粒度切片**（租约
  过期无信号，被 fence 者按租约重试接管）；每次注册后先做门控复查（只读缓存
  + 所有权表），关闭"腾位/接管事件落在注册之前、notify 无 waiter 可达"的
  丢通知竞态。XACK/接管 notify 因此由死代码变为生效路径。普通（非有序）组的
  BLOCK 读者保持纯"数据落地"停靠语义，不受影响。
- 空唤醒（信号到了但无新数据）由读侧吸收：XREADGROUP 循环头重校验，
  普通XREAD 对空结果继续 park——XACK 的 notify 不可能让 BLOCK 的 XREAD
  给客户端回空数组。

## 关联
- 规范：[mq-lite.md](../../mq-lite.md)「有序消费组与 Kafka 校准语义」
- 前批：[changelog 2026-09-09 mq-ordered-groups](./mq-ordered-groups.md)
