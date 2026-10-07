# Lite MQ：MQ 路线决策 + Lite MQ 面规范

## 定位
- rdb 的消息队列能力保持**单一产品线**：RocketMQ 5.5 Lite Mode 风格的语义模型
  （父主题 + 动态队列 + 消费组水位 + PEL），经 Redis Streams 动词面暴露——即
  [agents/rust](../agents/rust/index.md) 的 `lite/` 模块族（"A 路线"）。
- Kafka 线协议兼容改为**分阶段落地**（P0 已开始）：可选的 `kafka front` 协议前置
  适配层，规范见 [kafka-front.md](./kafka-front.md)。
- 本文档两部分：上半部分为路线决策记录（**final，不再复议**）；下半部分为正在落地的
  Lite MQ 面**规范（living spec）**，与实现同步演进。

## 路线决策（final）

### 决策陈述
- A 路线是唯一在维护的 MQ 线路：全部能力经 Redis Streams 动词访问
  （XADD/XREADGROUP/XACK/XPENDING/XCLAIM/XAUTOCLAIM/XGROUP/...），Redis SDK 即用。
- Kafka wire 兼容：原判 "rejected for now"，**2026-09 起改为分阶段落地**——形态维持
  当年预留的样子：可选的 `kafka front` 协议前置（parent/child → topic/partition
  静态映射），不是引擎分叉。规范与阶段表见 [kafka-front.md](./kafka-front.md)
  （P0：骨架 + ApiVersions/Metadata，已落地）。单节点持久性风险显式接受，
  "先数据面复制、再谈协议"的结论对 P3+（组协调、acks=all 承诺）依然有效。

### 差距盘点：引擎级 vs 协议级
对齐"一个可用的 MQ"所缺的能力，**全部是引擎级缺口，没有一个能靠换协议解决**：

| 缺口 | 层级 | 处置 |
|---|---|---|
| PEL / pending list（投递未确认的持久化） | 引擎 | 本批落地（kind 0x0F） |
| 重投递：XPENDING / XCLAIM / XAUTOCLAIM | 引擎 | 本批落地 |
| 消费者管理：XGROUP CREATECONSUMER / DELCONSUMER | 引擎 | 本批落地 |
| 多流消费：XREAD / XREADGROUP 多 STREAMS | 引擎 | 本批落地 |
| Lite 基准（xadd / xreadgroup / xack 工况） | 引擎 | 本批落地 |
| 客户端 SDK 生态 | 协议 | Redis SDK 即用，无缺口 |

- 结论：**两条路线（保留 Redis 动词面 vs 换 Kafka 协议）需要先做完全相同的引擎可靠性
  工作**——协议换皮解决不了上表任何一行。顺序只能是**先引擎、后协议适配**。

### Kafka 兼容成本清单（为什么 rejected for now）
- **11 个 wire API** 最小可用集：ApiVersions / Metadata / Produce / Fetch / ListOffsets /
  OffsetFetch / FindCoordinator / JoinGroup / SyncGroup / Heartbeat / LeaveGroup /
  OffsetCommit。
- **内嵌 group coordinator**：generation / epoch 与 rebalance 状态机（JoinGroup/SyncGroup
  两阶段、成员失效探测、rebalance 通知）——一个独立于存储的分布式子系统。
- **消息格式**：RecordBatch v2（CRC、attributes、变长字段）+ 4 种 codec
  （none/gzip/snappy/lz4）。
- 量级：**以月计**的工程量，且此后每个 Kafka client 版本都是持续兼容面。
- **语义信任陷阱（关键否决理由）**：Kafka 客户端默认预期 acks=all / ISR / 幂等生产 /
  事务；而 rdb 的整个数据面（包括普通 KV）是节点本地、非复制的（Lite 元数据同样不经
  raft）。做出"接受 Kafka 连接但只有单节点持久性"的半兼容，等于给生产用户埋信任陷阱：
  客户端一切正常，直到节点故障才暴露没有 ISR。
- **Redpanda 式全兼容是一条独立产品线**，不是增量 feature；做对的前提是数据面复制
  先落地。

### 先引擎、后协议
- 引擎可靠性工作（PEL、重投递、消费者管理、多流、基准）对两条路线**等价必要**——先
  做完，A 路线立即受益。
- 若未来确需 Kafka 面：`kafka front` 作为协议前置，把 parent/child 静态映射为
  topic/partition（类比设想中的 sql/front 之于 MySQL 协议），引擎与 Redis 动词面不动。
- 目标用户陈述：**目标是 Redis SDK 用户**。若一个 Kafka 应用无法更换客户端 SDK，那是
  另一个量级的项目——前提是先做数据面复制，再谈协议。

## Lite MQ 面规范（已落地）

以下为规范（spec），由 `src/lite/`（`pel.rs` / `pending.rs` / `claim.rs` / `autoclaim.rs` / `read.rs` /
`park_wait.rs` / `group.rs` / `ack.rs` / `dlq.rs` / `redeliver.rs`）实现；对 Redis 的偏差总表见
[COMPAT.md](../COMPAT.md) Lite Mode 条目。

### PEL 物理布局（kind 0x0F 窗口）
- 每条流一个 `KIND_STREAM_PEND`（0x0F）窗口，随流族回收范围整窗回收：
  - pend 条目 = `data_key(prefix, 0x0F, stream) ++ group ++ 0x00 ++ <id 16B BE>`——
    固定宽大端后缀使 PEL 天然按 id 有序，范围查询即前向扫描。
  - 消费者登记 = `data_key(prefix, 0x0F, stream) ++ group ++ 0x01 ++ name`——tag 0x01
    严格排在同组全部 tag-0x00 行之后，两个子空间互不交叠、均可前缀扫描。
- 值 = 统一 expire 信封 + JSON `{consumer, delivered_ms, times_delivered}`。

### 投递与消费者登记
- `XREADGROUP ... >`：投递在**同一 latched WAL 批次**内同步写 PEL 行（与条目同一
  持久化路径）；重复投递覆盖既有 PEL 行。
- 消费者首次投递自动登记；`XGROUP CREATECONSUMER` / `XGROUP DELCONSUMER` 显式管理。
- `DELCONSUMER` 清除该消费者名下全部 pending 行，回复 = 清除行数。
- `XGROUP DESTROY` 范围删除该组整个 0x0F 窗口。
- `XINFO CONSUMERS <stream> <group>`：name / pending / idle 三字段；idle 取该消费者
  最新 PEL 行的 `delivered_ms`（无独立活动跟踪）。

### XACK
- 删除 PEL 行与组水位持久化（kind-0x0E 记录）**同一原子批次**；回复保持水位推进计数
  语义（偏差详见 COMPAT 条目）。

### XPENDING / XCLAIM / XAUTOCLAIM
- `XPENDING <stream> <group>`：摘要——总数 / 最小 id / 最大 id / 每消费者计数。
- `XPENDING ... <start> <end> <count> [consumer]`：范围形态，支持 IDLE 过滤与
  consumer 过滤。
- `XCLAIM`：min-idle-time、FORCE、JUSTID，以及投递提示 `IDLE <ms>` / `TIME <unix-ms>` /
  `RETRYCOUNT <n>`（与 id 列表任意交错，Redis 同款文法；**不支持** LASTID，语法错误）。
  提示作用于**每个成功 claim 的 PEL 行**（JUSTID claim 同样写入——所有权转移也是一次
  PEL 写）：IDLE → `delivered_ms = now - idle`（回拨，钳到 0）；TIME → `delivered_ms`
  直接取给定墙钟（idle 由 now 推导）；RETRYCOUNT → `times_delivered` 整体改写（不再
  自增）。`delivered_ms` 仍是空闲口径的**唯一事实源**：XPENDING 的 idle/deliveries 两列、
  min-idle 门与空闲 sweep 全部由它推导——大 IDLE / 过去 TIME 回拨后的行会**更早**满足
  min-idle（正确行为）。坏值报 `ERR value is not an integer or out of range`。
- `XAUTOCLAIM`：游标续扫；COUNT 默认 100，单轮扫描上限 10×COUNT；回复为 Redis≥7 的
  三元素形态（含 deleted-ids 数组）；JUSTID 支持。

### 多流 XREAD / XREADGROUP
- `STREAMS` 列表必须配平（流数 = id 数）；COUNT 为每流配额。
- 阻塞形态：全 `>` 流的 XREADGROUP BLOCK 只 park **一个** waiter，同时挂在该消费者
  全部 `>` 流的 meta 键下——任一流有新条目即醒。

### 崩溃模型（at-least-once）
- PEL 行在投递时同步落盘（kill -9 至多丢在途批次）；重启后 delivered 回卷到已提交
  水位，**未 ACK 条目重投（可能重复）——客户端幂等是契约**。
- 组水位 200ms 批量刷盘窗口只会导致重投，**永远不会丢**。

### 可观测性与基准
- `rdb_lite_backlog` gauge：各组缓存 pending 计数之和；首次加载组时从盘上重算。
- `rdb_lite_dlq_depth` gauge：全部在册 DLQ 目标流的条目深度之和（点读聚合）。
- `rdb_lite_messages{op=...}` counter：按操作计数，新增 `dlq`（死信转移条数）与
  `redeliver`（空闲重投条数）两个 op。
- bench 新增工况：`xadd` / `xreadgroup` / `xack`。

### DLQ 与 MAXDELIVERY（死信队列）
- **语法**：`XGROUP CREATE <stream> <group> <id|$> [MKSTREAM] [ORDERED [INFLIGHT <n>]]
  [MAXDELIVERY <n≥1>] [DLQ <name>]`（两个新子选项与 `ORDERED` 正交，可任意组合）。
  - `MAXDELIVERY <n>`：单条消息的投递次数上限；下一次交付会把 `times_delivered` 推过
    `n` 时不再投递，改为死信转移。`n` 必须 ≥1（0 报
    `ERR value is not an integer or out of range`）。
  - `DLQ <name>`：死信目标流名，必须是合法 `parent/child` 全名（否则
    `ERR invalid DLQ stream name`）；**必须随 MAXDELIVERY 出现**（无上限的组永不转移，
    先行报 `ERR syntax error`，同 INFLIGHT 依赖 ORDERED 的先例）。
  - 缺省目标 = 字面量 `<stream>/dlq`：三段名仍以首个 `/` 前的 parent 定 slot，
    与源流同 slot 同窗；嵌套 child 不参与 XADD 裸 parent 轮转，DLQ 不会收到业务流量。
- **转移语义（单 WAL 批原子）**：一次转移在**一个 WriteBatch** 内完成三件事——
  删除源组 PEL 行、推进组 committed 水位（**语义等同 XACK**：跨洞转移停在
  `head_after_ack`，重启不会复活）、把消息（原 id/字段 + 溯源字段 `__dlq_group` /
  `__dlq_consumer` / `__dlq_times` / `__dlq_src`）写入 DLQ 流。判定与转移都发生在
  投递路径（XREADGROUP 重投 / XCLAIM / XAUTOCLAIM / 空闲 sweep）持有的流 latch 内，
  并发 claim 不可能双转；重复 claim 已转移的 id 既不投递也不追加 DLQ 副本。
  溯源字段名与业务字段撞名时业务值原样保留。
- **触发面**：转移判定只在投递时刻发生（默认无后台任务；启用 `lite.redelivery_idle_ms`
  后由空闲 sweep 在同一投递路径的 latch 内触发，见下节）；未配 MAXDELIVERY 的组
  行为与之前完全一致（零开销）。
- **ORDERED 组限制**：有序组只允许 **PEL 头**（最小 pending id）被转移——与
  XCLAIM/XAUTOCLAIM 只认头的接管语义一致，越过头部的转移被抑制。
- **DLQ 本身是普通流**：可 XRANGE/XLEN、可建组独立消费/ACK，也可再配
  MAXDELIVERY 分级死信；指标 `rdb_lite_dlq_depth`（gauge）与
  `rdb_lite_messages{op="dlq"}`（counter）。

### 自动重投（空闲 sweep，默认关）
- **配置**：`lite: redelivery_idle_ms: <ms>`（`src/conf.rs`，默认 **0 = 完全不启用**，
  无后台任务）。设为正数后，200ms 一轮的旋转扫描从 kind-0x0E 窗口发现组（重启后
  未被触碰的组也能被扫到；每轮 32 组、每组 16 行，大 PEL 跨轮分摊）。
- **语义**：对 `now - delivered_ms >= redelivery_idle_ms` 的 PEL 行执行 claim 原语——
  `times+1` 并刷新 `delivered_ms`，重投给**当前消费者**（不换主）；越过 MAXDELIVERY
  的行直接走 DLQ 转移。已 ACK 的行永不重投。ORDERED 组只 sweep **PEL 头**
  （`ordered::force_takeover`，与 claim 同路径）。
- **实现姿态**：每轮 SYNC、流 latch 走 `try_lock`——正在执行命令的流本轮跳过，
  绝不 park 在后台任务上；重投计数进 `rdb_lite_messages{op="redeliver"}`，
  死信进 `{op="dlq"}`。
- **sweep × min-idle 交互**：sweep 对判定为 due 的行执行 claim 原语时**会刷新
  `delivered_ms`**——客户端若用 `min-idle-time >= redelivery_idle_ms` 的
  XAUTOCLAIM/XCLAIM 去接手，行刚爬到客户端的 idle 门槛，下一轮 sweep（200ms 节奏、
  同一门槛）就先把它重投并刷新了时钟，客户端**几乎永远观察不到**这些行；它们可以在
  **零客户端消费**的情况下被 sweep 一路推过 MAXDELIVERY 进 DLQ。要客户端接管有效，
  取 `min-idle-time < redelivery_idle_ms`（sweep 门槛更宽，客户端先到先得）。

### 延迟消息（XADD DELAY，kind 0x1D 暂存行）
- **语法**：`XADD <stream> [<id|*>] DELAY <ms> <f> <v> [...]`——`DELAY <ms>` 为
  **前置选项**（位于 id 之后、首对 field-value 之前，大小写不敏感）；不带 `DELAY`
  的 XADD 行为完全不变。`ms` 为**相对延迟**；`DELAY 0` 等价于不延迟（同步可见）；
  非正整数/非整数报 `ERR value is not an integer or out of range`，`now+ms` 溢出报
  `ERR delay deadline overflow`。
- **到期前不可见**：暂存行是独立 kind `0x1D` 记录（键 = `<slot>/ 0x1D <due_ms u64BE>
  <stream_len u32BE> <stream> <预留 id 16B>`，值 = entry 体；**due 在键首**使到期扫描
  是有界早停前缀扫描，键序即 due 序），物理上不是流 entry——XREAD/XREADGROUP/XRANGE/
  XLEN 到期前全部不可见、XLEN 不计；读路径零改动。
- **到期投递**：后台 sweep（`lite: delay_sweep_ms: <ms>`，默认 **0 = 不启用**、无任务；
  对齐 redeliver_loop 的 spawn 模式）逐 slot 扫 0x1D 窗口，`due <= now` 的行在目标流
  latch 下**单批**完成"删暂存行 + 写 entry + meta 维护（len+1/last_id/idle retouch）"，
  随后唤醒该流的 BLOCK 读者（XADD 同款 notify，流 meta key + 父 topic key）。崩溃两态
  归一：行在 = 未投（重启后重扫再投，不丢）；行不在 = entry 已落（不双投）。
- **id 语义（对计划草案的有记录偏差）**：XADD 时刻在 latch 下预留 id 并**推进
  last_id**（防后续 XADD 撞号），回复该 id；但到期写入时**重新分配新 id**（恰如普通
  append）——预留 id 可能落后于延迟窗口内新写入并被消费的水位（组 delivered/
  XREAD `$`），沿用会永远不可见（静默丢投）；新 id 同时保证"乱序提交按 due 升序投递、
  消费者看到的顺序 = 到期顺序"。回复 id 是预约凭证，不承诺最终 entry 恰为该 id。
- **P0 键族登记**：暂存行不属 STREAM_FAMILY 连续段（0x0C..=0x0F），且 due 在键首使
  按 stream 的范围删除不可行——`family_delete_entries`（XIDLE 到期/惰性清除/RENAME
  覆盖目标）显式**扫描折叠**该 slot 0x1D 窗口中目标流的行；`move_family`（RENAME）
  同法把 0x1D 段搬到新名；FLUSHDB 经 `classify`（0x1D 记为 typed family member）
  随全库清除。漏登记 = 已删流被暂存行复活投递（P0）——`lite_delay_e2e` 三连回归
  （XIDLE/RENAME/FLUSHDB）逐一断言。
- **兜底守卫**：sweep 交换前在 latch 下重读流 meta——meta 缺失（族已被删/搬走）时
  **只删暂存行、不投递**，任何竞态漏出的孤儿行不可能复活已删流。
- **kafka 面不暴露**：Produce 面无延迟参数；Fetch 只见到期后的普通 entry。RESP
  动词面之外，RocksMQ HTTP 前置的 `/produce?delay_ms=` 是**纯透传**（见下节）。

### XTRIM MINID
- **语法**：`XTRIM <stream> MINID [~|=] <id> [LIMIT <n>]`（对齐 Redis 6.2+）；与
  `MAXLEN` 正交（可交替使用）。删除所有 **id 严格小于** `<id>` 的条目，边界 id 本身
  保留；回复 = 本次删除条数。
- `~`（近似）与 `=`（精确）**行为完全一致**：受害者精确计算，绝不欠删；`LIMIT <n>`
  限制本轮删除上限（两种 flag 后均可带），余量留给下一次调用。
- **时间窗留存**：id 首段即到达毫秒时间戳，故 `XTRIM <s> MINID <cutoff_ms>-0`
  就是"保留最近时间窗"——一条周期命令即可，无需逐消息索引。
- **kafka 账本守卫**：流上存在任何 kind-0x20 组提交账本行时，XTRIM/XDEL 一律拒绝
  （`ERR stream <name> has committed consumer-group offsets; delete the groups first`），
  防止 ordinal↔id 映射在 kafka 面读者脚下漂移；XDEL 同守卫。

### XADD 选项与追加时修剪（NOMKSTREAM / MAXLEN / MINID）

选项解析集中在 `src/lite/append_opts.rs`（`parse_xadd`），与 XTRIM **共用**修剪解析器
（`parse_trim` / `trim_at`）与受害者计算（`trim_victims`），不另起一套语法：

| 选项 | 语义 | 备注 |
| --- | --- | --- |
| `NOMKSTREAM` | 流不存在时**不建键**，回 nil（RESP2 `$-1`，与读路径的空回同形） | 键缺席可用 `XINFO STREAM`（`ERR no such key`）/ `XLEN`（0）验证；流已存在则为普通追加 |
| `DELAY <ms>` | 暂存到到期交换（见上节） | 历史位置在 id 之后；纯数字、u64 范围，坏值报 not-an-integer |
| `MAXLEN [~\|=] <n>` | 追加后把流修剪到最新 `n` 条（新条目计入） | 与 XTRIM 的 MAXLEN 同语义 |
| `MINID [~\|=] <id> [LIMIT <n>]` | 追加后删除 id **严格小于** `<id>` 的条目 | LIMIT 收窄为 XTRIM 参数语义（见 `plans/2026-10-07-mq-p3-backfill` §1 复核结论） |

- **位置文法**：选项可出现在 id 之前或之后（`XADD <s> [NOMKSTREAM][trim] [<id\|*>]
  [DELAY <ms>] <f> <v> ...`）。一旦 id **之前**出现了选项块，id 即为必填（Redis 自身
  文法如此，客户端把自动 id 写成 `*`）；无前置选项时保持历史的奇偶省略文法
  （`XADD s f v` = 自动 id）不变。
- **同一批次**：追加（或 DELAY 暂存）与修剪受害者删除落在**同一个 latched fsync 批次**
  （meta len 先加后剪）；新条目本身在计划够得着时也入受害者（如 `MAXLEN 0`、MINID
  高于新 id）。XADD 携带修剪同样过 kafka 账本守卫。
- 选项可重复，后者覆盖前者；错误文案沿用 XTRIM 家族（wrong number of arguments /
  not an integer / Invalid stream ID）。

### XINFO STREAM FULL

`XINFO STREAM <stream> FULL [COUNT <n>]`（COUNT 默认 10，须 >0）：Redis 7 形态的深视图，
编码在 `src/lite/xinfo_full.rs`（分发留在 `info.rs`）。流级 = `length`、
`last-generated-id`、`entries`（最新 `n` 条、按 id 升序输出，`[id, f, v ...]` 同
XRANGE 帧）、`groups`。组级 = `name`、`last-delivered-id`（缓存优先，回落组记录）、
`pending`（`[id, consumer, 距上次投递 ms, 投递次数]` 行，COUNT 截断）、`consumers`。
消费者级 = `name`、`seen-time`、`pending`（精确计数，**不**截断）、`pel`
（`[id, 距上次投递 ms, 投递次数]` 行，COUNT 截断）。

**省略字段**（引擎模型没有对应数据，宁缺毋假）：`radix-tree-keys` / `radix-tree-nodes`
（无 radix 索引）、`entries-added` / `max-deleted-entry-id` /
`recorded-first-entry-id`（meta 无这些计数器，见 `model.rs` 的 `MetaPayload`）、组级
`entries-read` / `lag`（无按组读取计数）。`seen-time` 为**近似值**：取该消费者最新
PEL 投递时间，无 PEL 历史时取登记时间（`ConsumerState.created_ms`）——与
`XINFO CONSUMERS` 的 idle 口径同源。非 FULL 的 `XINFO STREAM` 输出保持不变
（length / last-generated-id / groups / idle-ms）。

### 广播消费（broadcast）客户端模式

**模式**：一条流要"每个订阅者都拿到全量消息"时，给**每个订阅者一个独立消费组**
（one group per subscriber），各组各自 `XREADGROUP GROUP <g_i> <c> ... > ` 从自己的
`delivered` 水位读全流；与之相对的**竞争消费**（competing consumers）是同组多名
消费者分摊消息。两种模式可并存于同一流（再建一个"工作组"即可）。

依据（引擎行为，均可在 `src/lite/` 验证）：
- **组间完全隔离**：每组有自己的 `delivered`/`committed` 水位与整个 kind-0x0F PEL
  窗口（`pel.rs`：PEL 键 = `stream ++ group ++ tag ++ id`，组名为键的一部分）——
  一组的 XACK/XCLAIM/XAUTOCLAIM/XPENDING 只看本组行，互不可见。
- **DLQ / MAXDELIVERY / 重投也是组内属性**：`maxdelivery`/`dlq` 挂在组记录上
  （`model.rs` `GroupPayload`），空闲 sweep 按组扫描（`redeliver.rs` `discover_groups`
  直读 kind-0x0E 组记录）——一个订阅者积压死信不影响其它订阅者的投递。
- **投递顺序对所有组一致**：条目按 id 升序投递（`read.rs` 的 `>` 路径沿 entry 键序
  扫描），故所有组看到相同的全序。
- **延迟消息的到期序对所有组一致**：到期交换按 due 全局排序写入流
  （`delay.rs` 暂存行扫 due 交换、分配正式 id）——各组看到的都是**按到期时间排序
  后**的到达序（同 due 批内按预约 id 序），不会出现两订阅者看到的相对顺序不同。

最小 RESP 会话示例（两个订阅者各一组，竞争消费对照）：

```text
# 发布方：一条流，三个订阅视角（g1/g2 广播组 + gw 工作组）
XGROUP CREATE bc/q0 g1 0-0 MKSTREAM     # 订阅者 1 的组
XGROUP CREATE bc/q0 g2 0-0 MKSTREAM     # 订阅者 2 的组
XGROUP CREATE bc/q0 gw 0-0 MKSTREAM     # （对照）竞争消费工作组
XADD bc/q0 1-1 f hello
XADD bc/q0 2-1 f world

# 订阅者 1：拿到全量（自己的水位从 0-0 起）
XREADGROUP GROUP g1 sub1 COUNT 10 STREAMS bc/q0 >
1) 1) "bc/q0"
   2) 1) 1) "1-1" 2) "f" 3) "hello"
      2) 1) "2-1" 2) "f" 3) "world"

# 订阅者 2：同样全量——组隔离，g1 的读/ACK 不影响 g2
XREADGROUP GROUP g2 sub2 COUNT 10 STREAMS bc/q0 >
1) 1) "bc/q0"
   2) 1) 1) "1-1" 2) "f" 3) "hello"
      2) 1) "2-1" 2) "f" 3) "world"

# 工作组任一成员只拿一半（对照：竞争消费分摊）
XREADGROUP GROUP gw w1 COUNT 1 STREAMS bc/q0 >
1) 1) "bc/q0" 2) 1) 1) "1-1" 2) "f" 3) "hello"
XREADGROUP GROUP gw w2 COUNT 1 STREAMS bc/q0 >
1) 1) "bc/q0" 2) 1) 1) "2-1" 2) "f" 3) "world"

# 各组独立 ACK/重投：g1 确认 1-1 只动 g1 的 PEL（g2 仍可独立确认/重试）
XACK bc/q0 g1 1-1
:1
XPENDING bc/q0 g2        # g2 的 PEL 原封不动
1) (integer) 2 ...
```


### HTTP 面（rocksmq front，WP4）
RocksMQ HTTP 前置（[rocksmq-http.md](./rocksmq-http.md)）在本批对齐四接口，
全部经 `command::dispatch` 复用上面的引擎语义，**零旁路**：
- **`wait_ms` 长轮询**：`/consume` 可选参数，映射为 XREADGROUP/XREAD 的 `BLOCK`
  （同一 `park_wait` 循环）；空通道 park 至到期回 200 空列表，非阻塞错误。
  延迟消息的**到期交换**会唤醒 park 中的读者（暂存行写入不唤醒）。
- **`POST /pending`**：XPENDING 概要只读透传（总数/min/max id/按消费者分布），
  与 RESP 面同口径；不建组、不动 PEL。
- **`delay_ms`**：`/produce` 可选参数，纯透传 XADD `DELAY`（`0`/缺省 = 不带）；
  到期前一切消费路径不可见，到期交换分配新 id（回复 id 是预约凭证）。
- **`rocksmq_token` Bearer**：空（默认）= 开放；非空 = 全路由 401/通过矩阵，
  es/s3 同款姿态。

## 有序消费组与 Kafka 校准语义（P0/P1/P2）

对齐 Kafka 语义模型的三件套：**P1 提交语义**、**P0 顺序消费**、**P2 有序接管**
（P3 同 key 同队列 = `XPICK ... pick_hash`，已落地）。

### P1：提交水位 = Kafka committed offset（连续前缀提交）
- `XACK` 后组已提交水位（kind-0x0E 记录）只在 **被 ACK 的连续前缀** 上推进：
  候选 = `head_after_ack`（从 `succ(committed)` 起 PEL 首条幸存 pending 行以下的
  最大已 ACK id）；跨洞 ACK 不推进水位。
- 越过空隙的 ACK id **不记忆**：其 PEL 行仍随 `XACK` 删除，但水位冻结在幸存
  pending 行以下；重启/回卷到已提交水位后**按日志重投**这些条目
  （at-least-once，宁可重复、绝不丢失）。`XACK` 回复计数仍按越过旧水位的 id 计。
- 组记录只在水位实际推进时同步落盘（未推进的 ACK 只删 PEL 行）；重启回卷到
  已提交水位，重投未提交尾部。

### P0：有序消费组（队列独占所有权 + 有序投递）
- `XGROUP CREATE ... ORDERED [INFLIGHT <n>]`（rdb 扩展；INFLIGHT 依赖 ORDERED，
  有序组最小归一为 1；MAXDELIVERY/DLQ 子选项见「DLQ 与 MAXDELIVERY」节，与
  ORDERED 正交）：组内队列同一时刻**至多一个消费者持有**。
- 所有权为**内存态租约**（默认 30s，无协调者，测试钩子
  `shared.lite.set_lease_ms`）：`XREADGROUP ... >` 即申领；租约空闲过期后下个
  申领者接管并 **epoch 递增**——被废黜/被隔离消费者的 `>` 读一律空回
  （`*-1`，BLOCK 读者重新 park），接管唤醒该流 meta 键下的等待者。
- **BLOCK 读者的门控停靠**：满窗或被隔离的 `>` BLOCK 读者不空转扫描积压，
  而是挂在该流 meta 键上 park，由窗口/所有权事件（XADD、XACK 腾位、接管、
  SETID、DESTROY）唤醒；租约过期本身无信号，故 park 切片 cap 到租约粒度，
  被 fence 的读者按租约自行醒来重试接管（每次注册后复查门控，无丢通知窗口）。
- **INFLIGHT 是吞吐旋钮**：1（默认）= 严格串行（RocketMQ orderly 等价），
  >1 = Kafka 式预取流水线；窗口满（`inflight_max - pending ≤ 0`）同样空回，
  ACK 腾位并唤醒该流 meta 键下 park 的满窗读者。`pending` 按 PEL **去重行数**
  计（重投不重复计数）。
- 满窗口不靠 `>` 迁移（卡死工作走 P2 接管）；所有权随进程重启一并消失
  （重启本就断开所有连接，无跨进程僵尸）。XGROUP DESTROY/DELCONSUMER、
  FLUSHDB、流回收同步清理所有权。

### P2：有序接管只认 PEL 头
- ORDERED 组的 `XCLAIM`/`XAUTOCLAIM` **只从 PEL 头**（最小 pending id）转移
  所有权：min-idle 预检失败的 claim **不翻转所有权**；FORCE 越过头被抑制
  （头仍然赢）；XAUTOCLAIM 单轮最多认领头一行。成功接管后 epoch 递增并唤醒
  等待者；PEL 行携带所属 epoch（可观测性）。

### 消费者 idle GC（`lite.consumer_gc_ms`，默认关）

**定位**：后台回收"死掉"的组成员——消费者登记行（kind 0x0F tag 0x01）随首次投递/CREATECONSUMER
落盘后永不自清，长期运行的话题会积累幽灵成员（XINFO CONSUMERS / XINFO FULL 越来越长）。
GC 由 `lite: consumer_gc_ms: <ms>` 门控（`src/conf.rs`，默认 **0 = 完全不启用**、无后台任务，
升级零行为变化），实现在 `src/lite/consumer_gc.rs`。

**三重判据（缺一不可，任一不满足即保留）**：
1. **无 PEL 条目**——该消费者名下 pending 为零。本轮在流 latch 下整段扫完该组 PEL 以"证明"
   空（超过 1024 行的大 PEL 本轮跳过该组，轮转照常，绝不猜测）；
2. **无活跃租约**——既没有停在等待中的 XREADGROUP（`Runtime::parked` 计数，read.rs 在
   `wait_targets` await 前后成对 acquire/release），也不是有序组队列的**在租** owner
   （`ordered::peek` + `lease_live`：有序机制自身的所有权租约已覆盖 owner，无需另建）；
3. **空闲越过阈值**——`now - seen_ms >= consumer_gc_ms`。

**seen_ms 口径（本批新增字段）**：`ConsumerState` 增加 `seen_ms`（`#[serde(default)]`，
旧行解码为 0 时保守回落 `created_ms`），由**本来就要同步落盘的写**顺带刷新，**零新增 fsync**：
`>` 投递到该消费者（read.rs，随 PEL 行同批，`created_ms` 保持不漂移）、XCLAIM/XAUTOCLAIM
指向它（claim.rs `register_consumer`，随 claim 批）、XACK 其名下条目（ack.rs 经
`pel::touch_seen`，只刷新已知名、绝不复活已回收名）。**不刷新**的场景（有意为之）：
空轮询（`>` 读到空不写库，不为此加写）、后台重投 sweep（无人参与的时钟刷新会让僵尸成员
永生，恰是有序租约 `takeover_if_stale` 明确避免的语义）。因此一个"只轮询不收消息"的消费者
在阈值后可能被回收——它在下一次投递时随投递批自动重新登记（首见重写，一个批），自愈无感。

**删除路径**：复用 XGROUP DELCONSUMER 的同一套代码——`group::plan_consumer_removals`
（PEL 行清除 + 登记行删除，同批）+ `group::consumer_removal_effects`（backlog 回退、有序
所有权释放、运行时登记遗忘），经流 meta latch（`try_lock`，redeliver 同约定：正在执行命令
的流本轮跳过）下的一次**同步** `ops::batch_write` 提交。

**与重投 sweep / XINFO 的交互**：GC 不持有也不触碰 sweep 的发现游标与 resume map（自带
独立发现游标），两者交错互不干扰；GC 删除登记行后，`XINFO CONSUMERS` 与 `XINFO STREAM
FULL` 的 consumers 名册随之收缩（两者名册 = 登记行 ∪ PEL 行主）。FULL 的 `seen-time`
字段口径不变（仍是最新 PEL 投递时间、无 PEL 时登记时间的近似，不读 `seen_ms`）。**kill -9 持久性**：回收是
同步批写，SIGKILL 后重启已回收成员**不会**重现（proc e2e 断言），幸存者及其 PEL 完整；
重启后 delivered 水位回卷到 committed，未 ack 行按 at-least-once 重投给下一个 `>` 读者。

**节奏**：1s 一轮（慢于 flusher 的 200ms：回收时延无观测面，阈值通常是分钟级，且每轮要
整扫一组 PEL），每轮至多 32 组（发现复用 redeliver 的旋转扫描）。**有序 owner 豁免**是租约
性的而非永久：租约在（默认 30s 内有投递刷新）必保；租约过期且 PEL 空、seen 过期的遗弃
owner 与普通成员一样回收（队列由下一个询问者接管）。

## 关联
- 实现：[agents/rust](../agents/rust/index.md)（`lite/` 模块族）
- 偏差总表：[COMPAT.md](../COMPAT.md)
- 首次落地：[changelog 2026-08-17 lite-mode](./changelog/2026-08-17/lite-mode.md)
- 引擎补齐：[changelog 2026-08-21](./changelog/2026-08-21/mq-lite-engine-and-kafka-decision.md)
- 本批落地：[changelog 2026-09-09](./changelog/2026-09-09/mq-ordered-groups.md)
- 引擎可靠性批次：[changelog 2026-10-06](./changelog/2026-10-06/mq-engine-batch1.md)
