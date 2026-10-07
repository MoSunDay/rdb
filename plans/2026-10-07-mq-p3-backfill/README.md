# 2026-10-07 mq-p3-backfill：P3 按需池全池补齐计划

> 状态：proposed
> 日期：2026-10-07
> 输入：`plans/2026-10-06-mq-gap/05-p3-pool.md` §2 按需池（条目 #1–#15）
> 关联：本文件即本计划总览（承担 `00` 号角色；按 `plans/index.md` 约定，单文档
> 计划允许总览直接以 README.md 承载，工作包不另拆号）

本文是 P3 按需池的**全池立项计划**：2026-10-07 用户指令「做好补齐的规划」触发
`05-p3-pool.md` 附录 A 登记，按 §1.3 立项流程成文。范围 = 池内 #1–#15 的分诊、
波次拆分、测试与提交切分、风险与验收门；与已落地的 mq-gap Batch 1/1.5/2
（A/B 级车道）正交，不重开其工作包。

## 0. 定位与触发登记

### 0.1 治理红线（引自 `05-p3-pool.md` §5.5）

> **无信号抢跑**：以"顺手""低风险"为由提前实现池内条目——实现可以，但必须先走
> §1.3 立项（哪怕当天完成），保证台账与 changelog 完整。

本计划即该立项动作本身：先登记、后分诊、再排波次，任何代码提交（含 W0 的
conf 键）都以本文合入为先决条件。

### 0.2 触发信号登记（附录 A 模板）

```
触发条目：#1–#15（全池）
信号原文：用户指令「做好补齐的规划」（2026-10-07 会话）
影响面：AdminClient 建删 topic / 新 SDK 版本握手 / HTTP 批量与回放 / 运维视图
诉求方期望：全池补齐规划并执行
分诊结论：13 立项；#3 末位立项（本轮观望不实施，再评条件：出现按事件时间检索/回放的真实诉求）；#9 LIMIT 语义收缩澄清
登记人 / 日期：OpenCoder 会话 / 2026-10-07
```

按 §5.4 三件套纪律：本计划合入时同步改 `plans/2026-10-06-mq-gap/00-gap-matrix.md`
C 级行、`05-p3-pool.md` 出池标注与 `features/changelog/` 记录（W4 收尾统一执行，
见 §2）。

## 1. 分诊表

15 条逐一给出结论；条目名与 `05-p3-pool.md` §2 总表逐字一致。

| # | 条目 | 分诊结论 | 处置要点 |
| --- | --- | --- | --- |
| 1 | CreateTopics / DeleteTopics / CreatePartitions | 立项 | AdminClient 建/删 topic 映射为显式建 parent 流 + N 队列；删走键族折叠（见 §4 风险行） |
| 2 | `kafka_auto_create_topics` | 立项 | conf 开关 + produce 到未知 topic 按开关建默认单分区；默认 false 零行为变化 |
| 3 | produce timestamp 保留 | 末位立项（观望，本轮不实施） | 唯一动存储格式项：新写带 t pair、旧行回退到达时钟、by-ts 双时钟；再评条件见 §0.2 |
| 4 | ListOffsets v2+ | 立项 | K 车道首要：现仅注册 v0–v1，新 SDK 默认只发 v2+，握手后直接断连（error 35） |
| 5 | DescribeConfigs 桩 | 立项 | 回 topic 级最小配置集（空/默认值），满足 AdminClient 启动期探测 |
| 6 | OffsetForLeaderEpoch | 立项 | 单节点语义下 epoch 恒定，回常量应答（leader epoch = 当前恒定值） |
| 7 | XCLAIM TIME/RETRYCOUNT/IDLE | 立项 | claim 写侧改写投递时间/次数/空闲；读侧 XPENDING 输出随之变化的回归 |
| 8 | XINFO FULL | 立项 | 流/组/PEL 全量视图；输出规模受 #14 落地顺序保护（先 GC 后 FULL） |
| 9 | XADD NOMKSTREAM/LIMIT | 立项 / LIMIT 语义收缩澄清 | NOMKSTREAM 正常实现；LIMIT 语义 = XTRIM 的 LIMIT 参数（既有实现已覆盖），台账注明，不造私有语义 |
| 10 | HTTP 批量 produce | 立项 | `/produce_batch` 单请求多条写入；逐条生效、逐条幂等，不引入批事务边界 |
| 11 | HTTP 批量 ack | 立项 | `/ack_batch` 单请求多条确认；失败按条返回 |
| 12 | HTTP `/range` 回放 | 立项 | 按 id 区间拉历史消息；只读，不影响消费组水位 |
| 13 | `rocksmq_max_connections` | 立项 | conf 键 + rocksmq 接入层连接上限（镜像 kafka 面 ConnGuard，0 = 内建默认 4096） |
| 14 | 消费者注册行 idle GC | 立项（独立波） | 组内死成员按 idle 回收；三重判据（无 PEL ∧ 无活跃租约 ∧ 超时），默认关 |
| 15 | 广播消费客户端模式文档化 | 立项（纯文档） | `features/mq-lite.md` 增广播模式节：每消费者独立组名 |

> **复核结论（2026-10-07，W1 #9b）**：#9 的 LIMIT 子句维持「= XTRIM 参数语义」的收缩
> 结论，**已覆盖**。证据：XTRIM 与 XADD 修剪共用同一解析器与执行器——
> `src/lite/append_opts.rs:110-123`（`trim_at` 解析 `MINID [<~|=>] <id> [LIMIT <n>]`，
> XTRIM 的 `parse_trim` 与 XADD 的 `scan_opts` 都经它）、`src/lite/append_opts.rs:263`
> 与 `:274`（LIMIT 预算在取受害者**之前**判定，LIMIT 0 = 本轮不删）、执行侧
> `src/lite/append.rs:362`（xtrim）与 `:140`（xadd 携带修剪同批删除）。行为由
> `tests/lite_trim_minid_e2e.rs`（LIMIT 分段/LIMIT 0，既有）与
> `tests/lite_xinfo_full_e2e.rs::xadd_trim_pins_xtrim_limit_semantics`（XADD
> `MINID ... LIMIT` 钉行为，本批新增）双重钉住。#9a 新增的 NOMKSTREAM / XADD 修剪
> 不引入任何私有 LIMIT 语法。

分诊口径：**13 条立项进入执行波次**；#3 观望（唯一动存储格式的条目，等真实
by-ts 诉求再评）；#9 立项但 LIMIT 子句按既有 XTRIM 参数语义收窄，不新增私有
语法（台账在 `COMPAT.md` MQ 节注明）。

## 2. 波次与车道

| 波次 | 车道 | 条目 | 交付物 |
| --- | --- | --- | --- |
| W0 前置 | C（conf/治理） | #2/#13/#14 配置面 | `src/conf.rs` 一次性碰齐 `kafka_auto_create_topics`(bool，默认 false)、`rocksmq_max_connections`(i64，0=内建默认 4096)、`lite.consumer_gc_ms`(u64，0=关)；`config/conf.yaml` 注释样例；conf 单测；本计划文档 + 索引登记 |
| W1 三车道并行 | K（kafka wire） | #1/#4/#5/#6 + #2 接线 | 新 `src/kafka/admin_topics.rs`（CreateTopics(19)/DeleteTopics(20)/CreatePartitions(37)）；DescribeConfigs(32) 桩；OffsetForLeaderEpoch(23) 常量应答；ListOffsets v2+ flexible 解码；广告面 15→20（+19/20/23/32/37）；produce 未知 topic 接 `kafka_auto_create_topics` |
| W1 三车道并行 | L（lite 动词） | #7/#8/#9 + #15 | 新 `src/lite/append_opts.rs`（NOMKSTREAM/LIMIT 解析）；`src/lite/claim.rs` 增 TIME/RETRYCOUNT/IDLE；`src/lite/info.rs` 增 XINFO FULL；`features/mq-lite.md` 广播模式节 |
| W1 三车道并行 | H（http 面） | #10/#11/#12/#13 | `src/rocksmq/api.rs` 增 `/produce_batch`/`/ack_batch` 路由；新 `src/rocksmq/range.rs`（`/range` 回放）；接入层 ConnGuard 镜像（`rocksmq_max_connections` 接线） |
| W2 引擎风险项 | G（GC） | #14 | 新 `src/lite/consumer_gc.rs`：判据 = 无 PEL ∧ 无活跃租约 ∧ 超时；默认关（`lite.consumer_gc_ms: 0`） |
| W3 可选（本轮观望） | — | #3 | 不做；触发再评后另立细案 |
| W4 集成收尾 | D（docs/台账） | 全部 | scenario 扩段（广播/批量/GC 场景）；`features/e2e-coverage.md`/`COMPAT.md`/`features/changelog/` 同步；`00-gap-matrix.md` + `05-p3-pool.md` 出池三件套；全量验收 |

波次依赖：W0 先行（conf 键是 K/H/G 三车道的接线前提）；W1 三车道互不依赖可
并行；W2 独立成波（引擎风险项给足评审面）；W3 不排期；W4 收尾。车道内唯一
顺序约束：#14（W2）先于 #8 的 FULL 大组视图评审收口（同池 §3.4 纪律）。

## 3. 测试与提交

### 3.1 新增 e2e 文件

| 文件 | 覆盖 |
| --- | --- |
| `tests/kafka_topics_e2e.rs` | #1 建/删 topic、#5 DescribeConfigs、#6 OffsetForLeaderEpoch、#4 ListOffsets v2+、#2 auto-create 开关两态 |
| `tests/lite_claim_opts_e2e.rs` | #7 XCLAIM TIME/RETRYCOUNT/IDLE + XPENDING 读侧回归 |
| `tests/lite_xinfo_full_e2e.rs` | #8 XINFO FULL 流/组/PEL 视图 |
| `tests/rocksmq_batch_range_e2e.rs` | #10/#11 批量端点 + #12 `/range` + #13 连接上限 |
| `tests/lite_consumer_gc_e2e.rs`（+ 进程级伴随文件） | #14 GC 判据、默认关、误删守卫（活跃成员/有 PEL 成员不被回收） |

既有回归门：`lite_ordered_*`、`flushdb_*`、`kafka_*` 全族、`rocksmq_*` 全族；
#7 与 Batch 1 的 claim/重投面同文件，落地时以先合入形态为基线 rebase（池
§3.2 纪律）。

### 3.2 harness 纪律

`tests/common/mod.rs` 已 799/800 行：本计划**不加一行**；需要的共享 helper 一律
落子模块（`tests/common/` 下新文件），大文件继续走既有拆分先例。

### 3.3 提交切分

| 序 | 提交 | 内容 |
| --- | --- | --- |
| 1 | feat(conf) | W0 三键 + yaml 样例 + conf 单测 + 本计划文档/索引 |
| 2–3 | feat(kafka) ×2 | a) ListOffsets v2+ + 广告面；b) admin_topics + DescribeConfigs + OffsetForLeaderEpoch + auto-create 接线 |
| 4–5 | feat(lite) ×2 | a) append_opts（#9）+ claim 选项（#7）；b) XINFO FULL（#8）+ 广播文档（#15） |
| 6 | feat(rocksmq) | 批量端点 + `/range` + 连接上限（#10–#13） |
| 7 | feat(lite)+test | consumer_gc（#14）+ 专属 e2e（含进程级） |
| 8+ | docs/test | e2e-coverage / COMPAT / changelog / 出池三件套（W4，允许拆 2–3 个） |

### 3.4 路径隔离（与 SQL 工作流并行）

- 文件集白名单：`src/conf.rs`、`src/kafka/**`、`src/lite/**`、`src/rocksmq/**`、
  `config/*.yaml`、`tests/`（上述新文件 + 既有 mq 系）、`features/mq-*.md`、
  `COMPAT.md`、本计划目录；
- **不碰** `src/sql/**` 与 `tests/*sql*`；不 revert、不整理任何在途脏文件；
- 共享脏文档（`plans/index.md`、`features/e2e-coverage.md`、`COMPAT.md`）只做
  本计划自己的 hunk，提交时手工手术切分，不带走他人改动。

## 4. 风险与规模

| 风险 | 缓解 |
| --- | --- |
| ListOffsets v2 flexible 解码（tagged fields）引入形态误判 | 先落 wire 层单测骨架（v2 请求帧字节级样例）再接 handler；广告面注册与解码同提交 |
| #14 误删活跃成员 | 三重判据（无 PEL ∧ 无活跃租约 ∧ 超时）+ 专属 e2e（活跃/带 PEL/死成员三态）+ 默认关 |
| #1 删 topic 漏折叠（delay 暂存 0x1D / 组账本 0x20 残留） | 复用 `family_delete` 键族路径（0x1D/0x20 已在 Batch 1/2 登记折叠）；e2e 断言删后 DLQ/延迟/账本齐消 |
| 行数红线（`claim.rs` 404、`info.rs` 334、`api.rs` 257 迭代后逼近 800） | append_opts / range / ConnGuard 镜像 / xinfo_full 编码全部独立新模块，老文件只做接线 |
| 广告面膨胀（15→20）引发客户端协商面变化 | 仅 ApiVersions 行变化，changelog 明示新增 key 清单与版本窗口 |
| SQL 工作流并行踩踏 | §3.4 白名单 + hunk 手术；`src/sql/**`、`tests/*sql*` 零接触 |

规模口径（沿用池 §2 预估）：**8 小 + 6 中 + 1 中高风险 ≈ 4 波 6 车道 7–10 提交**
（小：#2/#4/#5/#7/#9/#11/#13/#15；中：#1/#6/#8/#10/#12/#14；中高：#3，观望）。

## 5. 验收门

| 门 | 标准 |
| --- | --- |
| 全量回归 | `cargo test --workspace --no-fail-fast` 全绿；用例数对基线 1780 **只增不减** |
| 静态门 | `cargo fmt --check` 零 diff；`cargo clippy --workspace -- -D warnings` 零告警 |
| 专项回归门 | `lite_ordered_*`、`flushdb_*`、`kafka_*` 全族、`rocksmq_*` 全族逐套绿 |
| 行数审计 | 新文件 ≤400 行；迭代文件 ≤800 行（`src/conf.rs` 355 起步、`tests/common/mod.rs` 799 冻结） |
| 台账完整性 | 出池三件套（00 矩阵 / 05 池 / changelog）同批落；#9 LIMIT 语义收缩在 `COMPAT.md` 注明 |
