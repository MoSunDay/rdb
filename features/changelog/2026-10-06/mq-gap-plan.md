# MQ 能力差距完备计划落盘（plans/ 目录启用，纯文档批）

Commit: 9fbf118（纯文档批，无代码/配置/行为变更）

## 背景
常用 MQ 功能 × Lite/Kafka/HTTP 三面差距复核完成。关键复核结论：bench 已有
xadd/xreadgroup/kafka-* workloads 非缺口；`src/ds/expire/mod.rs:69-90` 键族
枚举删除是新增存储 kind 的真实集成风险（漏登记则 XIDLE/RENAME/FLUSHDB 漏删）；
kafka headers 真回放将改变 `tests/kafka_produce_e2e.rs` 既有断言。据此产出
分批实施计划。

## 变更
- 新增 `plans/index.md`：plans/ 目录约定（proposed→accepted→landed/archived；
  落地后摘要归档本 changelog，未做项回写按需池；每份 <400 行）。
- 新增 `plans/2026-10-06-mq-gap/`（7 份，均 <400 行）：
  - `00-gap-matrix.md`：A/B 级差距矩阵（含佐证路径）+ 三面覆盖 + 批次映射；
  - `01-engine-reliability.md`（WP1）：DLQ+MAXDELIVERY、自动重投（默认关）、
    XTRIM MINID、RENAME 搬 0x20 账本、kafka 面流守卫；
  - `02-delay-messages.md`（WP2）：XADD DELAY、暂存 kind 0x1D、
    `family_delete_ranges` 强制登记（P0 回归项）；
  - `03-kafka-parity.md`（WP3）：headers 真回放、ListGroups/DeleteGroups、
    SASL `kafka_token`；
  - `04-http-parity.md`（WP4）：wait_ms 长轮询、`POST /pending`、
    `rocksmq_token` Bearer；
  - `05-p3-pool.md`：15 条按需池（逐条触发条件）+ 显式不做清单；
  - `06-e2e-matrix.md`：8 个新 e2e 文件总表 + 回归门 + 验收命令。
- 执行批次：Batch 1 = WP1 全部 + WP3 headers 回放（5 个 e2e）；Batch 2 =
  WP2 + WP4 + WP3 其余；Batch 3 = 按需池。**均未开工（状态 proposed）**。

## 影响面
- 零代码改动；现有能力文档（features/*、agents/rust/）不变——计划中能力
  未实现，稳定层不得记为现状。
- 落地各批次时须同步：`features/mq-lite.md`、`features/kafka-front.md`、
  `features/rocksmq-http.md`、`features/e2e-coverage.md`、`COMPAT.md`、
  `agents/rust/index.md`，并在本 changelog 追加对应批次条目。

## 关联
- 计划入口：`plans/index.md`；总纲：`plans/2026-10-06-mq-gap/00-gap-matrix.md`
- 决策依据：`features/mq-lite.md`（final 决策）、`features/kafka-front.md`
  （:267/:270 已登记缺陷，由 WP1 销账）
