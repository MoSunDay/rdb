# MQ P3 按需池入池：三条台账外盲点登记（服务端过滤 / KIP-429 / 按 key 回查）

Commit: 待回填（docs-only 台账批，落提交时补 sha；台账三件套 00/05/本文件同批）

> 性质：docs-only 台账同步（无代码变更、无行为变更）。

## 触发与分诊
2026-10-08 台账复核（P3 回填收尾后的收口检查）发现：A/B 级车道 + 14/15 出池后，
已知差距已收敛到 `plans/2026-10-06-mq-gap/05-p3-pool.md`，但仍有三条**池与显式
不做清单均未登记**的盲点。按 §5.1 入池规则分诊（非 §1.3 立项——复核没有触发
信号原文，不满足附录 A 的登记前提；三条全部回到"等信号"状态）。分诊落位：
三级分诊表（§1.5）的按需池层——有价值 + 可写出可观测触发条件 + 不紧急。

复核同时确认的**非盲点**边界（不动台账）：
- #3（produce timestamp 保留）维持 §2 唯一原生存留行，再评条件不变；
- ISR / 多副本 MQ 数据面维持 §4.1「C-挂起」，不占池编号；
- §4 显式不做清单（终局 11 条）无新增、无复议。

## 逐条入池评估（触发条件 / 落点 / 规模）

| # | 条目 | 现状证据 | 触发条件 | 预估落点 | 预估规模 |
|---|---|---|---|---|---|
| 16 | 服务端消息过滤 | Lite 面定位 RocketMQ 5.5 Lite 风格（`features/mq-lite.md` 定位节），但投递路径无任何过滤；现仅 XPENDING 的 IDLE/consumer 查询过滤 | 消费方工单给出"客户端全量拉取后本地丢弃"的带宽/延迟数据与订阅子集占比，诉求服务端按 tag/属性过滤投递 | 新 `src/lite/filter.rs` + `src/lite/read_xread.rs` 投递判定；属性载体若动 envelope 须与 #3 同批定演进 | 中（若动 envelope 接近上限） |
| 17 | KIP-429 协作式 rebalance | 组协调器仅 eager：`src/kafka/coordinator/state.rs` rebalance kick 清空全组成员 joined 位、JoinGroup 只携带成员单策略（非协商集）；偏差此前未文档化 | 消费端以 cooperative 策略（CooperativeStickyAssignor 等）上线后出现重平衡停顿或混编报错，附客户端配置与复现；触发后**先复核**服务端必须改的部分（§3.5） | `src/kafka/coordinator/`（join/state/session）+ `features/kafka-front.md` | 中（状态机分支 + 长等待路径 + e2e） |
| 18 | 按 key 的消息回查 | 回查仅按 id（`src/rocksmq/range.rs` `/range`）与按时间（ListOffsets by-ts，#3 未决时按到达时钟）；业务 key 无索引 | 对账/排障工单只有业务单号（消息 key），需定位所在 id/offset，附工单样例与保留窗口 | 新二级索引 kind（key → id，族删除登记参照 0x20 账本先例）+ `src/rocksmq/query.rs` | 中（写路径索引维护 + 新 kind + 查询面 + e2e） |

三条的触发条件均按 §2.1 三判据（可观测 / 可复现 / 有主体）自检通过；分组纪律、
与工作包交付物的交叠核对和 #16↔#3、#17 的先复核后立项约束在 `05` §3.5。

## 文档同步（入池三件套 + 交叉引用）
- `plans/2026-10-06-mq-gap/05-p3-pool.md`：§2 入池注记 + #16–#18 三行、§2.2 交叠
  看板三行、§3.5 分组备注、§1.2 信号类别表扩展；
- `plans/2026-10-06-mq-gap/00-gap-matrix.md`：C 级节追加 2026-10-08 入池注记
  （三行索引，指回 `05` §2）；
- `plans/2026-10-06-mq-gap/README.md`：状态板 open 列表 + 头部注记；
- `features/kafka-front.md`：风险注记新增"rebalance 仅 eager"偏差条（#17 的文档化
  清欠——复核前该偏差未在任何文档提及）；
- `features/mq-lite.md`：关联节交叉引用 #16/#18；
- `agents.md` / `plans/index.md`：计划索引指针同步。

## 验证
- 台账一致性：00 号矩阵 C 级存留行（#3 + #16–#18）== `05` §2 存留行（4 行）；
  池编号连续无重号（3、16、17、18）；
- 行限：`05-p3-pool.md` 332 → 359 行（<800）；本条目为新增文件（<400）；
- 无代码变更：`cargo check --workspace` 通过（工作区不受影响），既有 e2e/
  单测面零改动。

## 第二轮（同日复核）

### 触发与分诊（换尺子：客户端运维工具 + 横切面）
第一轮以"三面语义对照"为尺；第二轮换尺子——以**客户端运维工具**（Kafka-UI/
AKHQ/CMAK、官方 bin 脚本、redis-shake 等迁移/复制工具）与**横切面**（metrics/
告警、配置键）为尺再过一遍，发现六条池与显式不做清单均未登记的盲点。分诊口径
与第一轮一致：按 `05` §5.1 入池（登记**非立项**——无触发信号原文，六条回到
"等信号"状态；#24 例外，登记的是死键处置）。同轮复核还产出两组非池结论：
显式不做清单 +5 行（见下）与同日两条缺陷（DUMP 折 0x1D、FLUSHDB 逐出协调器，
走缺陷修复车道，**另见同日缺陷修复条目**，本条目只留指针）。

### 逐条入池评估（#19–#24）

| # | 条目 | 现状证据 | 触发条件 | 预估落点 | 预估规模 |
|---|---|---|---|---|---|
| 19 | kafka admin 运维四件（合并条目） | OffsetDelete(47)/DeleteRecords(21)/DescribeLogDirs(35)/DescribeCluster(60) 均未广告（`advertised_apis` 注册表无此 key），Java AdminClient 对应方法即 UnsupportedVersionException；援引 `05` §3.1 admin 面"一并立项"先例合并为一行 | 运维侧引入 Kafka-UI/AKHQ/CMAK 或官方脚本（`--delete-offsets`/`kafka-delete-records`/`kafka-log-dirs.sh`）的工单，附工具版本与报错摘录 | 新 `src/kafka/admin_ops.rs` + conn 分发（广告集同批扩展） | 中（4 个 wire 面 + 独立 e2e） |
| 20 | XSETID | 动词缺失；且与既有 XGROUP SETID（`src/lite/group.rs:495`，已实现）联动存在语义洞：外部重建流后 last_id 不回退，新条目可低于组回退水位而被 `>` 读者永久跳过 | 迁移/复制工具（redis-shake 等）接入或外部重建流运维诉求，附工具名与复现步骤 | `src/lite/append.rs` / 新 `src/lite/setid.rs`（两处回退语义一并定界） | 小 |
| 21 | XREADGROUP NOACK | 选项缺失：`src/lite/read.rs:564-574` 文法无此分支，投递一律写 PEL 行 | 火焰/旁路免 PEL 消费的工单（临时排查消费不占 PEL、不进重投链路），附消费拓扑 | `src/lite/read.rs` | 小 |
| 22 | kafka Fetch 消费计数 | produce 计入 `rdb_lite_messages{op=add}`（`src/kafka/produce.rs:218`）但 fetch 零打点（`src/kafka/fetch_records.rs` 无 observe），消费侧速率在 metrics 不可见 | 吞吐对账/消费侧速率监控工单（produce/fetch 计数对不上） | `src/kafka/fetch_records.rs` + `src/monitor.rs`（复用 op=read 或新 label，与 produce 口径对齐一次定名） | 小 |
| 23 | 队列深度/consumer-lag/组数量 gauge | 现有仅 PEL backlog 与 DLQ depth（`src/monitor.rs:24-30`），无 entry 深度、lag、组数；并收 02 号计划"暂存深度 gauge"未收尾意图（`plans/2026-10-06-mq-gap/02-delay-messages.md:177-178`，随本条并入，不再单独立项） | 容量规划/消费滞后告警工单（需要按流/组的深度与 lag 曲线） | `src/monitor.rs` + `src/lite/mod.rs` 刷新循环（点读聚合须评估扫描成本） | 中（多 gauge + 刷新成本定界） |
| 24 | `allow_ip_list` 死键处置 | `src/conf.rs:40-41` 解析但无执行点、文档零登记（静默偏差：自 Go 归档实现原样搬来的配置键——Go 侧同样只解析不执行，`archive/go/internal/conf/conf.go:26`，两边都是死键） | 公网暴露后的访问控制诉求（或评审决定删键——登记处置结论即可） | `src/conf.rs` + 接入层或删除 | 小 |

六条触发条件均按 `05` §2.1 三判据（可观测 / 可复现 / 有主体）自检通过；#24 的
触发条件自带两个出口（补执行点或删键），是池内唯一不以"实现能力"为默认出口的
条目。分组纪律与交叠核对在 `05` §3.6。

### 显式不做清单 +5 行（同轮分诊）

| 项 | 理由 |
|---|---|
| ElectLeaders(43) | 单节点无 leader election 对象 |
| Alter/ListPartitionReassignments(45/46) | 无副本重分配对象——与 ISR 挂起同类，前置是数据面复制 |
| DescribeQuorum(55) | 控制面是 openraft，不作为 kafka API 暴露 |
| UnregisterBroker(64) | 单 broker 无注销对象 |
| KIP-848 新组协议（ConsumerGroupHeartbeat(65)/ConsumerGroupDescribe(66)） | 经典组协议已覆盖目标客户端，新协议面不开放（与 flexible 版本 / Metadata v8 封顶同口径） |

五行的完整依据列与镜像表：`05` §4 + 00 号矩阵"显式不做"表。

### 文档同步（第二轮三件套 + 指针）

- `plans/2026-10-06-mq-gap/05-p3-pool.md`：§2 第二轮入池注记 + #19–#24 六行、
  §2.2 交叠看板六行、新增 §3.6 分组备注、§4 +5 行、§1.2 信号类别表补编号；
- `plans/2026-10-06-mq-gap/00-gap-matrix.md`：C 级节追加第二轮入池注记（六行
  索引，指回 `05` §2）+ "显式不做"表镜像 +5 行；
- `plans/2026-10-06-mq-gap/README.md`：状态板 open 列表 + 头部注记扩为两轮合计；
- `plans/index.md` / `agents.md`：2026-10-08 入池指针扩为两轮合计（#16–#24）。

### 文档清欠（同轮落）

- `COMPAT.md`：Lite 动词面 XGROUP 枚举补 `SETID`（此前漏登已实现子命令）+ 补
  ENTRIESREAD 偏差行（Redis 7 参数不收，`src/lite/group.rs:495` arity 门）；
- `features/mq-lite.md`：可观测性节补漏登指标 `rdb_lite_streams{kind=live|reaped}`、
  `rdb_lite_offset_dirty`、`rdb_lite_messages{op=dlq_fail}`；
- `plans/2026-10-06-mq-gap/00-gap-matrix.md`：Batch 1 回归门槛处 `backup_surface_common`
  措辞加注——"只读面相等断言"实为 allowlist 集合相等（backup store 现无数据面写入
  路径，多副本数据面根因挂 `05` §4.1「C-挂起」）；
- `features/e2e-coverage.md`：追加同日缺陷修复两条 e2e 登记（`lite_delay_migrate_e2e`
  新文件 3 用例；`kafka_group_e2e` 追加 flushdb/ListGroups 无幽灵组用例，标注追加
  进既有文件）。

### 缺陷两条指针（另见同日缺陷修复条目）

- **DUMP 折 0x1D**：DUMP/RESTORE（同名与改名）此前不搬延迟暂存行——修复 + 3 用例
  `tests/lite_delay_migrate_e2e.rs`（缺陷细节、root cause 与修复叙事见同日缺陷
  修复条目，本条目不重复）；
- **FLUSHDB 逐出协调器**：清库未逐出 kafka 组协调器内存态，ListGroups 残留幽灵组
  ——修复 + `kafka_group_e2e` 追加用例（同上，见同日缺陷修复条目）。

两条均为**已落地面的行为缺陷**（走缺陷流程，不经池分诊），与本轮入池六条性质
不同：池条目是"缺能力等信号"，缺陷是"已承诺面不一致即修"。

### 验证（第二轮）

- 台账一致性：00 号矩阵 C 级存留行（#3 + #16–#24）== `05` §2 存留行（10 行）；
  池编号连续无重号（3、16–24）；显式不做 00 号镜像表与 `05` §4 同步 +5 行；
- 行限：`05-p3-pool.md` 359 → 406 行、`00-gap-matrix.md` 310 → 335 行（均 <800）；
  本文件 46 → 126 行（<400）；
- 无代码变更：本轮为纯文档批（`src/`、`tests/` 零改动）；两条缺陷修复由同日
  缺陷修复条目承载，e2e 登记仅为台账指针。
