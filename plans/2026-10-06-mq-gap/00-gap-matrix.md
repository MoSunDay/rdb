# MQ 能力完备差距矩阵（常用 MQ 功能 × Lite/Kafka/HTTP 三面 × 处置）

> 状态：landed（状态板见 `README.md`）
> 日期：2026-10-06
> 关联：01–06 各工作包文档

## 范围与读法

- 本文是 `2026-10-06-mq-gap/` 计划的 00 号总览：以"常用 MQ 功能清单"为尺，对
  三面做只读差距复核，产出分级处置矩阵；每条的落地细案、触发条件与测试设计
  分别在 `01`–`06` 号文档展开，本文不重复其细节。
- 三面定义：

| 面 | 载体 | 说明 |
| --- | --- | --- |
| Lite | RESP 动词面（`src/lite/`） | Redis Streams 风格动词（XADD/XREADGROUP/XTRIM 等），第一公民 |
| Kafka | wire 面（`src/kafka/`） | Kafka 线协议前置，存储与引擎复用 Lite MQ |
| HTTP | rocksmq 面（`src/rocksmq/`） | RocksMQ 风格 HTTP 前置，同一 `src/lite/` 引擎 |

- 优先级：A 级（P1）= 引擎可靠性 + 已登记缺陷；B 级（P2）= 常用语义补齐；
  C 级（P3）= 按需池（逐条触发条件在 `05-p3-pool.md`）。
- 佐证路径与行号以撰写时仓库快照为准；状态标记：❌ 缺失、⚠️ 部分/已登记未修、
  ✅ 已具备。
- 阅读路径：只要结论 → 本文 + `05`；引擎可靠性实施 → `01`；delay 实施 →
  `02`（含族删除登记强制项）；kafka 面 → `03`；HTTP 面 → `04`；测试与
  CI 排期 → `06`。

## 复核结论（矩阵前置说明）

1. **bench 不是缺口**：`bench/src/cli.rs:88-94` 已有 xadd / xreadgroup / xack 及
   kafka-prod / kafka-fetch workloads，压测面覆盖存在，从差距矩阵移除，
   不设工作包。
2. **键族删除是真实集成风险**：`src/ds/expire/mod.rs:69-90` 的
   `family_delete_entries` 按 kind 段**显式枚举**删除——STREAM_FAMILY
   `0x0C..=0x0F`，并显式折叠 OFFSET_FAMILY `0x20` 账本窗口（单一宽区间会
   吞掉 JSON/vectorset/search 记录，见 `src/ds/codec.rs` 注记）。因此 WP2 的
   延迟暂存 kind **必须登记进 `family_delete_ranges`**（`src/ds/codec.rs:194`），
   并同步核对 typed-roots 分类规则（`src/ds/codec.rs:274` 附近），否则
   XIDLE 整流 / RENAME / FLUSHDB 等族删除路径会**漏删暂存行**。此项已列为
   WP2（`02` 号文档）的**强制验收项**。
3. **headers 回放是回归点**：`src/kafka/fetch_records.rs` 现以 JSON envelope
   回放，`tests/kafka_produce_e2e.rs` 有既有 envelope 断言；修复后该断言将被
   改变，**须同步更新**，不能只加新测试。
4. **复核中发现的遗漏项已补入矩阵**：kafka produce timestamp 保留、
   CreateTopics/DeleteTopics、`kafka_auto_create_topics`、
   `rocksmq_max_connections` 配置 parity、消费限流（throttle 恒 0）、消费者
   注册行 GC、广播消费文档化模式、HTTP `/range` 回放与批量 ack。处置：前几项
   补入 C 级池（见下文），消费限流经复核归入"显式不做"（协议字段保留、不实现
   语义，见 `05`）。

### 键族删除机制速览（复核结论 2 的技术背景）

`family_delete_entries`（`src/ds/expire/mod.rs:69-90`）不做跨家族宽区间删除：
kind 字节排序在键字节之前，一个 `0x0C..=0x20` 的宽区间会吞掉**其他家族**
（JSON / vectorset / search）的记录，因此按家族显式枚举 kind 段，流删除时再
把 OFFSET_FAMILY 窗口折叠进来。与本计划相关的 kind / 家族常量：

| 常量 | kind 值 | 佐证 | 与本计划的关联 |
| --- | --- | --- | --- |
| `KIND_STREAM_META` | `0x0C` | `src/ds/codec.rs:39` | STREAM_FAMILY 段首 |
| `KIND_STREAM_PEND` | `0x0F` | `src/ds/codec.rs:42` | PEL 行：DLQ 转移时删除的对象 |
| `KIND_STREAM_OFFSET` | `0x20` | `src/ds/codec.rs:64` | committed-offset 账本：RENAME 搬运对象、守卫探测对象 |
| `STREAM_FAMILY` | `0x0C..=0x0F` | `src/ds/codec.rs:76` | `family_delete_ranges` 枚举段 |
| `OFFSET_FAMILY` | 单 kind `0x20` | `src/ds/codec.rs:79` | 流删除时显式折叠的第二窗口 |
| （预留位） | `0x1D` | 当前未占用 | WP2 延迟暂存 kind 候选 |

登记动作清单（WP2 强制验收项展开）：新 kind 进 `family_delete_ranges`
（`src/ds/codec.rs:194`）的所属家族段；typed-roots 分类规则
（`src/ds/codec.rs:274` 附近）能识别新 kind；RENAME 的 `move_family`
（`src/command/keys_core.rs:377,398`）搬运段覆盖暂存行。三者缺一即漏删/漏搬。

**工程约束提示**：`src/lite/read.rs` 786 行（贴近 800 行迭代上限）、
`src/rocksmq/api.rs` 399 行。WP1/WP2 触碰 lite 引擎、WP4 触碰 rocksmq 面，
任何增量前先按职责拆分文件，保持每文件在限内。

## A 级矩阵（P1：引擎可靠性 + 已登记缺陷）

| 功能 | 现状（含佐证路径） | 方案 | 落点文档 | 验收 e2e |
| --- | --- | --- | --- | --- |
| 死信队列 + 最大投递次数 | ❌ `times_delivered` 仅作展示，不驱动任何投递策略（`src/lite/claim.rs`） | `XGROUP CREATE ... MAXDELIVERY <n> [DLQ <stream>]`；转移 = 同 latch 单 WAL 批（写 DLQ 流 + 删 PEL 行 + 推进连续前缀水位，语义等同 ack）；ordered 组仅 PEL 头可转；默认 DLQ 流名 `<stream>/dlq` | `01` | `lite_dlq_e2e.rs` |
| 自动重投调度 | ⚠️ 半自动：依赖客户端轮询 XAUTOCLAIM 驱动重投 | 引擎侧独立 sweep（复用既有 200ms 后台节奏模式）；新配置 `lite_redelivery_idle_ms`，默认 0 = 关闭；ordered 组走 `force_takeover` 头接管 | `01` | `lite_redeliver_e2e.rs` |
| 时间窗留存 | ❌ 仅 MAXLEN 条数截断，无时间窗语义 | `XTRIM <key> MINID [~\|=] <id> [LIMIT n]`（对齐 Redis 语义，兼作消息留存策略） | `01` | `lite_trim_minid_e2e.rs` |
| kafka headers 往返断裂 | ⚠️ produce 侧落为字段对 `"h"`，fetch 侧回放 JSON envelope（`src/kafka/fetch_records.rs`；既有断言在 `tests/kafka_produce_e2e.rs`） | 回放真 record headers；exotic 形状保留 envelope 兜底；同步更新既有 envelope 断言 | `03`（Batch 1 执行） | `kafka_headers_roundtrip_e2e.rs` |
| RENAME 不搬 0x20 账本 | ⚠️ 已登记未修（`features/kafka-front.md:267`）：`move_family` 不搬运 OFFSET_FAMILY 账本行 | `move_family`（`src/command/keys_core.rs:398`）增补 OFFSET_FAMILY 段搬运 | `01` | `kafka_rename_ledger_e2e.rs` |
| kafka 面流 XTRIM/XDEL 守卫 | ⚠️ 已登记未修、顺延中（`features/kafka-front.md:270`）：ordinal 稳定性风险仍在 | 探测到 0x20 账本引用即拒绝（有界扫描）；错误文案与普通错误区分 | `01` | `lite_trim_minid_e2e.rs`（含守卫拒绝用例） |

A 级共性：全部落在引擎/动词层，不新增面；其中两项（RENAME 账本、守卫）是
`features/kafka-front.md` 已登记欠账的**清偿**，不是新需求。

### A 级要点注记

1. **DLQ 转移语义**：三动作（写 DLQ 流、删 PEL 行、推进连续前缀水位）在同一
   latch 单 WAL 批内完成，对外语义**等同 ack**——客户端观察不到中间态，DLQ
   流自身是普通流（可 XREADGROUP 消费、可再转移）。`times_delivered`
   （`src/lite/claim.rs`）从展示字段变为策略输入。
2. **ordered 组约束**：保序组的前缀水位语义决定**仅 PEL 头**可转入 DLQ；
   中段洞穿会破坏有序投递假设，非头条目只能等待其前缀收敛。
3. **重投的兼容姿态**：`lite_redelivery_idle_ms` 默认 0 = 关，保持"客户端
   XAUTOCLAIM 驱动"的现行模型不变；开启后 sweep 复用既有 200ms 后台循环
   节奏（`src/lite/mod.rs` 后台循环），ordered 组接管走 `force_takeover`
   头，避免与客户端侧接管竞争。
4. **MINID 对齐范围**：`~`（近似删）与 `=`（精确删）两形态、`LIMIT <n>`
   上界都要与 Redis 钉平；与既有 MAXLEN 正交共存。时间窗留存的典型用法
   （"保留 N 天"）由 MINID + 定期调用或外部调度组合达成。
5. **headers 兜底边界**：常规 header 形状走真 record headers 回放；无法安全
   映射的 exotic 形状**保留 JSON envelope 兜底**（与现状兼容，不产生新错误
   路径）。`tests/kafka_produce_e2e.rs` 既有断言更新是交付物的一部分，
   不是附加项。
6. **守卫的探测成本**：0x20 账本引用探测为**有界扫描**（按 key 定界），
   不引入全库扫描；拒绝时的错误文案需与其他 trim 拒绝原因可区分，便于
   客户端自助判断"该流有消费者、不能裁"。

## B 级矩阵（P2：常用语义补齐）

| 功能 | 现状（含佐证路径） | 方案 | 落点文档 | 验收 e2e |
| --- | --- | --- | --- | --- |
| 延迟消息 | ❌ 无延迟语义 | `XADD <key> * DELAY <ms> field value ...` 前置选项；独立暂存 kind `0x1D`（当前未占用）+ due 扫描器（仿 `src/ds/expire/active.rs` 扫描器模式）；"删暂存 + 写目标流"同批原子 = 不双投；强制登记 `family_delete_ranges`（见复核结论 2）；HTTP 面 `delay_ms` 透传；Kafka 面刻意不暴露 | `02` | `lite_delay_e2e.rs` |
| HTTP 阻塞/长轮询 consume | ❌ HTTP consume 为立即返回式轮询，无等待语义 | `wait_ms` 参数映射到 XREADGROUP BLOCK，复用 `src/lite/park_wait.rs` 阻塞等待基建 | `04` | `rocksmq_wait_pending_e2e.rs` |
| HTTP pending 可见性 | ❌ HTTP 面无 pending 只读视图 | `POST /pending?channel&group`：XPENDING 摘要只读透传，不新增写路径 | `04` | 同上 |
| kafka ListGroups/DeleteGroups | ❌ 无组管理 API | 新 `src/kafka/admin.rs`；广告面 +2 API；DeleteGroups 清理对应账本行；重跑 `scrtips/e2e_scenarios/scenario_kafka_sdk.sh` | `03` | `kafka_admin_e2e.rs` |
| kafka 鉴权 parity | ❌ 无鉴权 | SASL PLAIN，配置键 `kafka_token`；401 / 认证失败矩阵进 e2e | `03` | 同上 |
| rocksmq 鉴权 parity | ❌ 无鉴权 | `rocksmq_token` Bearer（对齐 es / s3 前置的姿态）；测试形态参照 `tests/es_auth_e2e.rs` | `04` | 同上 |

B 级共性：均为"常用 MQ 客户端开箱即用"所需的最小面补齐；鉴权两键为
**配置键名**，默认不启用（空 = 关），不改变现有部署行为。

### B 级边界注记

1. **delay 的不双投**：到期投递 = "删暂存行 + 写目标流"**同一 WAL 批**，原子
   性由引擎批保证；到期前条目对消费面完全不可见（不是"可见但阻塞"）。
   HTTP `delay_ms` 只是参数透传，语义单点在引擎。
2. **wait_ms 的默认不变**：不传 = 现行立即返回行为；阻塞等待落在
   `src/lite/park_wait.rs` 既有基建上，HTTP 层只做参数透传与超时返回，
   不另造等待机制。
3. **pending 只读**：`POST /pending` 是 XPENDING 摘要的只读透传；ack / claim
   等写路径仍归 Lite 动词面，HTTP 面不新增写动词。
4. **admin 的状态一致性**：DeleteGroups 清账本行须与协调器状态机
   （`src/kafka/coordinator/state.rs` 的纯状态转移风格）保持同一套转移
   规则，不绕开 runtime 直接改存储。
5. **鉴权模型收窄**：两个 token 均为**单租户共享 token** 模型（配置键
   `kafka_token` / `rocksmq_token`），非多用户 ACL；未配置时行为与现状
   完全一致。401 / 认证失败矩阵进各自 e2e。
6. **配置落点**：新增配置统一进 `src/conf.rs`（`lite_redelivery_idle_ms`、
   两个 token 键等），沿用现有配置风格。

## C 级（P3 按需池）

以下条目**本计划不排期**，逐条触发条件与理由在 `05-p3-pool.md`（触发即从池中
取出单独立项，不改本矩阵）。

**2026-10-07 出池注记**：全池触发立项，经 `plans/2026-10-07-mq-p3-backfill/`
落地（摘要 `features/changelog/2026-10-07/mq-p3-backfill.md`；shas：91d78ff、
1619c8c、1d55e69、b8c3c10、17979dd，配置键 697b3b9）。行标注沿用本矩阵记号：

- CreateTopics / DeleteTopics / CreatePartitions —— ✅ 已落地（`src/kafka/
  admin_topics.rs` + `topic_store.rs`）
- `kafka_auto_create_topics` —— ✅ 已落地（默认 false 零行为变化）
- produce timestamp 保留 —— ❌ 悬置（#3 观望：唯一动存储格式项，再评条件 =
  出现按事件时间检索/回放的真实诉求，见 `05` §2 存留行）
- ListOffsets v2+ —— ✅ 已落地（v0–v5，v4+ leader_epoch 恒 -1）
- DescribeConfigs 桩 —— ✅ 已落地（静态最小集桩，`admin_configs.rs`）
- OffsetForLeaderEpoch —— ✅ 已落地（常量应答：epoch -1 + log 末端）
- XCLAIM TIME / RETRYCOUNT / IDLE —— ✅ 已落地（LASTID 仍不支持）
- XINFO FULL —— ✅ 已落地（`src/lite/xinfo_full.rs`）
- XADD NOMKSTREAM / LIMIT —— ✅ 已落地；**LIMIT 收缩**：语义 = 既有 XTRIM 参数
  （`src/lite/append_opts.rs` 共用解析器），已覆盖不造私有语法，台账
  `COMPAT.md`
- HTTP 批量 produce、批量 ack、`/range` 回放 —— ✅ 已落地（`src/rocksmq/
  batch.rs` + `range.rs`）
- `rocksmq_max_connections` —— ✅ 已落地（0 = 内建 4096，达上限静默拒纳）
- 消费者注册行 idle GC —— ✅ 已落地（三重判据 + kill -9 持久，默认关）
- 广播消费客户端模式文档化 —— ✅ 已落地（`features/mq-lite.md` 广播节）

## 显式不做

决策依据为 `features/mq-lite.md` 路线决策（final，不再复议）与三面语义边界；
逐条完整理由同样在 `05-p3-pool.md`：

| 条目 | 处置要点 |
| --- | --- |
| 事务消息 | 无 txn 语义基建，不在单机数据面承诺内 |
| 幂等 producer | 无去重账面，暂不引入 |
| ISR / 多副本 | 挂起待数据面复制能力，在 `05` 单列"C-挂起"类 |
| 优先级队列 | 现有队列/水位模型外能力 |
| 消息级 TTL | 由 A 级 MINID 时间窗留存替代 |
| 消息轨迹 | 可观测面另案，不混入本计划 |
| 增量 fetch session | 当前 Fetch 响应形态已满足目标客户端 |
| zstd | 压缩面按需，随 C 级评估 |
| 消费限流 / 配额（throttle 恒 0） | 协议字段保留 0 值，不实现限流语义 |
| TLS | 传输安全面另案 |
| 全量 Redpanda 兼容 | 以主流 Kafka 客户端面为准，不做全兼容承诺 |

## 三面覆盖视图

每项能力在哪面暴露、哪面刻意不暴露（"—"= 本计划不在该面新增暴露）：

| 功能 | Lite（RESP 动词面） | Kafka（wire 面） | HTTP（rocksmq 面） |
| --- | --- | --- | --- |
| DLQ / 最大投递次数 | `XGROUP CREATE` 参数 + 引擎转移 | — | — |
| 自动重投调度 | 引擎 sweep（配置开关） | — | — |
| 时间窗留存 | `XTRIM MINID` | — | — |
| kafka 面流 XTRIM/XDEL 守卫 | 守卫在动词层生效 | 正确性受益（ordinal 稳定） | — |
| RENAME 搬 0x20 账本 | `RENAME` 动词修复 | 正确性受益（账本随流迁移） | — |
| kafka headers 往返 | 不涉及（字段对天然往返） | Produce / Fetch 真 headers 往返 | 不涉及 |
| 延迟消息 | `XADD ... DELAY <ms>` | 刻意不暴露 | `delay_ms` 透传 |
| HTTP 长轮询 consume | 复用 XREADGROUP BLOCK 基建 | — | `wait_ms` 参数 |
| HTTP pending 可见性 | 复用 XPENDING 摘要 | — | `POST /pending` |
| ListGroups / DeleteGroups | — | +2 admin API | — |
| kafka 鉴权 | — | SASL PLAIN（`kafka_token`） | — |
| rocksmq 鉴权 | — | — | Bearer（`rocksmq_token`） |

刻意不暴露的原则：每面只暴露其协议受众期望的形态（如 delay 在 Kafka 面无
wire 语义，强行映射会破坏 Fetch 的 ordinal 稳定性假设），跨面只共享引擎层
的正确性修复（守卫、账本搬运）。

视图读法：

- **列出现即方案**：表中动词 / 参数均为 WP 文档定义的目标形态，非现状；
  现状见 A / B 级矩阵"现状"列。
- **"正确性受益"**：该面不新增 API / 参数，但因引擎层修复而消除了一个
  错误模式（RENAME 后账本悬空、trim 破坏 ordinal）。
- **"不涉及"**：该能力本就不经过此面（如 headers 是 Kafka wire 概念，
  Lite 字段对天然往返）。

## 处置映射与批次

> **易混淆点**：落点文档编号（01–05）是内容划分；执行批次（Batch 1–3）是
> **代码提交批次**，两者不同。

| 功能 | 落点文档 | 执行批次 |
| --- | --- | --- |
| DLQ / 最大投递次数 | `01` | Batch 1 |
| 自动重投调度 | `01` | Batch 1 |
| 时间窗留存（MINID） | `01` | Batch 1 |
| kafka 面流 XTRIM/XDEL 守卫 | `01` | Batch 1 |
| RENAME 搬 0x20 账本 | `01` | Batch 1 |
| kafka headers 往返 | `03` | Batch 1（`03` 中唯一进 Batch 1 的条目） |
| 延迟消息 | `02` | Batch 2 |
| HTTP 长轮询 consume | `04` | Batch 2 |
| HTTP pending 可见性 | `04` | Batch 2 |
| kafka ListGroups / DeleteGroups | `03` | Batch 2 |
| kafka 鉴权（SASL PLAIN） | `03` | Batch 2 |
| rocksmq 鉴权（Bearer） | `04` | Batch 2 |
| C 级全部条目 | `05` | Batch 3（按需）：2026-10-07 触发全池立项，经 `plans/2026-10-07-mq-p3-backfill/` 出池落地（#3 观望悬置） |

- Batch 1 = `01` 全部 + `03` 的 headers 回放缺陷修复，含 5 个新 e2e：
  `lite_dlq_e2e.rs`、`lite_redeliver_e2e.rs`、`lite_trim_minid_e2e.rs`、
  `kafka_headers_roundtrip_e2e.rs`、`kafka_rename_ledger_e2e.rs`。
- Batch 2 = `02` + `04` + `03` 其余，含 3 个新测试文件（`lite_delay_e2e.rs`、
  `kafka_admin_e2e.rs`、`rocksmq_wait_pending_e2e.rs`），覆盖 4 类验收点：
  延迟、kafka admin/SASL、rocksmq 长轮询+pending、token 鉴权。
- WP 之间**无代码依赖**，可并行开发；唯一顺序约束：**Batch 2 的 delay 依赖
  Batch 1 对 `family_delete` / `move_family` 路径的改造**（暂存 kind 的族删除
  登记与账本段搬运改造完成后，delay 才能安全落暂存行）。

### Batch 1：引擎可靠性 + headers 清偿

- 范围：`01` 全部 5 项 + `03` 的 headers 回放缺陷修复（`03` 中仅此条进
  Batch 1）。
- 交付物：5 个新 e2e（`lite_dlq_e2e.rs`、`lite_redeliver_e2e.rs`、
  `lite_trim_minid_e2e.rs`、`kafka_headers_roundtrip_e2e.rs`、
  `kafka_rename_ledger_e2e.rs`）+ `tests/kafka_produce_e2e.rs` 既有断言更新。
- 回归门槛：`tests/lite_ordered_e2e.rs`、`tests/lite_pel_e2e.rs`、
  `tests/flushdb_lite_e2e.rs`、`tests/kafka_offsets_e2e.rs` 全绿，
  `backup_surface_common` 备份只读面相等断言不回退。

### Batch 2：常用语义补齐

- 范围：`02` + `04` + `03` 其余（ListGroups/DeleteGroups、SASL）。
- 前置：Batch 1 已合入（delay 的族删除登记依赖，见上）。
- 交付物：3 个新测试文件（`lite_delay_e2e.rs`、`kafka_admin_e2e.rs`、
  `rocksmq_wait_pending_e2e.rs`），覆盖 4 类验收点：延迟、kafka
  admin/SASL、rocksmq 长轮询+pending、token 鉴权；
  `scrtips/e2e_scenarios/scenario_kafka_sdk.sh` 重跑通过。
- 回归门槛：`tests/kafka_group_e2e.rs`、`tests/kafka_fetch_e2e.rs`、
  `tests/kafka_wire_e2e.rs`、`tests/es_auth_e2e.rs` 形态参照项全绿。

### Batch 3：按需

- `05` 池内条目逐条按触发条件评估；触发即从池取出、另立计划文档，不回改
  本矩阵。
- 2026-10-07 落地注记：全池触发，另立 `plans/2026-10-07-mq-p3-backfill/` 执行
  完毕（C 级行标注见上节）；池内仅存 #3 观望行。

## 验收基线与文档同步（指向 06）

- 新增 8 个 e2e 文件（上两节已列）与既有回归的关系、逐用例设计在
  `06-e2e-matrix.md`。
- 既有回归不得回退：`tests/lite_ordered_e2e.rs`、`tests/flushdb_lite_e2e.rs`、
  `tests/lite_pel_e2e.rs`、`tests/kafka_produce_e2e.rs`、
  `tests/kafka_fetch_e2e.rs`、`tests/kafka_group_e2e.rs`、
  `tests/kafka_offsets_e2e.rs`、`tests/kafka_wire_e2e.rs`，以及
  `backup_surface_common`（备份只读面相等断言）；场景脚本
  `scrtips/e2e_scenarios/scenario_lite_mq.sh`、
  `scrtips/e2e_scenarios/scenario_kafka_sdk.sh` 保持可跑。
- 落地后同步文档面：`features/e2e-coverage.md`、`features/mq-lite.md`、
  `features/kafka-front.md`、`features/rocksmq-http.md`、`COMPAT.md`、
  `agents/rust/index.md`，摘要按 `features/changelog/` 的
  `YYYY-MM-DD/{topic}.md` 约定归档（`features/kafka-front.md` 中两条已登记
  欠账清偿后须同步改写登记行）。
