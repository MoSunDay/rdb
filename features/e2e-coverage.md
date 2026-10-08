Commit: b7378c0
# E2E 覆盖地图与缺口台账

> 缺口基线为 git `f071032`（2026-09-26 全量只读比对，30/188 命令零覆盖等）；基线缺口
> 已全部闭合（过程与修复叙事见 `changelog/2026-09-26/e2e-coverage-closeout.md`），
> 下文为覆盖现状 + 残余缺口。

## 测试层级地图

| 层级 | 载体 | 说明 |
|---|---|---|
| in-proc registry | `tests/common/lite.rs`（真实 store + 真实命令注册表） | 断言精确 RESP 字节，无 TCP |
| in-proc wire | `resp::serve` 临时监听（`resp_e2e`/`gaps_e2e`/`bits_e2e`/`server_surface_e2e`/`routing_newcmds_e2e`） | 真实编解码 + 原始 TcpStream |
| in-proc raft | `raft_cluster_e2e`/`raft_transport`/`ha_failover`/`sql_oracle_cluster_e2e` | 进程内 openraft 节点 |
| 进程级 e2e | `tests/common/mod.rs`（`CARGO_BIN_EXE_rdb` + 临时 yaml + 临时端口） | 最接近真实部署 |
| 前端专用 mini-harness | `kafka_front_common/`、`es_common/`、`s3_e2e/`、`rocksmq_http_e2e` | 真实二进制 + 手搓协议客户端 |
| shell 场景 | `scrtips/e2e_scenarios/scenario_*.sh`（run_all.sh glob，当前 9 个） | redis-cli/iredis/redis-py/mysql/confluent-kafka 等真实客户端 |

CI 入口：push 跑 `cargo test --workspace --no-fail-fast`（含全部 Rust e2e）；
夜间 `soak.yml` 跑 `soak_kill9.sh` + `run_all.sh`（soak 已装 confluent-kafka）。

## 覆盖现状（基线缺口 → 对应套件）

- **命令面 30 个零覆盖 → 0**：`hash_read_e2e`（HSETNX/HMGET/HEXISTS/HSTRLEN/HKEYS/HVALS/HRANDFIELD）、
  `set_more_e2e`（SPOP/SRANDMEMBER/SSCAN/SDIFF/SINTER/SMISMEMBER）、`zset_more_e2e`
  （ZCOUNT/ZMSCORE/ZREVRANK/ZRANDMEMBER/ZREM/ZPOPMAX/ZREVRANGEBYSCORE/ZLEXCOUNT/ZREMRANGEBYSCORE/
  ZRANGEBYLEX + ZREVRANGEBYLEX 首条成功路径）、`keys_more_e2e`（UNLINK/EXPIREAT/PEXPIREAT/PERSIST/
  RANDOMKEY/RENAMENX/TYPE/KEYS/CONFIG）、`list_e2e` 补 LPUSHX/RPUSHX、`search_e2e` 补 FT.DROPINDEX
  （与 FT.DROP 同 handler、连文档一起删——与 RediSearch 的 DROPINDEX-保文档语义不同，已钉住）。
- **RESTORE 成功路径 + ASKING 门（wire）**：`asking_restore_e2e`——IMPORTING 槽无 ASKING 得 MOVED、
  同连接 ASKING 单次放行、`SETSLOT STABLE` 复位；DUMP（库内 `ds::dump::dump_key`）→ wire RESTORE
  成功往返（typed 家族 re-root 到目标键）、REPLACE/BUSYKEY/ttl/ABSTTL(空操作)/坏载荷错误面。
- **JSON/Vector/FT/CONFIG 传输层盲区**：`wire_families_e2e`（JSON/Vector）+ `wire_ft_config_e2e`
  （FT.*/CONFIG）全部走真实 TCP（空拓扑单节点）。
- **进程级 backup 接管**：`backup_failover_e2e`（3 真进程 + `spawn_node_backup`、yaml 注入
  backup_target_map、kill -9 → MOVED 指向存活同伴的 backup 监听 → 可读/拒写 -READONLY → 重启回切）；
  `backup_surface_e2e`（readonly allowlist **87/87** 全量过门证明 + 33 个写命令负向；表与播种助手在
  `tests/backup_surface_common/`，另含 table 与 `readonly::ALLOWED` 的集合相等运行时断言）。
- **ES 前端**：`es_auth_e2e`（es_token 401 矩阵 + 双节点 remote-slot 400 `routing_exception` 精确信封）。
- **S3 checkpoint**：retention 剪裁（盘上 = 协议面 = 保留数）+ meta.json 文件清单与 S3 GET 字节一致性。
- **Kafka 边角**：`kafka_group_e2e` 补 LeaveGroup 未知成员/group（均 25）、DescribeGroups 未知
  group（group 级 NONE + state Dead，非 25）。
- **MQ 引擎批次（2026-10-06 Batch 1，首次入台账）**：
  - `lite_dlq_e2e`（7）：MAXDELIVERY 超限单 WAL 批原子转移三件套（PEL 删行+水位推进+
    DLQ 入队）、重复 claim 不双转、ORDERED 仅 PEL 头、显式 DLQ 名/语法门（DLQ 须随
    MAXDELIVERY、n≥1）、kill -9 转移原子持久、`rdb_lite_dlq_depth` gauge、默认无重投。
  - `lite_redeliver_e2e`（5）：sweep 只碰 idle 行（times+1+delivered_ms 刷新）、越
    MAXDELIVERY 行直接 DLQ、已 ACK 行永不重投、与手动 XCLAIM 共存、ORDERED 只重投头。
  - `lite_trim_minid_e2e`（5）：MINID `=`/`~` 同为精确删、`<ms>-0` 时间窗留存、语法/边界
    错矩阵、与 MAXLEN 正交、kafka 账本守卫拒 XTRIM/XDEL。
  - `kafka_headers_roundtrip_e2e`（4）：produce 形状还原真 record headers、tombstone
    带 headers、无 headers 不变、exotic 形状回退 envelope + `rdb-envelope` 标记头。
  - `kafka_rename_ledger_e2e`（2）：RENAME 随搬 0x20 账本（守卫跟新名）+ 旧名
    OffsetCommit 回 3 `UNKNOWN_TOPIC_OR_PARTITION`；同 slot 配对形状单测。
- **延迟消息（2026-10-06 Batch 2 / WP2，首次入台账）**：
  - `lite_delay_e2e`（9）：XADD DELAY 到期前全读路径不可见（XLEN/XRANGE/XREADGROUP）
    → 到期交换、乱序提交按 due 升序交换且读序=到期序、BLOCK 读者被交换唤醒、
    **三连泄漏回归**（XIDLE 惰性清除/主动采样、RENAME 随搬 0x1D 段、FLUSHDB 清空
    ——暂存行必须随流族死/搬，已删流不得被复活投递）、选项负矩阵（非整数/0=不延迟/
    悬空选项/溢出拒绝）、同路径重开两态持久。
  - `lite_delay_proc_e2e`（3）：真二进制 + `lite.delay_sweep_ms` 的**spawn 扫描器**
    进程级（注册教训：扫描器 spawn 路径必须有进程 e2e）、BLOCK 读者被 spawned 交换
    唤醒、kill -9 双态持久（已交换 entry 存活 + 未到期暂存行重启后经扫描器交换）。
- **MQ Batch 1.5/2 补齐（2026-10-06，首次入台账；changelog
  `changelog/2026-10-06/mq-batch2.md`）**：
  - `lite_group_dlq_e2e`（4）：XGROUP CREATE `DLQ` 选项负矩阵——目标==源流（原地
    覆写 + len 虚涨）、跨 slot 显式目标（集群形态落错节点窗口）、空名（不静默回退
    默认目标）各专用报错且不留组；隐式默认与合法同 slot 显式目标仍可用。
  - `lite_dlq_triggers_e2e`（3）：进程级补齐后两个 MAXDELIVERY 触发面——XGROUP
    SETID 回卷后 `>` 重投越限转移（崩溃重投路径）、XAUTOCLAIM 反复交付越限转移；
    JUSTID 仅移所有权永不触发（四个触发面至此全覆盖）。
  - `lite_redeliver_proc_e2e`（1）：真二进制 + `lite.redelivery_idle_ms=800` 的
    spawn 重投 sweep——被弃 PEL 行交付次数无客户端干预上升、同窗未达 idle 阈值
    行不被碰。
  - `kafka_headers_fidelity_e2e`（3）：header **名字**字节保真（含协议禁止但未强制
    的非法 UTF-8）produce→存储→Fetch 两条回放路径（native "h" JSON hex 名 /
    envelope 回退值）均字节精确；`rdb-envelope` 撞名真用户头胜出、原样回放。
  - `kafka_admin_e2e`（4）：ListGroups = runtime ∪ 仅账本组（Empty 占位态）；
    DeleteGroups 走 lite 拆除折 0x20 行并逐出 runtime（XTRIM/XDEL 账本守卫的
    wire 出口）；未知组 69 且批内其余照常；SASL 全矩阵（pre-auth ApiVersions、
    未认证断连、错口令 58+close、PLAIN 通过后全生产面）。
  - `rocksmq_wait_pending_e2e`（4）：`wait_ms` 长轮询（超时空回保默认值 / 唤醒 /
    延迟消息到期可见）、`POST /pending` 汇总与错误面、`delay_ms` 透传、
    `rocksmq_token` Bearer 矩阵。
- **MQ P3 按需池回填（2026-10-07，首次入台账；计划 `plans/2026-10-07-mq-p3-backfill/`，
  摘要 `changelog/2026-10-07/mq-p3-backfill.md`）**：6 个新 e2e 文件 + 1 个新公共
  harness（`tests/common/mq.rs`，rocksmq HTTP 进程级夹具，`tests/common/mod.rs` 冻结
  在 799 行不加一行）+ 2 个 wire/unit 侧新文件（`src/kafka/admin_topics_tests.rs`、
  `admin_configs_tests.rs`）。
  - `kafka_topics_e2e`（3，进程级，helpers 在 `tests/kafka_front_common/topics.rs`）：
    topic 全 wire 生命周期（CreateTopics 建流→Metadata/Fetch 可见→CreatePartitions
    扩容→DeleteTopics 删净）、删 topic 键族折叠（entries+0x20 账本+0x1D 延迟行+嵌套
    DLQ 齐消，Resp 侧佐证）、`kafka_auto_create_topics` 两态（默认 false = produce
    未知 topic 回 3 不建流；true = 建默认单分区后写入）；ListOffsets v5（latest/
    earliest + -1 leader_epoch）、DescribeConfigs 桩、OffsetForLeaderEpoch 常量应答
    断言随生命周期用例展开（v2–v4 帧形态由 `src/kafka/offsets_query.rs` 单测钉住，
    广告面 15→20 由 `kafka_wire_e2e` 版本注册断言更新）。
  - `lite_claim_opts_e2e`（7）：XCLAIM IDLE 回拨 / TIME 停墙钟 / RETRYCOUNT 改写
    计数（JUSTID claim 同样落 PEL 写）、回拨行即刻满足 min-idle、FORCE/JUSTID 与
    提示任意交错、坏值负矩阵——XPENDING idle/deliveries 列读侧同步回归。
  - `lite_xinfo_full_e2e`（5）：`XINFO STREAM FULL` 形状（entries/组 pending/消费者
    pel）与 COUNT 截断（消费者 pending 精确不截断）、非 FULL 输出不变、语法矩阵、
    NOMKSTREAM 不建键回 nil、`xadd_trim_pins_xtrim_limit_semantics`（XADD
    `MINID ... LIMIT` = XTRIM 参数语义钉死，P3 #9 收缩结论的证据）。
  - `rocksmq_batch_range_e2e`（7，fixture `tests/common/mq.rs`）：批量 produce
    有序 id、混合批量逐项隔离（一项非法其余成功且错误就地可见）、批量 ack 子集
    收缩 pending、`/range` 界内回放 + limit 截断、`/range` 只读（组消费仍见全部
    未确认消息）、三新路由 token 门禁、`rocksmq_max_connections: 1` 第二条并发
    连接被静默拒纳。
  - `lite_consumer_gc_e2e`（6，in-proc 直调 GC 轮）：无 PEL 空闲成员回收而带 PEL
    成员保全、XACK 刷新 seen 时钟（未超时不回收）、parked XREADGROUP 读者保全
    （活跃租约判据）、有序 owner 在租豁免而同组旁观者照常回收、`0` 默认关零回收、
    GC 与重投 sweep 交错互不干扰（各自游标独立）。
  - `lite_consumer_gc_proc_e2e`（4，真二进制 + `lite.consumer_gc_ms` spawn 后台
    GC）：spawned GC 回收幽灵成员、活跃成员保全而空闲成员消失、kill -9 后已回收
    成员**不重现**（同步批写持久；幸存者及其 PEL 完整、未 ack 行按 at-least-once
    重投）、默认关（0）无后台任务零行为变化。
  - 场景覆盖（`scrtips/e2e_scenarios/`，由并行收尾车道扩展中）：`scenario_kafka_sdk.sh`
    增 AdminClient 建/删 topic 步（真实 SDK 走 CreateTopics/DeleteTopics wire）、
    `scenario_lite_mq.sh` 增 XINFO FULL / NOMKSTREAM 步——作为场景层回归入口计入
    本批覆盖，脚本断言随收尾合入。
- **MQ 缺陷修复（2026-10-08，首次入台账；DUMP 折 0x1D 与 FLUSHDB 逐出协调器，
  另见同日缺陷修复条目）**：
  - `lite_delay_migrate_e2e`（3，新文件）：DUMP/RESTORE **同名**还原折 0x1D 暂存行
    （延迟行随流族搬运、due 语义不丢）、DUMP/RESTORE **改名**搬 0x1D 段（延迟行
    跟新名，旧名不得复活）、MIGRATE 不丢延迟行（跨节点搬运后仍按 due 交换）。
  - `kafka_group_e2e`（+1，**追加进既有文件，非新文件**）：flushdb 清库后
    ListGroups 无幽灵组（流族删除须同步逐出组协调器内存态，组面查询不得残留
    已删流上的组）。
- **SQL 兼容收敛（2026-10-06 MySQL-gap M0-M5，首次入台账；计划
  `plans/2026-10-06-mysql-gap/`，逐里程碑 `changelog/2026-10-06/mysql-m*.md`）**：
  - `sql_query_semantics_e2e`（6）：ORDER BY/GROUP BY 序数（`'1'`/`-1`/`1+1` 常量键
    no-op 差分钉死序数不再退化）、ORDER BY/HAVING 别名（别名胜出同名源列）、
    prepared `LIMIT ?`/`OFFSET ?` 与负/小数/字符串绑定 1064、FROM DUAL、
    1054/1235 措辞负矩阵。
  - `sql_funcs_{string,numeric,datetime,control}_e2e`（29 = 7+8+7+7）：四函数族精确
    值 + 结果列元数据定型 + prepared + 负矩阵（1582 arity、1235 方言）；control
    含 CASE 惰性、`<=>`/XOR 真值表、CAST/CONVERT 全目标。
  - `sql_upsert_e2e`（5）/`sql_insert_select_e2e`（3）/`sql_upsert_cluster_e2e`（1）：
    affected 1/2/0 全矩阵（pk/unique、多行混合累计）、`VALUES(col)` 引用与
    `c = c + VALUES(c)`、改 pk ODKU、REPLACE 删多插一、INSERT…SELECT 语句前快照
    （同表翻倍）、INSERT…SET、ODKU 烧号、集群 ODKU/REPLACE 每节点 1235 + plain
    与 INSERT…SELECT 跨 band 2PC 提交三节点一致。
  - `sql_subquery_e2e`（10）：相关标量/IN/EXISTS（空集/NULL 陷阱）、两层嵌套、
    setop 臂内相关、INTERSECT/EXCEPT DISTINCT/ALL 多重集算术与混合链优先级
    差分、窄拒绝矩阵（相关 JOIN 条件/派生表外层引用/skip-level/WITH RECURSIVE）。
  - `sql_ddl_surface_e2e`（5）/`sql_ddl_surface_cluster_e2e`（1）：TRUNCATE（条目
    随旧 table_id 清扫、自增重置、事务内拒绝）、RENAME（prepared 旧名 re-exec
    干净报错不悬挂）、索引 DDL 全形态（ALTER/CREATE/DROP INDEX + 内联 KEY）、
    SHOW 面（CREATE TABLE 逐字断言 + 渲染重建往返、VARIABLES LIKE、STATUS）、
    会话函数 per-exec 绑定；集群侧 TRUNCATE/RENAME ack 即刻全节点可见 + follower
    响亮拒绝。
  - `sql_outer_join_e2e`（7）：LEFT/RIGHT [OUTER] JOIN 首次 e2e——NULL 补齐、
    anti-join、OUTER 关键字可省、链式/过滤/INNER 混排、join 键索引语义不变、
    RIGHT ≡ 换位 LEFT。
  - `sql_failover_e2e`（2）：三进程 SIGKILL SQL leader——旧连接 10s 内干净报错、
    新 leader 接管 DDL/跨 band 写/ts 授块（尸体 band 响亮失败、绝不部分提交）；
    尸体 respawn 追平后全成员可读杀前基线（MVCC 一致）。
  - `sql_decimal_e2e`（1）：自 `sql_types_e2e` 拆出的 DECIMAL 精确语义自包含用例
    （字节级等价搬家）。
- **SQL 静默错误结果清零（2026-10-06 mysql-hardening H0；计划
  `plans/2026-10-06-mysql-hardening/`，changelog `mysql-h0-silent-wrong-results.md`）**：
  - `sql_rename_gc_e2e`（1，新增）：`RDB_SQL_GC_PERIOD_MS=200` 缩短 GC 周期，RENAME
    后 5+ 轮清扫数据完好（回归 RENAME 数据丢失 ship-blocker）；TRUNCATE 后旧数据
    确清、新写入完好。
  - `sql_upsert_e2e`（6）：+同语句腾出唯一值的 ODKU/REPLACE 正确分支（affected 3，
    无辜行不再误删）、ODKU/普通 UPDATE pk 挪移活行线上 1062（ER_DUP_ENTRY）。
  - `sql_subquery_e2e`（11）：+内层同名未限定列绑定内层表（遮蔽回归，线上）。
  - `sql_query_semantics_e2e`（7）：+ROUND(SUM)/COALESCE(SUM) 聚合包裹、ODKU 赋值
    与 UPDATE ORDER BY 中的会话函数绑定。
  - `sql_e2e`：+prepared `LIMIT ? OFFSET ?` 参数序（换位即错的数据）与
    `LIMIT ?, ?` 双占位符 1064。
  - `sql_e2e`（2026-10-08）：+prepared 数值/DATE 绑定进文本定型占位符列
    （`COALESCE(NULL, ?)` 出规范文本、DOUBLE 定型投影吃 Int 运行 cell、同一连接
    后续查询存活——二进制协议断连回归；暂存 src 重跑以 `connection closed`
    失败验证过用例真实覆盖）。
  - `sql_funcs_numeric_e2e`（8）/`sql_funcs_string_e2e`（7）：+整型溢出 1690 措辞
    负矩阵 ×5（Add/Sub/Mul/Div/Neg）、CONCAT_WS 空串参数分隔符矩阵。
  - 既有套件更新：`mysql_compat_e2e`（M3 增 INTERSECT/EXCEPT DISTINCT/ALL 与混合链
    左折叠、尾部 `ORDER BY 1 LIMIT .. OFFSET ..`、相关 IN/EXISTS/标量子查询与拒绝
    矩阵断言）；`tests/common/mysql.rs`（M5 新增）收敛 7 个单机套件
    （sql/sql_types/sql_txn/sql_index/mysql_compat/auto_increment/columnar）的
    connect/ddl/rows 样板并吸收
    `sql_funcs_common`/`sql_upsert_common`（目录已删，各套件行数 -16%~-32%）；9 个
    集群/HA 编排套件（sql_2pc/sql_join_cluster/sql_setop_cluster/sql_dist_read/
    sql_restart/sql_composite_pk/sql_ddl_visibility/starrocks_model/
    sql_upsert_cluster）**有意未迁移**（自带编排/取证样板，后续分批）。
- **Shell 场景**：新增 `scenario_ha_failover.sh`（首次启用 `e2e_kill_node`，kill→MOVED-backup ~4-5s、
  回切 ~5s）、`scenario_lite_mq.sh`（XADD/XREADGROUP/XPENDING/XACK/XAUTOCLAIM + ORDERED/INFLIGHT，
  2026-10-06 增 (g) DLQ/MAXDELIVERY 死信+DLQ 独立消费、(h) XTRIM MINID/LIMIT/时间窗
  与 (i) 延迟消息（DELAY 暂存到期交换/全新 id/BLOCK 唤醒/RENAME 随搬）三段）、
  `scenario_migrate.sh`（`migrate task` 全流程 + 反向回迁）；`scenario_redis_session.sh` 增
  redis-py `RedisCluster()` 探针（**已连接成功**：protocol=2 跳过未实现的 HELLO；无 redis-py 自跳过）。
- **漂移与卫生**：RESULTS.md/soak.yml 的“4 场景”漂移改为 glob 措辞；soak.yml 补装 confluent-kafka；
  `scrtips/redis-py.py`/`redis-async-py.py` 内嵌真实 raft token 换 `RDB_TOKEN` 环境变量占位。

## 残余（记录在案，非本次范围）

- `CONFIG` 仍是固定桩（无 GET/SET 子命令解析，仅回 `cluster-require-full-coverage/no`）；
  `RANDOMKEY` 同 slot 全键时确定性地返回字典序首键。
- redis-py 探针未覆盖 pipeline/pubsub/`CLUSTER SHARDS`（RESULTS.md known gaps）。
- `config/conf_3268*.yaml` 明文 raft token（P3 债务，非 e2e 范围）。
