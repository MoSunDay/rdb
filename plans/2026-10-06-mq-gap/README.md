# MQ 能力完备差距计划 — 状态板（2026-10-06）

> 状态：landed（A/B 级车道全部落地；C 级按需池悬置，见下）
> 日期：2026-10-06（立项）/ 2026-10-07（收口）
> 本文是本计划目录的唯一状态入口：00–06 号文档为立项时的实施细案，内容不再随
> 落地回写（除状态行外）；落地事实以 changelog 为准 —— Batch 1：
> `features/changelog/2026-10-06/mq-engine-batch1.md`；Batch 1.5 + Batch 2：
> `features/changelog/2026-10-06/mq-batch2.md`。

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
| P3 按需池 | `05` | — | 悬置（按需） | — | 触发条件未满足，条目 1–15 全部保留 |

注：WP2 的"复用预约 id 交换"细案在落地时改为**交换追加全新 id**（预约 id 可能低于
组 delivered 水位而永久不可见，`src/lite/delay.rs` ID POLICY 有完整决策记录），
XADD 回复 id 语义为预约令牌——这是与 `02` 号文档的唯一实质偏差。

## 剩余未做（open 列表）

- **P3 按需池 15 条全部悬置**（`05-p3-pool.md` §2 总表）：kafka AdminClient 侧
  （1 建/删 topic、2 删记录、3 produce 时间戳、4 ListOffsets v2+、5
  DescribeConfigs 桩、6 OffsetForLeaderEpoch）、Lite 动词边角（7 XCLAIM
  TIME/RETRYCOUNT/IDLE、8 XINFO FULL、9 XADD NOMKSTREAM/LIMIT）、HTTP 批量与
  回放（10 批量 produce、11 批量 ack、12 `/range`）、面治理（13
  `rocksmq_max_connections`、14 消费者注册行 idle GC、15 广播消费文档化）。
  每条触发条件即立项开关，满足前不排期。
- `05` §4 的**显式不做清单**维持终局不变。
