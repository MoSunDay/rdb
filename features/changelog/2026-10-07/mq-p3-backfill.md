# MQ P3 按需池回填：topic admin / ListOffsets v2+ / lite 动词边角 / HTTP 批量与回放 / 消费者 idle GC

Commit：bbcd728（计划登记 docs(plans)）、697b3b9（W0 conf 三键）、91d78ff（ListOffsets
v2–v5）、1619c8c（topic admin 三件套 + DescribeConfigs/OffsetForLeaderEpoch + produce
auto-create）、1d55e69（lite：XADD 选项 / XCLAIM 提示 / XINFO FULL / 广播文档）、
b8c3c10（rocksmq 批量 + /range + 连接上限）、17979dd（lite 消费者 idle GC）；
本条目为 W4 台账收尾（出池三件套 + 文档面同步）。

## 触发与分诊
2026-10-07 用户指令「做好补齐的规划」触发 `plans/2026-10-06-mq-gap/05-p3-pool.md`
附录 A 登记（全池 #1–#15），按 §1.3 立项流程成文
`plans/2026-10-07-mq-p3-backfill/`（README 即总览）。分诊结论：**13 条立项进入
执行波次；#3（produce timestamp 保留）末位立项、本轮观望不实施**——它是唯一要动
存储格式的条目（新写带 t pair、旧行回退、by-ts 双时钟），再评条件 = 出现按事件
时间检索/回放的真实诉求；**#9 的 LIMIT 子句收缩澄清**：语义就是既有 XTRIM 的
LIMIT 参数（本轮逐字复核，`plans/2026-10-07-mq-p3-backfill/` §1 复核结论有证据
链），已覆盖、不造私有语法，台账在 `COMPAT.md` MQ 节注明。

## 落地明细（按池条目）
- **#1 topic admin 三件套**（1619c8c）：新 `src/kafka/admin_topics.rs`（wire）+
  `src/kafka/topic_store.rs`（存储编排：建分区 = parent 槽前缀下 `T/p<N>` 空流、
  排序 latch + 单批 fsync；删 = DEL/闲置收割同一条 family-delete 路径，折叠
  entries+meta、0x20 账本、0x1D 延迟行与嵌套 DLQ 流）。校验阶梯：名 → 副本分配
  （39）→ replication_factor 只收 1/-1（38，单节点无第二副本位）→ 分区数 -1=默认 1 /
  1..=10000（37）→ 存在性（36/3）；CreatePartitions 取新总数、收缩回 37；请求
  configs 解析后忽略。版本全 classic：CreateTopics(19) v0–v4、DeleteTopics(20)
  v0–v3、CreatePartitions(37) v0–v1。
- **#2 `kafka_auto_create_topics`**（697b3b9 键 + 1619c8c 接线）：bool，默认
  **false = 未知 topic 照旧回 error 3，零行为变化**；true 时 produce 先走
  `topic_store::ensure_topic` 建默认单分区。
- **#4 ListOffsets v2–v5**（91d78ff）：`src/kafka/offsets_query.rs`——v2 增
  isolation_level（解析忽略）+ throttle 首字段，v4 增 current_leader_epoch 解码与
  响应 leader_epoch（恒 -1 "unknown epoch"，KIP-320 客户端跳过截断；与 Fetch 同
  姿态）；v0–v1 字节不变；-3 max_timestamp 按 latest 应答。
- **#5 DescribeConfigs(32) v0–v3 桩**（1619c8c，`src/kafka/admin_configs.rs`）：
  TOPIC 资源回静态最小 Kafka 默认集（cleanup.policy=delete、retention.ms=604800000
  ——**常量而非活配置**、retention.bytes=-1、min.insync.replicas=1），其他资源
  类型 42 INVALID_REQUEST；无 AlterConfigs。
- **#6 OffsetForLeaderEpoch(23) v0–v3**（1619c8c）：常量应答——error 0 +
  leader_epoch -1（本 broker 全程不报 epoch）+ end_offset = log 末端（与 Fetch 高
  水位同源）。
- **#7 XCLAIM IDLE/TIME/RETRYCOUNT**（1d55e69，`src/lite/claim.rs`）：与 id 列表
  任意交错（Redis 文法；LASTID 仍不支持）；IDLE 回拨 delivered_ms、TIME 停墙钟、
  RETRYCOUNT 改写计数；JUSTID claim 同样落 PEL 写；XPENDING 读侧随之回归。
- **#8 XINFO STREAM FULL**（1d55e69，新 `src/lite/xinfo_full.rs`，分发留
  `info.rs`）：流/组/消费者三层视图；省略字段清单（radix-tree-keys/nodes、
  entries-added、max-deleted-entry-id、recorded-first-entry-id、组级
  entries-read/lag）与 `seen-time` 近似口径见 `features/mq-lite.md`。
- **#9 XADD NOMKSTREAM/LIMIT**（1d55e69，新 `src/lite/append_opts.rs`）：
  NOMKSTREAM 不建键回 nil；MAXLEN/MINID/LIMIT 与 XTRIM **共用**解析器与受害者
  计算，追加与修剪同批 fsync；LIMIT = XTRIM 参数语义（收缩结论，见上）。
- **#10–#12 HTTP 批量与回放**（b8c3c10，新 `src/rocksmq/batch.rs` + `range.rs`）：
  `/produce_batch`、`/ack_batch`（JSON 数组 ≤100，逐元素重组 Query 调单路由
  处理器、逐项 status+body、失败不中断）、`/range`（只读 XRANGE 回放，无 PEL/
  组副作用；延迟行到期前不可见）；照常过 token 门禁。
- **#13 `rocksmq_max_connections`**（697b3b9 键 + b8c3c10 接线，新
  `src/rocksmq/guard.rs`）：i64，0（默认）= 内建 4096（与 kafka front 同常量），
  负值回落；进程级计数 + RAII ConnGuard，达上限新连接**静默关闭**（不写 HTTP
  字节，stderr 一行），镜像 kafka 先例。
- **#14 消费者 idle GC**（17979dd，新 `src/lite/consumer_gc.rs`）：
  `lite.consumer_gc_ms`（u64，默认 **0 = 完全不启用**）。三重判据（无 PEL ∧ 无
  活跃租约 ∧ seen_ms 越阈值）缺一不可；删除复用 XGROUP DELCONSUMER 路径同步批写
  （kill -9 后已回收成员不重现）；与重投 sweep 游标互不干扰。节奏 1s/轮、每轮
  ≤32 组（复用 redeliver 轮转）——**慢于 flusher 200ms 的理由**：回收时延无观测面、
  阈值实际是分钟级、每轮要整扫一组 PEL（`src/lite/consumer_gc.rs` 头注）。有序
  owner 豁免是租约性的：租约在必保，租约过期的遗弃 owner 与普通成员同回收。
- **#15 广播消费文档化**（1d55e69，纯文档）：`features/mq-lite.md` 增「广播消费
  （broadcast）客户端模式」节——每消费者独立组名，组隔离下各自全量。

## 行为变更说明
- **唯一默认可见的 wire 面增量**：kafka ApiVersions 广告集 **15 → 20**（新增
  key 19/20/23/32/37，纯增量行；`kafka_token` 非空时 22，17/36 照旧）。存量
  客户端协商面不受影响（未用这些 key 的客户端零感知）。
- 新配置三键默认全部零行为变化：`kafka_auto_create_topics=false`（未知 topic 仍
  回 3）、`rocksmq_max_connections=0`（内建 4096，与原无限行为同量级上限）、
  `lite.consumer_gc_ms=0`（不 spawn GC 任务）。
- HTTP 新路由是新增面（`/produce_batch`、`/ack_batch`、`/range`），既有四路由
  字节不变；`/range` 只读无副作用。

## 验证
- 用例总数 **1780 → 1839（+59，只增不减）**：6 个新 e2e 文件（`kafka_topics_e2e`
  3、`lite_claim_opts_e2e` 7、`lite_xinfo_full_e2e` 5、`rocksmq_batch_range_e2e` 7、
  `lite_consumer_gc_e2e` 6、`lite_consumer_gc_proc_e2e` 4）+ 新公共 harness
  `tests/common/mq.rs` + wire/unit 新文件（`src/kafka/admin_topics_tests.rs`、
  `admin_configs_tests.rs`）+ 既有文件扩展（`kafka_wire_e2e` 广告面 20 行断言、
  `tests/common/lite.rs` 选项解析助手等）。
- 覆盖台账：`features/e2e-coverage.md` 新增「MQ P3 按需池回填」块；三面规范同步
  `features/kafka-front.md`（新「Topic 管理与常量应答」节）、`features/mq-lite.md`、
  `features/rocksmq-http.md`；偏差总表 `COMPAT.md`（含 #9 LIMIT 台账注记）。

## 偏差（对计划文档）
- **feat(lite) ×2 塌缩为 ×1**（1d55e69）：计划的提交切分是 append_opts/claim 与
  XINFO FULL/广播文档两刀，但 W1 的 L 车道两条并行线共享 `src/lite/mod.rs` 等
  接线 hunk，不可分割提交，合并为一刀（内容无变化）。
- **XINFO FULL 省略字段**：radix-tree 计数等 Redis 字段引擎无对应数据，宁缺毋假，
  清单见 `features/mq-lite.md`（COMPAT 同步）。
- **GC tick = 1000ms**（非 flusher 的 200ms）：理由见 #14 明细；每轮 ≤32 组。
- **W3 未排期**（按计划原文）：#3 观望不动存储格式；再评条件 = 真实 by-ts 检索/
  回放诉求出现。
- **场景脚本扩段**（`scenario_kafka_sdk.sh` admin 步、`scenario_lite_mq.sh`
  FULL/NOMKSTREAM 步）由并行收尾车道进行中，作为场景层回归入口另行合入。

## 后续
P3 按需池出池三件套同批落地（`00-gap-matrix.md` C 级行标注、`05-p3-pool.md` §2
出池注记、本条目）；池内仅存 **#3（观望，再评条件在案）**，`05` §4 显式不做清单
维持终局不变。计划 `plans/2026-10-07-mq-p3-backfill/` 推进 landed。
