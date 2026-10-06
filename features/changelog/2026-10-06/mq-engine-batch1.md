# MQ Batch 1：DLQ+MAXDELIVERY、自动重投、XTRIM MINID、kafka headers 真回放、RENAME 账本随搬

Commit: 4259d92（DLQ/自动重投/MINID+账本守卫）、8b23b56（RENAME 随搬 0x20 账本）、370e654（kafka headers 真回放）、90e8c9b（场景用例+文档）；Batch 1.5 评审修复：fda8098（DLQ 水位提交后结算、sweep 批作废）、80a3bc2（headers 存储形态字节精确）、d043311（组语义：DLQ 校验/有序接管/续读游标/DESTROY 折叠账本）

## 背景
MQ 能力差距计划（`plans/2026-10-06-mq-gap/`，见 `00-gap-matrix.md` 复核结论）把
"一个可用的 MQ"的剩余缺口全部判为引擎级。Batch 1 落地 WP1 引擎可靠性全部五项
（`01-engine-reliability.md`）+ WP3 kafka parity 的 headers 回放缺陷修复
（`03-kafka-parity.md` scope one）：毒消息出口（DLQ）、无人值守重投、时间窗留存
（MINID）、两处存储一致性缺口（RENAME 搬账本、XTRIM/XDEL 守卫）与 kafka 面
headers 丢失。

## 变更
1. **DLQ + MAXDELIVERY**（`src/lite/dlq.rs`、`group.rs`）：`XGROUP CREATE <s> <g>
   <id|$> [MKSTREAM] [MAXDELIVERY <n≥1>] [DLQ <name>]`（DLQ 须随 MAXDELIVERY，缺省
   目标字面量 `<stream>/dlq`，同 slot 可独立消费）。下一次交付会把 `times_delivered`
   推过 `n` 时不再投递，**单 WAL 批**原子完成三件事：PEL 行删除、组 committed 水位
   推进（语义等同 XACK，含连续前缀规则）、消息进 DLQ 流（原字段 + 溯源字段
   `__dlq_group`/`__dlq_consumer`/`__dlq_times`/`__dlq_src`）。判定在投递路径的流
   latch 内（XREADGROUP 重投/XCLAIM/XAUTOCLAIM/sweep），无后台扫描器，并发 claim
   不双转；ordered 组仅 PEL 头可转移。触发面接线 `read.rs`/`claim.rs`/`autoclaim.rs`。
2. **自动重投**（`src/lite/redeliver.rs`、`conf.rs`、`main.rs`）：配置
   `lite: redelivery_idle_ms: <ms>`（默认 **0 = 完全不启用**，无后台任务）。启用后
   200ms 轮转 sweep 从 kind-0x0E 窗口发现组（每轮 32 组 × 16 行），对 idle PEL 行
   执行 claim 原语（times+1 + delivered_ms 刷新，重投给当前消费者），越 MAXDELIVERY
   走 DLQ；ordered 仅头（`ordered::force_takeover`）。每轮 SYNC、latch `try_lock`，
   永不 park 在后台任务上。
3. **XTRIM MINID**（`src/lite/append.rs`）：`XTRIM <s> MINID [~|=] <id> [LIMIT <n>]`
   （Redis 对齐；`~` 被精确实现，两种 flag 后均可带 LIMIT），删除 id 严格小于
   `<id>` 的条目；`<ms>-0` 形态即时间窗留存。与 MAXLEN 正交。
4. **kafka 账本守卫**（`append.rs`）：有 0x20 组账本行的流拒绝 XTRIM/XDEL，文案
   `ERR stream <name> has committed consumer-group offsets; delete the groups first`。
5. **RENAME 搬账本**（`src/command/keys_core.rs` `move_family`）：STREAM_FAMILY 搬运
   增折 OFFSET_FAMILY 行——组账本随流迁移（守卫跟新名）；旧名 OffsetCommit 回
   error 3 `UNKNOWN_TOPIC_OR_PARTITION`（映射解析先于账本写入，无悬空行）。
6. **kafka headers 真回放**（`src/kafka/fetch_records.rs`、`produce.rs`）：Fetch 对
   produce 形状（恰一个 "h" 对 + 合法 headers JSON + 其余 k/v/`__null__` 各至多一次）
   还原**真 record headers**；exotic 形状回退 JSON envelope + 标记头
   `("rdb-envelope", null)`；`parse_headers_json` 为 `headers_json` 逆函数，存储格式
   与存量数据零改动。
- 指标（`monitor.rs`）：`rdb_lite_dlq_depth` gauge、`rdb_lite_messages{op="dlq"/
  "redeliver"}` counter。
- 文档同步：`features/mq-lite.md`（三个新规范小节）、`features/kafka-front.md`
  （headers 回放 + 引擎批次修复节 + 偏差清单销账）、`COMPAT.md`、
  `features/e2e-coverage.md`、`agents/rust/index.md`；场景脚本
  `scrtips/e2e_scenarios/scenario_lite_mq.sh` 增 (g) DLQ/MAXDELIVERY、(h) XTRIM
  MINID 两段。

## 验证
- 新增 5 个 e2e 文件共 **23 用例**：`tests/lite_dlq_e2e.rs`（7：原子三件套/重复 claim
  不双转/ordered 仅头/语法门/kill -9/`rdb_lite_dlq_depth`/默认无重投）、
  `lite_redeliver_e2e.rs`（5）、`lite_trim_minid_e2e.rs`（5，含账本守卫）、
  `kafka_headers_roundtrip_e2e.rs`（4）、`kafka_rename_ledger_e2e.rs`（2）。
- 回归门 15 套件（lite 全家 + kafka wire/produce/fetch/offsets/group/failover +
  kv/tx/flushdb_lite 等）全绿；`cargo test --workspace` 84 target 绿，唯
  sql_funcs_* 5 个失败为并行 sql 工作流在途改动，与本批无关。
- 场景脚本 `scenario_lite_mq.sh` 全量 PASS（(g)(h) 新段含真 redis-cli 驱动的
  DLQ 转移、DLQ 独立消费、MINID/LIMIT/时间窗断言）。

## 后续
Batch 2（`plans/2026-10-06-mq-gap/` `02`/`04`/`03` 余项）：XADD DELAY 前置选项 +
kind 0x1D 暂存与 due 扫描器、rocksmq HTTP wait_ms 长轮询/pending 可见性/token
鉴权、kafka ListGroups/DeleteGroups 与 SASL PLAIN。
