# MQ 能力完备差距计划 — 状态板（2026-10-06）

> 状态：landed（A/B 级车道全部落地；C 级按需池已于 2026-10-07 经
> `plans/2026-10-07-mq-p3-backfill/` 全池出池落地，仅 #3 观望悬置，见下）
> 日期：2026-10-06（立项）/ 2026-10-07（收口）
> 本文是本计划目录的唯一状态入口：00–06 号文档为立项时的实施细案，内容不再随
> 落地回写（除状态行外）；落地事实以 changelog 为准 —— Batch 1：
> `features/changelog/2026-10-06/mq-engine-batch1.md`；Batch 1.5 + Batch 2：
> `features/changelog/2026-10-06/mq-batch2.md`；P3 回填：
> `features/changelog/2026-10-07/mq-p3-backfill.md`。

## 工作包/批次状态表

| 车道 | 文档 | 批次 | 状态 | 落地 commit | 验收 |
| --- | --- | --- | --- | --- | --- |
| WP1 引擎可靠性（DLQ+MAXDELIVERY/自动重投/XTRIM MINID/两缺陷） | `01` | Batch 1 | landed | 4259d92、8b23b56、90e8c9b | `mq-engine-batch1.md`（23 用例） |
| WP3 kafka headers 真回放（scope one） | `03` | Batch 1 | landed | 370e654 | 同上 |
| — Batch 1 评审修复（引擎收口/组语义/headers 字节保真/覆盖补齐） | `01`/`03` | Batch 1.5 | landed | fda8098、80a3bc2、e39fbd2、09628ff、d043311、17d1eb9、6c1ddc6 | `mq-batch2.md`（+11 用例） |
| WP2 延迟消息（XADD DELAY / 0x1D 暂存 / due 扫描 / 族登记） | `02` | Batch 2 | landed | 5ae5f07 | `mq-batch2.md`（12 用例） |
| WP4 RocksMQ HTTP 对齐（wait_ms/`/pending`/delay_ms/token） | `04` | Batch 2 | landed | fb77a24 | 同上（4 用例） |
| WP3 kafka 组管理 + SASL（ListGroups/DeleteGroups/PLAIN） | `03` | Batch 2 | landed | 59ed6eb | 同上（4 用例 + SDK 场景 6/6） |
| WP-验收 e2e 总表 | `06` | — | landed | 随各批次 | 全部用例落地（含 Batch 1.5 补齐的 4 个触发/进程级文件） |
| P3 按需池 | `05` | Batch 3（2026-10-07） | landed（14/15 出池；#3 观望悬置） | bbcd728（立项）、697b3b9、91d78ff、1619c8c、1d55e69、b8c3c10、17979dd | `mq-p3-backfill.md`（+59 用例，1780→1839） |

注：WP2 的"复用预约 id 交换"细案在落地时改为**交换追加全新 id**（预约 id 可能低于
组 delivered 水位而永久不可见，`src/lite/delay.rs` ID POLICY 有完整决策记录），
XADD 回复 id 语义为预约令牌——这是与 `02` 号文档的唯一实质偏差。

## 剩余未做（open 列表）

- **P3 按需池仅存 #3（produce timestamp 保留）观望**：2026-10-07 全池触发立项，
  #1/#2/#4–#15 经 `plans/2026-10-07-mq-p3-backfill/` 落地（出池注记在
  `05-p3-pool.md` §2）；#3 是唯一动存储格式的条目，再评条件 = 出现按事件时间
  检索/回放的真实诉求。
- `05` §4 的**显式不做清单**维持终局不变。
