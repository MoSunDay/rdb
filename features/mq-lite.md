# Lite MQ：MQ 路线决策 + Lite MQ 面规范

## 定位
- rdb 的消息队列能力保持**单一产品线**：RocketMQ 5.5 Lite Mode 风格的语义模型
  （父主题 + 动态队列 + 消费组水位 + PEL），经 Redis Streams 动词面暴露——即
  [agents/rust](../agents/rust/index.md) 的 `lite/` 模块族（"A 路线"）。
- Kafka 线协议兼容**现阶段明确拒绝**，仅保留为远期可选的协议前置适配层。
- 本文档两部分：上半部分为路线决策记录（**final，不再复议**）；下半部分为正在落地的
  Lite MQ 面**规范（living spec）**，与实现同步演进。

## 路线决策（final）

### 决策陈述
- A 路线是唯一在维护的 MQ 线路：全部能力经 Redis Streams 动词访问
  （XADD/XREADGROUP/XACK/XPENDING/XCLAIM/XAUTOCLAIM/XGROUP/...），Redis SDK 即用。
- Kafka wire 兼容：**rejected for now**。若远期重启，形态是一个可选的 `kafka front`
  协议前置（parent/child → topic/partition 静态映射），不是引擎分叉。

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

## Lite MQ 面规范（本批落地）

以下为规范（spec），由 `src/lite/`（`pel.rs` / `pending.rs` / `claim.rs` / `autoclaim.rs` / `read.rs` /
`park_wait.rs` / `group.rs` / `ack.rs`）实现；对 Redis 的偏差总表见 [COMPAT.md](../COMPAT.md)
Lite Mode 条目。

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
- `XCLAIM`：min-idle-time、FORCE、JUSTID；**不支持** IDLE / TIME / RETRYCOUNT / LASTID
  （语法错误）。
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
- bench 新增工况：`xadd` / `xreadgroup` / `xack`。

## 关联
- 实现：[agents/rust](../agents/rust/index.md)（`lite/` 模块族）
- 偏差总表：[COMPAT.md](../COMPAT.md)
- 首次落地：[changelog 2026-08-17 lite-mode](./changelog/2026-08-17/lite-mode.md)
- 本批落地：[changelog 2026-08-21](./changelog/2026-08-21/mq-lite-engine-and-kafka-decision.md)
