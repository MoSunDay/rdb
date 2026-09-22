# Kafka Front：Kafka wire 协议前置（分阶段落地）

## 定位
- **协议前置适配层**，不是引擎分叉：`src/kafka/` 只做 wire 编解码与语义映射，存储与
  引擎完全复用 Lite MQ（`src/lite/`）——与 `sql/front` 之于 MySQL 协议同构。
- Redis Streams 动词面（A 路线）仍是第一公民；Kafka 面在其上做只读映射 + 分阶段
  写路径。
- 路线决策变更记录见 [mq-lite.md](./mq-lite.md)（原 "rejected for now" 改为分阶段
  落地，P0-P5 已全部收口）。

## 映射表（权威）

| Kafka 概念 | Lite 概念 | 说明 |
|---|---|---|
| topic | parent 流 | 名字相同；parent 存在即 topic 存在 |
| partition | parent 下的 child 队列 | Kafka front 自建队列命名 `p<N>`；Lite XADD 自动队列 `q<N>` 同样映射为分区 N（RESP 侧造的流无需迁移即可见于 Kafka 面） |
| group | Lite 消费组 | 名字相同 |
| offset | 活跃条目集的 ordinal | 不是 `<ms>-<seq>` id；见"偏差" |
| committed offset | 独立账本行 | kind 0x20 `stream/offset`（P2）：键 = `data_key(父槽前缀, 0x20, 全名 t/p0) ++ "/" ++ group`，值 = JSON `{committed_ordinal,generation,leader}`；与 Lite 组水位（kind 0x0E）互不相干 |
| record key/value | 条目字段对 | P1 写路径定版：见下方"字段对映射"；读路径（P2 Fetch）按同一规则反向解码 |
| broker | rdb 节点 | 单 broker，node_id=1，host/port 取自 `kafka_bind`（通配地址改写为 localhost）；`kafka_advertised_host/port` 显式覆盖（见"上线修复"） |

## P0 已落地范围
- 帧：int32 BE 长度前缀 TCP 帧；请求/响应头 v0-v2（flexible 头含 tagged fields）。
- 编解码基元（`frame.rs`）：unsigned varint、zigzag varint、classic/compact string、
  nullable/bytes、数组计数、tagged fields 跳过/写出。
- `record.rs`：RecordBatch v2 解析 + CRC32C（Castagnoli，表驱动，poly 0x82F63B78
  reflected；标准向量 `"123456789" -> 0xE3069283` 已覆盖）。P1 起接入 Produce 写路径；
  `build_batch`/`BatchRecord` 为测试/e2e 侧编码器（broker 写路径不用它）。
- API：ApiVersions(18) v0-v3、Metadata(3) v0-v8、Produce(0) v0-v3（v3 由 P5a 解锁，
  初版 v0-v2）、ListOffsets(2) v0-v1（版本注册表在 `mod.rs`，未实现 api 回 error 35）。
  - ApiVersions 版本不支持时按规范回退 **v0 body + error 35 + 支持版本列表**；
    未知 api_key 同样回 ApiVersions v0 error 35。
  - Metadata v8 广告为最高版（刻意低于 flexible 的 v9+，避开 UUID topic id）；
    版本表含 `topic_authorized_ops`/`cluster_authorized_ops`（发出 i32 未置位哨兵值）。
  - topic 枚举 = 跨 slot 的 kind 0x0C 扫描（`catalog.rs`，有界）；分区 = child 队列
    `p<N>`/`q<N>` 的 N，topic 存在则至少回报分区 0；未知 topic 回 error 3。
- 接线：`kafka_bind`（空=关闭，backup 监听器天然不含）；P4 的 RocksMQ 风格 HTTP
  前置已落地于 [rocksmq-http.md](./rocksmq-http.md)（同一 `src/lite/` 引擎）。
  指标 `rdb_kafka_api_latency{api=...}`（与 SQL 直方图同桶型；Fetch 长轮询的
  park 时长**不计入**，acks=0 无响应帧也打点）。

## P1 已落地范围
- **Produce(0) v0-v2**（`produce.rs`；v3 随 P5a 广告解锁，见下）：请求体 v0-v2 同形
  （acks, timeout, topic 数据）；响应 v2 多回 `log_append_time=-1`（到达时钟 id 无
  Kafka append time）。
  - 分区解析：`p<N>` 优先，无则 `q<N>`（RESP 造的 `t1/q0` 即 partition 0）；都无
    → error 3 `UNKNOWN_TOPIC_OR_PARTITION`。**不自动建分区/建 topic**
    （= `allow_auto_topic_creation=false`）。
  - 写路径：每分区一个 WriteBatch + 一次 fsync（meta+N entries 同批）；entry id 用
    **到达时钟 auto_id**（Kafka record timestamp 不进 id、不保留）；`base_offset`
    = 追加前 meta len。
  - acks 语义：`0` = spawn_batch_write 异步落盘且**不写任何响应字节**（连接继续
    流式处理下一条请求）；`1`/`-1`(all) = 同步批量 fsync（单节点，无 ISR）；
    其他值 → 全部分区 error 21 `INVALID_REQUIRED_ACKS`。
- **ListOffsets(2) v0-v1**（`offsets_query.rs`）：latest(-1)=len、earliest(-2)=0、
  max_timestamp(-3) 按 latest 答（到达时钟无逐条 max）；by-ts = 首个
  `id.ms >= ts` 条目的 ordinal；查无 → `offset=-1,timestamp=-1`（error 0）。
  v0 响应分区为 `(partition,error_code,offsets int64[])`（miss=空数组），v1 为
  `(partition,error_code,timestamp,offset)`——按真实协议 schema 都含 error_code。
- **topic/offset 映射层**（`mapping.rs`）：topic_child/stream_name/validate_topic/
  partition_queue/latest_ordinal/ordinal<->id 换算/offset_by_timestamp。
- **字段对映射（权威，produce 写入 & P2 fetch 读取）**：
  - key 非 null 且无 headers → 2 对 `("k",key),("v",value)`；
  - key null 且无 headers → 1 对 `("v",value)`；
  - headers 非空 → key 对（key 非 null 时）+ v 对 + `("h",headers JSON)`；
  - null key/value 槽写哨兵对 `("__null__", b"")`（空 bytes 值仍是 `("v",b"")`，
    tombstone 可区分）；
  - headers JSON = `[{"n":"name","v":null},{"n":"name2","v":"<hex>"}]`（字节值
    hex 编码）。
- 解析错误分类（`classify_parse_err`，按 parse_batch 错误文本前缀）：含 `magic`
  → 35 `UNSUPPORTED_VERSION`；含 `compressed` → 76
  `UNSUPPORTED_COMPRESSION_TYPE`；其余（CRC 失配等）→ 2 `CORRUPT_MESSAGE`。
  topic 名非法（空/非法字符/超长）→ 17 `INVALID_TOPIC_EXCEPTION`。

## P2 已落地范围
- **Fetch(1) v0-v10**（`fetch.rs` + `fetch_records.rs`）：按请求阶梯解析/应答
  （v2+ throttle=0、v3+ max_bytes、v4+ isolation/last_stable_offset=hwm/aborted=null、
  v5+ log_start_offset=0、v7+ session_id=0/preferred_read_replica=-1/ forgotten
  topics 解析后忽略、v8+ 顶层 error=0）。
  - offset 语义：`fetch_offset` = ordinal；`== len` → error 0 + **0 长度记录集**
    （不发空 batch）；`> len` → error 1 `OFFSET_OUT_OF_RANGE`（hwm=len）；
    未知 topic/分区 → error 3、hwm=-1。
  - 记录取回：按字段对映射反向解码（0/1/2 对 → key/value 还原；带 headers 或
    超长 → 通用 JSON envelope `{"fields":[[name,hex]]}`，key=None）；batch 的
    base_offset = 请求 ordinal、first_timestamp = 首条到达 ms。
  - 预算：全局 max_bytes 是**软上限**——每个存活分区保底 1 条（floor=1），
    partition_max_bytes 正常截断；扫描按 256 条/块分块。
  - 长轮询：park 在各目标分区流的 meta 键上（先注册后扫描，防丢通知），醒来
    重扫；`total >= min_bytes.max(1)` 或 deadline 到或无存活分区即回——min_bytes
    只在"全空"情形生效；单次 park 切片上限 24h。acks 与 Fetch 共用 notify 通知。
- **提交偏移账本**（`ledger.rs`，kind 0x20）：见映射表行；`committed_ordinal`
  原样存取（无 ±1 换算），越过日志尾的提交照收（下次 Fetch 才报越界）；
  不让键"存在"（不进 meta_kinds）、envelope 0 无 TTL、走 fsync 批写路径。
  回收：分区/流删除路径（`expire::family_delete_entries` 对 STREAM_FAMILY 折叠
  OFFSET_FAMILY 窗口）连带清账本行；dump/restore 覆盖 0x20。
- **OffsetCommit(8) v0-v2**（`offsets_commit.rs`）：v1+ 官方形状
  （group+generation+member），v2 +retention_time（忽略）；**世代规则**：已有
  generation > 请求 generation（v1+）→ 该分区 error 22 `ILLEGAL_GENERATION`；
  v0 无世代字段，永不拒绝且保留已存 generation/leader。负 offset → 该分区
  error 1；提交越界不查。响应 v0-v2 同形（topics[partitions[part,error]]）。
- **OffsetFetch(9) v0-v7**：显式 topics 或 **null topics**（全组扫描，上限 10 万
  物理键，按流名排序聚合）；committed=-1 = 未提交；未知 topic/分区 → -1 + error 3，
  纯未提交 → -1 + error 0；v6+ committed_leader_epoch=-1、metadata 恒 null；
  v7 的 require_stable 在 topics 数组**之后**（尾部）解析并忽略。

## P3 已落地范围
- **组协调器**（`coordinator/`）：**纯内存 membership**（重启即失，客户端原生
  rejoin 即恢复；committed offsets 在 0x20 账本持久，不受影响）+ eager rebalance。
  - 状态机（`state.rs`，纯函数）：`Empty → PreparingRebalance(JoinGroup 阻塞或
    session 超时) → CompletingSync(SyncGroup 屏障) → Stable(Heartbeat) →(leave/
    expire/rebalance)→ …`；组转 Empty 时 generation+1。事件经 `Event` 列表由
    runtime 侧落账/通知（`Notify` 唤醒 parked join/sync follower）。
  - 成员过期：每成员 `deadline`（Heartbeat/JoinGroup 刷新）；sweep 仅剔除
    **严格过期**（`deadline < now`）成员并触发 rebalance。
  - member id：`rdb-<sanitized group>-<n>`，计数器以**进程时钟 seed**
    （微秒级，`now_seed`），跨重启不复用 id——旧进程的僵尸 id 撞名会绕过
    membership 栅栏（failover e2e 场景 b 实测捕获）。JoinGroup v1+ 空
    member_id 两段式（回 79 MEMBER_ID_REQUIRED+candidate id，KIP-394），v0 直接分配。
- **13 API 版本矩阵**（`implemented_apis()`，key 升序广播）：Produce 0-**3**、
  Fetch 0-10、ListOffsets 0-1、Metadata 0-8、OffsetCommit 0-2、OffsetFetch 0-7、
  **FindCoordinator(10) 0-1、JoinGroup(11) 0-4、Heartbeat(12) 0-4、
  LeaveGroup(13) 0-2、SyncGroup(14) 0-4、DescribeGroups(15) 0-3**、ApiVersions
  0-3。classic v0-v4（JoinGroup/SyncGroup v4 及以下不 flexible；v5+ 不开放）。
- **世代 fencing 两层**（`commit_fence`，member 检查先于 generation，同 broker）：
  - 层1 协调器 runtime：组不在内存或成员不在册 → 25 `UNKNOWN_MEMBER_ID`；
    在册但 generation 过期 → 22 `ILLEGAL_GENERATION`；组 PreparingRebalance →
    27 `REBALANCE_IN_PROGRESS`。
  - 层2 账本（组不在 runtime 或已 Empty 时降级）：行内 generation > 请求 → 22；
    v0 无世代字段跳过该层。
  - **跨重启播种**（上线修复）：组的**首次 JoinGroup** 从 0x20 账本行取
    max(generation) 作为 runtime 计数起点（屏障完成后新组世代 = 账本高水位+1），
    保证重启后的合法新组永远高于账本旧行——修复前 runtime 从 0 重计，重启前
    组已达 gen N≥2 时新组（gen 1）的每次 OffsetCommit 都被层2 拒 22，客户端需
    盲目 rejoin N-1 轮才追平（重启死区；e2e
    `restart_wipes_membership_but_keeps_offsets` 以 gen≥2 重启场景钉死）。
- **schema 决策**（自锚定 roundtrip 测试锁定）：JoinGroup v0 已含 protocol_type、
  v4 instance 在 member 之后；resp v2+ throttle 首位、v3+ 回显 protocol_type 与
  member instance。SyncGroup resp v1+ throttle 首位、v2+ protocol_name 回显。
  Heartbeat v1+ throttle 首位。**LeaveGroup v1+（非 v2）resp throttle 首位**；
  v0-v2 请求都是单 member_id（v3+ 多成员数组未开放）。**DescribeGroups 无
  leader 字段**、v1+ throttle 首位、v3 member instance；未知组 → 状态 "Dead"
  + error NONE。leader/follower 由服务端按 `st.leader` 判定（wire 无从区分）：
  leader 的 SyncGroup 分发 assignment 并置组 Stable；follower 在屏障期间
  parked，屏障完成或超时被唤醒。

## 真实 SDK 实测（P5a，2026-09）

用 **confluent-kafka 2.15.1（librdkafka 2.15.1）** 跑通全链路（场景固化于
`scrtips/e2e_scenarios/scenario_kafka_sdk.sh`，6/6：metadata、produce、
assign 消费、group 协议消费、commit/resume、rebalance；P5b 后扩至 7/7，
+压缩 produce/消费 roundtrip，见下节）。librdkafka 实际
协商版本：Produce v2→**v3**、Fetch v10、OffsetFetch v7（flexible）、
JoinGroup v4、SyncGroup v3、Heartbeat v3、ListOffsets v1、Metadata v8。
SDK 实测暴露并已修复的兼容性 bug（每条均有 e2e/单测或场景钉住）：

- **Produce v3 广告解锁 MSGVER2**：librdkafka 的 feature map 要求
  `Produce>=3 AND Fetch>=4` 才发 magic 2 RecordBatch；只广告 v2 时回退
  MessageSet v1 造出 underflow——v3 响应在 responses 数组之后还有 trailing
  `throttle_time_ms`（v1+），漏写导致 SDK 请求体解析错位。
- **KIP-394**：空 member_id 的 JoinGroup 必须回 **79 MEMBER_ID_REQUIRED**
  +新 id；回 25 会让 librdkafka 清空 id 无限重试（死循环）。
- **librdkafka flexible 请求头 = 经典 i16 client_id + tagged 尾**（非
  compact client_id）——`parse_req_header` 现有形状正好匹配（proxy 实证）。
- **ApiVersions v3 响应**：响应头钉在 v0/classic（无 tagged 字节，同
  Kafka `ApiKeys.responseHeaderVersion`）；且 `throttle_time_ms` 在
  api_keys 数组**之后**（v0 客户端在数组处停止读取）。
- **FetchResponse 顺序**（官方 schema）：throttle v1+、top-level
  error_code v7+ 在 session_id **之前**、partition 内
  `preferred_read_replica` 是 **v11+** 字段（classic v10 不写）。
- **Fetch 请求**：`current_leader_epoch`（v9+）在 fetch_offset 之前、
  log_start_offset v5+。
- **Metadata v8 三 bool**：allow_auto + include_cluster_authorized（8-10）
  + include_topic_authorized（8+）；响应 partition 的 leader_epoch v7+。
- **OffsetFetch v6+ flexible**：请求/响应全 compact 帧 + tag 尾；响应
  throttle v3+、CommittedLeaderEpoch v5+、top-level error（v2-7）在
  topics 数组之后。
- **SyncGroup v3** 经典分支漏 `group_instance_id`（v3+ nullable string，
  assignments 之前）→ SDK leader 端 assignment 错位。
- **DescribeGroups v3**：`authorized_operations` 断言 + 解析顺序
  （groups 数组先读、authorized_operations bool 在数组之后）。

## 压缩 produce（P5b，2026-09）

**只做 produce 侧解压**：收到压缩 RecordBatch 即解出内层 records、走与
未压缩完全相同的 Lite 追加路径；Fetch 恒回自建**未压缩** batch
（attributes=0），不重压缩。编译期由 cargo feature `kafka-codecs` 门控
（flate2 rust_backend + snap 1 + lz4_flex 0.11，三个 optional 依赖；
**不在 default/full**——默认构建零新增依赖，`cargo tree` 可证）。开启方式：
`cargo build --release --features kafka-codecs`。

- 支持格式（`src/kafka/codec.rs`，attributes 压缩位 1/2/3）：
  - **gzip**：完整 gzip member（`GzDecoder`）；失败再试裸 deflate
    （`DeflateDecoder`，兼容不带 gzip 头的流）。空 blob → 空记录集。
  - **snappy**：raw 流优先（librdkafka 即此形状）；带 xerial magic
    （`0x82 S N A P P Y 0`）则按 4B 长度前缀分块循环解。
  - **lz4**：标准 frame（`FrameDecoder`）；失败退**宽容手工 frame 走查**
    （跳过 HC/块校验/尾部 content checksum，拒绝 dict-id/非法版本），
    兜住 librdkafka 帧里 content-checksum 标志位不实/尾 4 字节的怪癖。
- **zstd（4）与 5-7 不支持**：直接回 76 `UNSUPPORTED_COMPRESSION_TYPE`
  （zstd 依赖 C 库，刻意排除；注意实测该 SDK 构建的 zstd produce 上线即
  attributes=0 未压缩，SDK 探不到 zstd 拒绝路径，由 Rust e2e 钉住）。
- 内层 blob = 压缩的 records 区（`bytes[61..]`，61B 头之后），条数取外层
  头 `records_count`，offset/timestamp delta 相对外层 base——Lite 路径
  本就丢弃时间戳（到达时钟），故只取 key/value/headers。
- 错误分类不变（`classify_parse_err`）：codec 报错文本刻意避开 `magic`
  一词（lz4 用 "frame signature mismatch"），坏流 → 2 `CORRUPT_MESSAGE`
  （裸 deflate 兜底可能把垃圾"解成"垃圾，随后在记录截断处报错，仍归 2）。
- 测试：`src/kafka/codec_tests.rs`（6 例，内嵌真实 librdkafka 三格式
  wire fixture）、`produce_tests.rs` feature 门控 Lite 追加断言、
  `tests/kafka_codec_e2e.rs`（真二进制：三格式 ×5 条 roundtrip、zstd→76、
  坏 gzip→2、XRANGE 键值断言）；SDK 场景第 7 步（30 条/codec，
  `batch.num.messages=10, linger.ms=200` 逼 librdkafka 真正成批压缩；
  无 feature 构建 DR 全 76 → 步骤自 SKIP，不判失败）。

## 阶段表

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 骨架 + 帧编解码 + ApiVersions/Metadata + bind 接线 | 已落地 |
| P1 | Produce(0) v0-v2 + ListOffsets(2) v0-v1 + topic/offset 映射层（无压缩） | 已落地 |
| P2 | Fetch(1) v0-v10 读路径（offset=ordinal）+ OffsetCommit(8) v0-v2/OffsetFetch(9) v0-v7 + kind 0x20 提交偏移账本 | 已落地（ListOffsets v2+ 顺延至 P3） |
| P3 | 组面：FindCoordinator/JoinGroup/SyncGroup/Heartbeat/LeaveGroup/DescribeGroups + eager rebalance + 世代 fencing；ListOffsets v2+ 与 XTRIM/XDEL 守卫顺延 | 已落地（membership 纯内存） |
| P4 | rocksmq_bind：RocksMQ 风格 HTTP 前置（原 RocketMQ remoting 方案改为极简 HTTP，见 [rocksmq-http.md](./rocksmq-http.md)） | 已落地 |
| P5a | 真实 SDK（confluent-kafka/librdkafka）实测 + 兼容性修复 + `scenario_kafka_sdk.sh` | 已落地 |
| P5b | 压缩 produce 侧解压：gzip/snappy/lz4，cargo feature `kafka-codecs`（默认关）；fetch 恒未压缩 | 已落地 |
| P5c | rdb-bench 工况 `kafka-prod`/`kafka-fetch`（手写 Produce v2/Fetch v4 客户端压 front）+ `scenario_kafka_bench.sh` | 已落地 |
| P5 | ~~压缩 codec~~（zstd 刻意排除，见偏差清单）、性能打磨 | 已落地（经 P5a/P5b/P5c 收口） |

## 偏差清单（对标准 Kafka）
- **单 broker，无 ISR**：Metadata 恒报 node 1；replicas=isr={1}，offline 恒空，
  controller 恒 1，cluster_id=`rdb-lite`。
- **acks=all = 单节点 fsync**：与 Lite 写路径同（meta+entry 一个批量 fsync），没有
  副本确认。acks=0 不回响应字节（协议行为），连接继续处理后续请求。
- **压缩仅 produce 侧、且是可选 feature**：默认构建 RecordBatch attributes
  压缩位非 0 即拒（error 76 `UNSUPPORTED_COMPRESSION_TYPE`）；开
  `kafka-codecs` feature 后 produce 侧解压 gzip/snappy/lz4（见 P5b 节），
  zstd 与 5-7 恒拒 76；Fetch 恒回未压缩 batch（attributes=0），无重压缩。
- **record timestamp 不保留**：entry id 用到达时钟（auto_id），Kafka record 的
  timestamp（first/delta）落库即丢弃；ListOffsets by-ts 查询按**到达时间**而非
  producer 时间戳。
- **不自动建 topic/分区**：Produce/ListOffsets 到未知分区回 error 3（等价
  `allow_auto_topic_creation=false`）；topic 须先经 RESP XADD（或 P2+ 的管理面）
  建立。
- **offset=ordinal 语义**：分区偏移是活跃（未 XDEL/XTRIM）条目集内的序数，trim 之后
  与物理条目不再一一对应；非 Kafka 的"不可变日志位移"。ListOffsets 的
  earliest/latest 已按 ordinal 语义实现（P1）；Fetch 的 offset 越界钳制 P2 细化。
- **禁用 XTRIM/XDEL on kafka 流**：P2 起对 kafka 面创建的流拒绝 XTRIM/XDEL
  （保护 ordinal 稳定性）；纯 Lite 流不受影响。
- **无 ACL**：authorized_ops 字段恒为未置位哨兵（i32::MIN）。
- **topic 枚举有界**：全量 Metadata 走全库有序扫描（上限 10 万物理键，超出则少报
  而不是阻塞连接）。
- **客户端兼容面**：目标是 kcat/franz-go/librdkafka 类客户端的握手与元数据；不支持
  SASL/TLS（连接即信任——连接配额/空闲超时/帧上限见"上线修复"，但**无鉴权**，
  仍要求受控网络或回环绑定部署）。
- **无增量 Fetch 会话**：session_id 恒 0、session_epoch 解析即忽略，
  forgotten_topics_data 只做形状校验；客户端退化为全量拉取（正确但不省带宽）。
- **Fetch 无事务面**：aborted_transactions 恒 null、log_start_offset 恒 0、
  committed_leader_epoch/log_start_offset(Fetch v5+) 恒 0/-1 哨兵；isolation
  级别忽略（无事务）。
- **Fetch 预算是软的**：全局 max_bytes 只约束总量，不饿死分区——每个存活分区至少
  回 1 条（floor=1）；min_bytes 仅在所有分区皆空时决定等待与否。
- **提交偏移 fencing 分两层、不再只看账本**：P3 起组在协调器 runtime 内时按
  membership+generation 栅栏（member 先于 gen 检查）；仅组不在内存或已 Empty
  时降级为账本 generation 比较（v0 无字段跳过）。不校验 leader、不阻塞
  producer、无 txn/offset-metadata 存储（metadata 恒回 null）。
- **账本回收缺口**：RENAME（`move_family`）不搬 0x20 账本行（STREAM_FAMILY 之外
  的家族搬运本就未覆盖）；另外 0x1A 行首空格误读的老问题同样适用于 0x20
  （`classify` 按 kind 字节判定，历史 0x1A 空格 bug 的窗口见 ds/codec 注记）。
- **XTRIM/XDEL 守卫顺延 P3**：P2 交付时对 kafka 面流尚未拒绝 XTRIM/XDEL
  （ordinal 稳定性风险仍在，偏差清单保留）。

## 上线修复（2026-09-22，P0 三项）
订阅/消费能力上线评估后修的阻断项（详见 `features/changelog/2026-09-22/kafka-launch-p0.md`）：
1. **重启 generation 死区**：组首次 JoinGroup 从账本播种 runtime 世代（见 P3
   "跨重启播种"）；e2e 以重启前 gen≥2 钉死回归。
2. **通配绑定广告 localhost**：新增 `kafka_advertised_host/port`，Metadata/
   FindCoordinator 的 brokers/coordinator 行优先采用（`handshake::advertise`）；
   e2e `advertised_overrides_beat_wildcard_bind`（0.0.0.0 绑定 + 覆盖）。
3. **连接面裸奔缓解**（配额级，鉴权仍缺、偏差清单保留）：进程级连接上限
   `kafka_max_connections`（默认 4096，超限 accept 即断）、空闲读超时 10min
   （对齐 Kafka connections.max.idle.ms）、单帧上限 100MiB→16MiB、帧缓冲跨请求
   复用（稳态零分配）；e2e `connection_cap_closes_excess_sockets`。
4. **指标修正**：Fetch 长轮询 park 时长不计入 `rdb_kafka_api_latency`（不再把
   客户端 max_wait 推向 +Inf 桶）；acks=0 无响应帧路径也打点。

## 风险注记（显式接受）
- **单节点持久性是既知风险**：当年否决 Kafka front 的理由仍然成立——Kafka 客户端
  默认预期 acks=all/ISR/幂等，而 rdb 数据面（含 Lite 元数据）不经 raft 复制。接受
  Kafka 连接意味着"看起来像 Kafka、故障时没有 ISR"。本路线以**协议前置 + 显式偏差
  文档**落地，不承诺复制语义；若需真 ISR，前提仍是数据面复制（见 mq-lite.md 路线
  决策）。
- 实测环境无 kcat；真实客户端覆盖由 P5a 的 confluent-kafka/librdkafka 实测补齐
  （见上节），P0/P1 的 wire 正确性另由
  `tests/kafka_wire_e2e.rs` + `tests/kafka_produce_e2e.rs`（真实二进制 + RESP 造流
  + 裸 TCP 手写帧，公共夹具在 `tests/kafka_front_common/`）与 lib 单测覆盖。

## 实现
- `src/kafka/`：`mod.rs`（注册表+bind/serve）、`frame.rs`（wire 基元）、`record.rs`
  （RecordBatch v2 解析 + CRC32C + `build_batch` 编码器）、`errors.rs`、
  `handshake.rs`（ApiVersions/Metadata）、`catalog.rs`（topic/分区目录）、
  `mapping.rs`（topic/offset 映射）、`produce.rs`（Produce v0-v3 写路径）、
  `codec.rs`（P5b gzip/snappy/lz4 解压，feature `kafka-codecs` 门控）、
  `offsets_query.rs`（ListOffsets v0-v1）、`fetch.rs`+`fetch_records.rs`
  （Fetch v0-v10，解析/预算/长轮询与记录反解码分文件）、`ledger.rs`
  （kind 0x20 提交偏移账本）、`offsets_commit.rs`（OffsetCommit/OffsetFetch）、
  `conn.rs`（连接循环 + 13 API 分发）；内嵌单测 `*_tests.rs`
  （codec/produce/fetch/offsets_commit）。
- P3 组协调器 `src/kafka/coordinator/`：`mod.rs`（runtime、member id、
  `commit_fence`）、`state.rs`（纯状态机）、`session.rs`（锁+sweep+Notify 唤醒）、
  `join.rs`（JoinGroup/SyncGroup 长等待）、`api.rs`（FindCoordinator/Heartbeat/
  LeaveGroup/DescribeGroups wire）、`group_api.rs`（JoinGroup/SyncGroup wire）；
  单测 `state_tests.rs`/`api_tests.rs`/`group_api_tests.rs`（状态机转移表 +
  逐版本 roundtrip）。
- 配置：`kafka_bind`、`kafka_advertised_host/port`（Metadata/FindCoordinator 广告
  覆盖，空/0=由 bind 推导）、`kafka_max_connections`（0=默认 4096）、
  `rocksmq_bind`（`conf.rs`，空=关闭）。
- 指标：`rdb_kafka_api_latency`（`monitor.rs`；conn 侧观测排除 Fetch park 时长、
  覆盖 acks=0 无响应路径）。
- 测试：`tests/kafka_wire_e2e.rs`（P0 握手/元数据）、`tests/kafka_produce_e2e.rs`
  （P1 Produce/ListOffsets + 错误路径）、`tests/kafka_fetch_e2e.rs`
  （P2 Fetch 阶梯/截断/长轮询）、`tests/kafka_offsets_e2e.rs`
  （P2 提交偏移：世代规则 + kill/respawn 持久化）；公共进程夹具
  `tests/kafka_front_common/`（含 `kill_now`/`respawn` 重启支持，`groups.rs`
  组协议 helpers）；`tests/kafka_group_e2e.rs`（P3 两段式 join→barrier→分发→
  Describe Stable→commit→Leave→rejoin→两层 fencing）；`tests/kafka_group_
  failover_e2e.rs`（session 超时 sweep 拉回 rebalance、重启丢 membership 但
  offsets 持久 + 跨重启 member id 不复用）；`tests/kafka_codec_e2e.rs`
  （P5b 压缩 roundtrip，feature 门控）；场景脚本
  `scrtips/e2e_scenarios/scenario_kafka_sdk.sh`（P5a 真实 SDK，7/7）与
  `scenario_kafka_bench.sh`（P5c bench 工况）。
