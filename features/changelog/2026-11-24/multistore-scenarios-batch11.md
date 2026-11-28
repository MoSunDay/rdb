# 多存储场景批次 11：#111–#130 二十例 e2e（2026-11-24）

承接批次 1–10（2026-11-19/20/21/22，累计 110 例），批次 11 再扩 20 例，编号
#111–#130，不改 `src/`、不改 `tests/common/` 与 `tests/kafka_front_common/`，只读复用
`tests/scenario_common/` 夹具。每例仍是一个真实业务故事横切多个存储面（KV/JSON/
Vector/FT/lite-MQ/SQL/RocksMQ-HTTP/ES/Kafka/S3），断言同一份数据在不同面之间的可见性
与一致性。本批首次为每个场景钉死**性能预算**：新增 `tests/scenario_common/perf.rs`
（`now`/`within`/`at_least`/`rate`/`scenario`）与 `s3_extra.rs`（`s3_call_headers`/
`header_value`，Range/HEAD 需要请求头而 `s3_call` 无此面），每个用例体在 `serial_guard`
之后立即取 `t0 = perf::now()`，至少一处 `perf::within(...)`，结尾 `perf::scenario(tag, t0)`
（整例上限 120s，≥10× 最慢整例基线：复审修正后两轮全量最慢 #127 5.56s / #130 5.58s，
120s 为其 ~21×；修正前 #130 因 `asking_cmd` 行数预算错而固定白等 5s，录得 10.56s；分项
`within` 是热块回归门，按热块基线单独设定、不套 10×；只有长轮询「真的停住」的断言用
`at_least` 下界）。全量日志因此同时是性能证据表：47 条 `perf[...]`，最慢 #130 5.58s /
#127 5.56s、#128 4.92s（一轮全量负载下 #127 consumer-gc 分项 2.45s，另一轮 0.61s），
其余均 <2.6s（#116 补 20 次 `HGETALL` 读回后 2.41–2.53s）。

## 场景矩阵

| # | 文件 | 平面 | 断言要点 |
|---|---|---|---|
| 111 | `scenario_video_streaming_range_e2e.rs` | S3+MySQL+RESP | 3 个 4KiB 分片 PUT；20 次 Range GET 字节精确（18 段闭区间 + `bytes=-512` 后缀 + `bytes=3000-` 开区间）且 `Content-Range` 精确；10 次 HEAD 只回 size；3 次带分隔符 LIST 折出 CommonPrefix；`bytes=9999-` 416 `InvalidRange`；SQL 分片登记表总额 = RESP `HSET` 尺寸缓存 |
| 112 | `scenario_telecom_cdr_batch_spool_e2e.rs` | RocksMQ+MySQL+RESP | 一次 `/produce_batch` 入 50 条（id 严格递增）；`/range` 回放 50 条 base64 载荷逐字节；组 `billing` 消费 10 + `/ack_batch` 清 PEL；`XLEN cdr112/q0` == 50；`delay_ms=700` 普通读不可见但唤醒长轮询（停 ≥600ms、上限 3s）；空通道停满 `wait_ms`；SQL 审计台账 |
| 113 | `scenario_event_platform_topic_admin_e2e.rs` | Kafka+MySQL+RESP | CreateTopics 物化 `events` 3 分区；重复创建 36；CreatePartitions 扩到 5；DescribeConfigs 回文档化的 4 项 TOPIC 配置；ListOffsets/OffsetForLeaderEpoch 读空水位；DeleteTopics 退役后 Metadata 无分区；RESP `XINFO TOPICS events` == 5（`p0..p4`）；10 轮 admin 往返；SQL topic 登记表 |
| 114 | `scenario_consumer_fleet_churn_e2e.rs` | Kafka+MySQL+RESP | 空 member id JoinGroup 79（回执 generation 0）、重入 generation 1 自任 leader、SyncGroup 回 `[p0]`、Heartbeat、OffsetCommit v2/OffsetFetch v0、ListGroups v1 Stable；已提交组的流 `XTRIM` 被提交位移台账守卫拒绝，LeaveGroup+DeleteGroups 后放行且 `MAXLEN 0` 回 3；10 个组同样 join/sync/heartbeat/leave/delete；SQL 台账 |
| 115 | `scenario_security_log_drill_e2e.rs` | MySQL+ES | 一次 `_bulk` 100 条（errors:false、100 created）；`query_string` 隐式 AND、`match_phrase` 位置邻接、keyword `prefix`、`_source` includes 精确剪枝 / excludes 丢字段；计数与 Rust 侧同一语料统计及 SQL `COUNT(*)` 三方一致；HEAD 200 → DELETE 200 → HEAD 404 |
| 116 | `scenario_product_search_ab_e2e.rs` | RESP(FT)+MySQL+ES | 同 20 件商品 = HASH + FT 索引 + SQL 表：match-all 每个命中的 doc id 作 HASH 键 `HGETALL` 读回，字段集与 FT 文档逐字段相等（顺序无关）；`ON HASH PREFIX 1` 与 `TAG` 均被拒（unknown option / unknown field type），真实面是 `FT.CREATE SCHEMA …` + `FT.ADD` JSON；match-all + `SORTBY price`、`RETURN` 按请求序投影、`RETURN 0` → `{}`、`HIGHLIGHT … TAGS`、`WITHSCORES` 4 位小数；ES `term` 与 SQL `WHERE` 同判红色商品 |
| 117 | `scenario_bank_reconciliation_fullouter_e2e.rs` | MySQL+S3 | FULL OUTER JOIN 精确 7 行（3 匹配 + 两侧各 2 未匹配 null-extended）；`USING (id)` 两侧展开、id 列出现两次；反连接 `WHERE b.id IS NULL` → 2 行；INTERSECT = 匹配集、EXCEPT = 未匹配集；join 上裸 `ORDER BY id` 歧义 → 1054 `ER_BAD_FIELD_ERROR`（非 1064）；对账报告 S3 PUT→GET 字节一致；200 join + 3 set op <2s（≥50 stmt/s） |
| 118 | `scenario_visual_sql_ann_e2e.rs` | MySQL+ES | 20 行 `VECTOR(4)` 三种度量（euclidean/cosine/dot）各算各的邻居序；元数据拼作 `vector(4)`；`STRING_TO_VECTOR`=`TO_VECTOR`、`VECTOR_DIM`、`VECTOR_TO_STRING` 往返；`CREATE VECTOR INDEX … WITH (METRIC 'euclidean', NPROBE 1)` 在 `SHOW INDEX` 里是 BTREE 行且 KNN 答案不变（ANN==精确）；ES knn 同语料回同一 top-5 id |
| 119 | `scenario_iot_columnar_analytics_e2e.rs` | MySQL+S3 | 1000 条传感读数分块多行 INSERT 进 `ENGINE=columnar`；聚合跨 segment 并集（COUNT/AVG 打印精度/MIN/MAX/按设备 GROUP BY）；append-only 契约：UPDATE/DELETE/CREATE INDEX → 1235 且消息点名 columnar；`SHOW CREATE TABLE` 渲染 `ENGINE=columnar`；TRUNCATE 清空保形；报告 S3 往返字节一致 |
| 120 | `scenario_saas_dual_engine_turso_e2e.rs` | MySQL+turso | 同一 REAL 节点双引擎：host rocks 默认目录 + `turso_crm` 物化为 turso library（文件 + raft 目录项）；`SHOW DATABASES` 合并两引擎；turso 会话跑 AUTO_INCREMENT + `LAST_INSERT_ID()`、JOIN、UNION、CTE；T3 allowlist 的 6 条仍拒形状 1235（响亮、绝不静默落到 host 表）；`USE turso_nope` 1049 |
| 121 | `scenario_order_history_prepared_pagination_e2e.rs` | MySQL+ES | 一条 prepared 语句 `?` 绑 LIMIT/OFFSET 翻两页（不相交、有序、无空洞）；`LIMIT 0` 空页；负 LIMIT 与 `LIMIT ?, ?` 双占位 → 1064；预算内重执行 20 次；`DEALLOCATE PREPARE`（COM_STMT_CLOSE）后 stale handle 再执行只会断连（驱动 `Io` 错误；`Server(1243)` 或任何 Ok 行集均判失败）；ES `from`/`size` 同窗交叉核对 |
| 122 | `scenario_hotel_row_lock_hold_e2e.rs` | MySQL | `SELECT … FOR UPDATE` 锁住命中 pk 直到 COMMIT；第二会话同_row 锁读快失败 1205 `Lock wait timeout exceeded`，另一行仍可锁；COMMIT 释放后重试落地；`FOR SHARE` 两会话共存；`NOWAIT`/`SKIP LOCKED` → 1235；10 轮 hold/release <2s |
| 123 | `scenario_regulatory_typed_report_e2e.rs` | MySQL+ES | 类型化报表 BIGINT/BOOLEAN/BLOB/DATE/DATETIME/DECIMAL(10,2)：DESCRIBE 精确类型名、BOOLEAN 回 1/0、BLOB 字节一致、DATE/DATETIME 规范化（宽松输入含）、不可能日期 1292；INT 字面量比 BOOLEAN 列 → 1235 `cannot compare`；`SHOW VARIABLES LIKE 'char%'`/`SHOW STATUS LIKE 'Up%'`/`@@version` 定形；ES 镜像行数一致 |
| 124 | `scenario_telemetry_shard_topics_lite_e2e.rs` | RESP+MySQL | `telemetry` 父题由 q0 5/q1 3/q2 2 三个显式队列支撑；`XINFO TOPICS` 按子题字节序回扁平 `[child, length]` 对；`XINFO STREAM` 内省单队列；组排空 q0 后 XACK 清空 PEL 汇总；Rust-only `XINFO LITE` 计数器 `messages`/`acks` 反映流量；SQL 分片登记表（路由器启动读取） |
| 125 | `scenario_data_lake_export_lifecycle_e2e.rs` | S3+MySQL+RESP | CreateBucket 200（重复 409 `BucketAlreadyOwnedByYou`）；25 个 part 对象；ListObjectsV2 `max-keys=10` 分页 10/10/5、并集 25 键有序、Size == SQL 清单；分隔符恰折出一个 `lake/` CommonPrefixes；非空 DeleteBucket 409 `BucketNotEmpty`；25 次删除 204、DeleteBucket 204、HEAD 404。用自建 `lake125` 桶（夹具预声明的 `rdb` 桶是 checkpoint 发布目标） |
| 126 | `scenario_kafka_autocreate_registry_e2e.rs` | Kafka+MySQL+RESP | 开启 auto-create：Produce v3 打未知 `auto126` 物化（base 0，下一批 1）；Metadata 回 1 分区；RESP `XLEN auto126/p0` == 2、`XINFO TOPICS auto126` == [p0, 2]；重复 CreateTopics 36；DeleteTopics 退役；SQL 租户登记表；10 轮 topic 往返 |
| 127 | `scenario_lite_gc_redelivery_e2e.rs` | RESP+MySQL | 两把后台扫帚 300ms（`lite.redelivery_idle_ms`/`lite.consumer_gc_ms`）：弃投递被 idle sweep 重投（XPENDING deliveries 1→≥2，无客户端在场，<3s；「恰 1 次」探针与投递读都走 20ms 静默窗，离 300ms sweep 窗口余量 ~280ms）；MAXDELIVERY 2 + 显式 DLQ 超限死信（`XLEN dlq` == 1，载荷 + trace 字段，PEL 行消失），XACK 清同组兄弟；空 PEL 消费者 <3s 从 `XINFO CONSUMERS` 消失；SQL 对账表 |
| 128 | `scenario_sql_dr_checkpoint_e2e.rs` | MySQL+S3+RESP | 写 20 行 SQL + 10 个 RESP 键（5 SET + 5 HSET 各两字段）后等待**晚于写入**的 checkpoint（id = unix ms、meta.json 最后落 = 集合完成）；SIGKILL、擦数据目录全部活 KV 文件但保留 `raft/`、拷回 checkpoint 文件（meta.json 与 `*.s3meta.json` 边车不是 RocksDB 状态）、同配置重启：COUNT(*) 20 且行值精确、HGETALL 完整、meta.json 仍在 |
| 129 | `scenario_kafka_offset_epoch_e2e.rs` | Kafka+RESP+MySQL | RESP 侧 `epoch129` 父流 10 次 XADD（topic p0）；ListOffsets v5 latest 10 / earliest 0；OffsetForLeaderEpoch v2 活分区 (0, -1「no epochs」, 10)、未知分区 (3, -1, -1)；OffsetCommit v0 7 → OffsetFetch v0 7 → Fetch v0 从 7 取尾批（base 7、3 条、hwm 10）；SQL 水位表镜像提交位移与 log end |
| 130 | `scenario_cluster_migrate_list_e2e.rs` | RESP+MySQL | 真两节点集群（node0 引导、node1 raft-http 加入、`cluster init` 等分槽带）：100 键单 hash-tag 槽播到属主；手开目的端 IMPORTING，COPY 落地探针证明 ASKING 一次性（裸读 -MOVED 回源，ASKING 读到值）；`migrate task <slot> <src> <dst>` → +OK、`migrate list` → JSON 记录（`"status":"done"`、`"moved":100`）、属主切换（dst 本地答、src -MOVED 到 dst）、leader 的 MySQL 面留 SQL 审计行 |

## 追加钉死（写进覆盖地图的语义点）

- S3 Range：`Content-Range` 与字节切片一一对应，`bytes=-N` 后缀、`bytes=N-` 开区间、
  不可满足 → 416 `InvalidRange`；HEAD 只回 size；ListObjectsV2 分页 + `CommonPrefixes`
  折叠；空桶/非空桶删除语义（409 `BucketNotEmpty` → 删空 → 204 → HEAD 404）。
- Kafka 控制面：CreateTopics/CreatePartitions/DeleteTopics + 重复创建 36；DescribeConfigs
  4 项配置；auto-create 由 Produce 触发且 RESP 侧分区名为 `<parent>/p<N>`（不是 `/q<N>`）；
  ListOffsets v5 水位、OffsetForLeaderEpoch v2 的 `leader_epoch -1`「无纪元」哨兵与未知分区
  `(3, -1, -1)`；OffsetCommit v0/v2 与 OffsetFetch v0 位移往返；JoinGroup 79 的 generation 0。
- Lite 队列卫生：idle sweep 重投递计数递增、MAXDELIVERY + 显式 DLQ 死信、XACK 清 PEL、
  空 PEL 消费者 GC、`XINFO LITE` 计数器、`XINFO TOPICS` 扁平 `[child, length]` 对。
- SQL：FULL OUTER JOIN 7 行 + USING 双 id + 反连接 + INTERSECT/EXCEPT；join 上歧义列
  1054；`VECTOR(4)` 三度量 KNN + 向量函数往返 + vector index 在 `SHOW INDEX` 里是 BTREE；
  columnar append-only 1235 + `SHOW CREATE TABLE`；prepared `?` 绑 LIMIT/OFFSET + 1064
  负 LIMIT + DEALLOCATE 后 stale handle；`FOR UPDATE` 1205 + `FOR SHARE` + NOWAIT/SKIP
  LOCKED 1235；类型化 DDL 的 DESCRIBE/BOOLEAN/BLOB/DATE 规范化/1292；双引擎
  `SHOW DATABASES` 合并 + T3 allowlist 1235 不静默回落；checkpoint 灾备跨 SQL+RESP 两面。
- RESP/FT：`FT.CREATE ON HASH PREFIX 1` 与 `TAG` 被拒、真实 FT.ADD JSON 面；`RETURN 0`
  → `{}`、`HIGHLIGHT TAGS`、`WITHSCORES` 4 位小数；migrate task/list 的 JSON 记录与
  ASKING 一次性门。

## 真实行为偏差（当轮断言并记录在案，未削弱测试）

- #114 JoinGroup 79 回执携带的 generation 是 0（组尚未 barrier，非 -1）；DeleteGroups 后
  同一条流 `XTRIM … MAXLEN 0` 回**实际修剪条数 3**（非 `:0`）。
- #112 生产默认 `delay_sweep_ms = 0`（延迟消息不交换），本例显式 `lite.delay_sweep_ms: 100`；
  `delay_ms=700` 的记录在下一个 100ms tick（700–800ms）才被交换，故 park 用 1500ms 而非
  700ms，`at_least(600ms)` 仍证明确实停住。
- #116 RediSearch 形 DDL `ON HASH PREFIX 1` / `TAG` 不是本面语法（unknown option /
  unknown field type）；真实面是 `FT.CREATE <idx> SCHEMA …` + `FT.ADD` JSON 文档，
  FT.SEARCH 与 `HGETALL` 读回用 `cmd_full_reply_wait`（40ms 静默窗）取全文回执。
- #117 join 两侧都暴露 `id` 时裸 `ORDER BY id` 回 MySQL 1054 `ER_BAD_FIELD_ERROR`
  （消息含 `ambiguous column`），不是 1064 解析错误。
- #121 `DEALLOCATE PREPARE` = 二进制协议 COM_STMT_CLOSE；stale handle 再执行由 opensrv
  命令循环在其语句表查无该 id 时以 io 错误断连（opensrv-mysql 0.7.0 `Command::Execute`），
  shim 的 1243 分支（`src/sql/front/shim.rs` `on_execute`）对已关闭 id 不可达；断言只接受
  驱动 `Io` 错误（`Server(1243)` 即偏差反转、任何 Ok 行集即「答出原页」，均判失败）。
- #123 INT 字面量与 BOOLEAN 列比较被拒 1235 `cannot compare`（不按 0/1 比较）。
- #125 生命周期骑自建 `lake125` 桶：夹具预声明的 `rdb` 桶是 checkpoint 发布器目标，
  其 `rocksdb/` 对象集开机 3s 后落地，会污染列表并阻塞删桶。
- #126/#113 Kafka 物化的分区名是 `<parent>/p<N>`（裸 RocksMQ 通道才是 `<channel>/q0`）。
- #130 需 `sql_rpc: true`（审计行写 leader 的 MySQL 面），并以 `wait_sql_nodes` 等
  两节点的 `sql_rpc` 端口都注册进目录后再做断言。

## 回归与卫生

- `cargo test --workspace --no-fail-fast`：全量 2141 例，2140 通过、1 例为既有 flake
  （`tests/sql/sql_2pc_e2e.rs` `dead_participant_aborts_and_restart_recovers`，全量并发
  负载下报 `ERROR 1213 (40001): sql ts: ts authority unreachable`，与本批无关；单独
  重跑绿）。其中 scenario 130 例（串行；初稿 197.19s，复审修正后两轮 199.05s / 190.57s
  全绿；最慢 #130 5.39s / 5.58s、#128 4.92s / 4.77s、#127 5.56s / 3.53s，均在 120s
  整体上限内）、lib 1432、其余 target 579（含 bins）。
- 复审修正（合入前，仅改场景文件）：#130 `asking_cmd` 按每条回复传 CRLF 行数
  （`[2,1]`：bulk 2 行、`-MOVED` 错误 1 行）而非 `times × lines`，凑不够行数不再靠 5s
  `timeout` 吞掉退出（整例 10.5s → 5.4s）；#127 投递读与「恰 1 次」探针改 20ms 静默窗
  （原 `rf` 250ms + `pending_rows` 200ms 让探针离 sweep 首个 [300,500) 窗口仅 ~45ms），
  重投计时戳改在投递前取；#121 stale handle 断言收窄为仅 `Io(_)`（原 `Server(_)` 会让
  1243 反面偏差也通过，Ok 臂声称接受 NULL 行但 `cell_i64` 遇 NULL 即 panic）；#116 补
  20 次 `HGETALL` 读回与 FT 文档逐字段比对（原 HASH 面只写不读），`cmd_int` 改用
  `scenario_common::int_of`。
- 推送前门禁复跑（修正后）：`cargo build --workspace` 与 `clippy -D warnings` 干净；
  `cargo test --workspace --no-fail-fast` 2140 过 + 1 例负载 flake
  `tests/sql/sql_join_cluster_e2e.rs` `cluster_joins_gather_both_sides`（node1 raft-http
  join head timeout，18 个 target 并发时出现；单跑 3/3 绿、`--test sql` 整 target 139/139
  绿；本批未动 `src/` 与 `tests/common/`，与 `sql_2pc` 的负载抖动同类）。（harness 侧重生已由
  batch-14 closure WP4 落地：`2026-11-27/batch14-defect-closure.md` 缺陷 4。）
- `cargo clippy --workspace --all-targets -- -D warnings` 零告警（修复 9 处：#129 同型
  cast、#127 文档 `>=` 转义、#120/#124 `query_drop(&format!(…))` 去借用）。
- 20 个新文件 147–398 行（上限 400）；`tests/scenario_common/mod.rs` 414 行（迭代上限
  800）；`perf.rs`/`s3_extra.rs` 为新增子模块，纯函数、无 class、无敏感数据。
