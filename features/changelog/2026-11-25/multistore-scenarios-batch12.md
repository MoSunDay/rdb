# 多存储场景批次 12：#131–#145 十五例 e2e（2026-11-25）

承接批次 1–11（2026-11-19/20/21/22/24，累计 130 例），批次 12 再扩 15 例，编号
#131–#145，不改 `src/`、不改 `tests/common/`、`tests/kafka_front_common/` 与
`tests/scenario_common/`。选题来自对 130 例覆盖地图的缺口审计：已实现但从未被场景
钉死的协议面（CLUSTER/RAFT 面板、统一 ZRANGE、SET EXAT/PXAT、XGROUP ORDERED/
INFLIGHT + XINFO FULL、JSON.FORGET/VSIM WITHATTRIBS/FT FP16、SQL 时钟函数族与数值/
字符串/位运算函数族、会话握手、RIGHT/CROSS JOIN + RENAME/DROP INDEX/DROP DATABASE、
StarRocks 型 DDL、ES REST 生命周期与 `_bulk` upsert、Kafka ApiVersions/
FindCoordinator/tombstone、S3 ListBuckets/v1 marker、运维面 `/metrics` 与 raft HTTP）。
每例仍是一个真实业务故事横切 ≥2 个存储面，断言同一份数据在不同面之间的一致性；每例
钉死性能预算（`serial_guard` → `t0` → ≥1 `perf::within` → `perf::scenario(tag, t0)`，
预算 = max(2s, 3× 实测) 向上取整到整秒，`at_least` 只用于「真的停住/没提前过期」）。

## 场景矩阵

| # | 文件 | 平面 | 断言要点 |
|---|---|---|---|
| 131 | `scenario_ops_dashboard_cluster_e2e.rs` | RESP+MySQL（真两节点，mysql+sql_rpc） | `CLUSTER INFO/NODES/SLOTS/KEYSLOT` + `RAFT stats/leader/nodes` 两节点互查：同 term/leader、`cluster_state:true`、16384/16384/0/0、known=size=2、epoch = term++commit_index（夹在前后两份 RAFT stats 快照之间）、NODES id = md5-with-40 规则、SLOTS 0..=16383 连续铺满、`-MOVED <slot> <owner>` 双向且 KEYSLOT = CRC16 重算、`raft get sql_nodes` 登记双节点 resp/mysql/http/sql_rpc、leader MySQL 快照行 = RESP 值并复制到 follower |
| 132 | `scenario_auction_ladder_zrange_e2e.rs` | RESP+MySQL | 12 个出价：统一 `ZRANGE` 10 对组合（BYSCORE ± REV/LIMIT/WITHSCORES、BYLEX、名次 REV、负名次）与 `ZRANGEBYSCORE/ZREVRANGEBYSCORE/ZRANGEBYLEX/ZREVRANGEBYLEX/ZREVRANGE` 回复字节相等；Rust 侧独立排序；11 条统一错误 + 4 条旧命令错误；XADD 12 条且尾条 = `ZREVRANGE 0 0`；SQL `ORDER BY amount DESC, bid_id LIMIT 2 OFFSET 1`（prepared）= REV 窗口 |
| 133 | `scenario_coupon_exat_window_e2e.rs` | RESP+MySQL | `EXAT/PXAT` 未来 → TTL/PTTL 区间；过去时间戳 +OK 且键即刻不存在（EXISTS 0、TTL -2、XX nil/NX ok）；KEEPTTL 保 TTL、`EX n KEEPTTL` 拒绝；EXAT 0/-5 invalid expire、EXAT abc 非整数；优惠券账本 `expires_at > / <= NOW()` = RESP 存活/过期集合、UNIX_TIMESTAMP 往返、剩余秒 ≈ TTL；`XADD DELAY 700` 暂存（XLEN 0）→ 提醒行 JOIN 活账本；两枚活券「没提前过期」`at_least` |
| 134 | `scenario_ordered_ticket_queue_e2e.rs` | RESP+MySQL | `XGROUP CREATE … ORDERED INFLIGHT 2` 配给 `>` 读（2 条后 nil）；INFLIGHT 无 ORDERED 语法错、INFLIGHT 0 范围错、重复 BUSYGROUP；`XINFO GROUPS` 13 元素含 `committed-id/ordered/inflight/owner/epoch`；`XINFO STREAM … FULL`：length、PEL 行 `[id, consumer, idle, deliveries]`、roster、`COUNT 1` 截条目/PEL/消费者 PEL 但不截计数；c2 被围栏、owner/epoch 1；重复 ACK :0；`XGROUP DESTROY` :1 → :0 → NOGROUP；审计表 delivered/acked/orphaned = PEL 行数 |
| 135 | `scenario_profile_merge_vector_attrs_e2e.rs` | RESP(JSON+Vector+FT)+MySQL | `JSON.FORGET $.legacy` :1/:0、缺键 :0、`$..legacy` 拒绝、`$.legacy.tags[0]` :1、`JSON.DEL` 同处理器；VADD VALUES dim 4（±1 向量余弦精确）:1/重加 :0、FP16 blob（RNE 编码器）:1、VCARD/VDIM、维度/坏 FP16 负例；VSETATTR 自 SQL 三元组；`VSIM WITHSCORES WITHATTRIBS` 扁平 `[elem, attr, score]`、Rust 侧独立排名、属性先于分数、COUNT 3、FP16 查询同序；FT.SEARCH `*N`+total 头、KNN NOCONTENT/WITHSCORES `1/(1+L2)`、TAG 过滤、FT.INFO ann_built 0 → FT.BUILD → (1,2) 质心、SPANN = 暴力；SQL `legacy IS NOT NULL` 计数 = JSON 面 |
| 136 | `scenario_attendance_clock_funcs_e2e.rs` | MySQL+RESP | 以 Hinnant 历元算法为独立 oracle：NOW/CURRENT_TIMESTAMP/SYSDATE/LOCALTIME(3)、CURDATE/CURTIME、DATE(NOW())、YEAR..SECOND 定值、DATE_SUB/SUBDATE 跨 MINUTE/HOUR/DAY、负区间、NULL 传播、UNIX_TIMESTAMP 四种入参、FROM_UNIXTIME 含 0、表内 FROM_UNIXTIME/文本插入往返为历元、DATEDIFF 夜班 [0,1,0]、DATE_FORMAT `%W %p`、范围谓词；RESP `SET .. EX 120` TTL (115,120]、PTTL、`EXPIRE 300`、缺失 -2、无过期 -1；SQL `UNIX_TIMESTAMP(doors_at)` = RESP `GET` |
| 137 | `scenario_partner_export_funcs_e2e.rs` | MySQL+S3+RESP | 数值族 24 格（ABS/CEIL/FLOOR/ROUND/TRUNCATE/MOD/POW/SQRT/SIGN/GREATEST/LEAST + NULL）、字符串族 28 格（LENGTH vs CHAR_LENGTH CJK+emoji、CONCAT/CONCAT_WS NULL 规则、SUBSTRING 三拼写、LPAD/RPAD、LOCATE/INSTR/POSITION、TRIM 三形、HEX/UNHEX、数字→文本）、位运算 AND/OR/XOR/左移/右移 u64 语义（`1<<63` = i64::MIN、`1<<64` = 0）、21 个未实现函数 1235；`partner_rows` 投影 vs Rust 分币精确；CSV → S3 PUT（ETag = Rust md5）GET 字节相等 + HEAD；SQL COUNT = CSV 行 = RESP HLEN |
| 138 | `scenario_bi_session_handshake_e2e.rs` | MySQL+RESP | 握手脚本（SET NAMES utf8mb4/latin1/DEFAULT、`autocommit = 1`、sql_mode、wait_timeout）全 OK；`@@character_set_*`="utf8mb4"、`@@autocommit`="1"、`@@version`="8.0.32-rdb"、`@@max_allowed_packet`=67108864；隔离级默认 REPEATABLE-READ、READ-COMMITTED/SERIALIZABLE/READ-UNCOMMITTED 连字符回显；拒绝 1235：`SET autocommit = 0`、`READ ONLY`、`SET time_zone`；USER()/CURRENT_USER()/SESSION_USER() 同值 `root@`、CONNECTION_ID 稳定且第二登录不同；`USE turso_sess138` → DATABASE() 回显、`USE rdb`、未知库 1049 保持；CJK 标签 prepared INSERT；RESP 哈希 HLEN = COUNT(*)、HGET = SQL 合计 |
| 139 | `scenario_warehouse_schema_migration_e2e.rs` | MySQL+RESP | RIGHT JOIN 5 行（Kyiv 空延）= 交换侧 LEFT JOIN = Rust 外连接；CROSS JOIN 精确有序 = 嵌套循环、COUNT 3×5；逗号 cross join 1235；`RENAME TABLE stock TO inventory`：SHOW TABLES 翻转、旧名 1146、行相等、PRIMARY 存活 SHOW INDEX、新名可插；`CREATE INDEX ix_sku` → EXPLAIN `IndexScan ix_sku -> 3 pks` + Filter、`DROP INDEX` → `SeqScan inventory`；`CREATE DATABASE turso_wh_mig139`/SHOW/USE/DROP、非 `turso_` 库名 1235、`USE` 缺失 1049、`DROP DATABASE` 缺失 1008；RESP 哈希 = SQL 行 |
| 140 | `scenario_starrocks_vendor_contract_e2e.rs` | MySQL(StarRocks 方言)+ES+RESP | 3 张 OLAP 表解析（DUPLICATE KEY BUCKETS 8、PRIMARY KEY BUCKETS 4 + DECIMAL + COMMENT、DUPLICATE 默认 BUCKETS 10）；7 条 1235（UNIQUE/AGGREGATE KEY、PROPERTIES、PARTITION BY、ORDER BY、LARGEINT、HLL）；`SHOW TABLES STATUS` 1064；INSERT…SELECT 进 PK 表去重、进列存 1235 append-only；COUNT/SUM/GROUP BY 精确；DESC/SHOW COLUMNS 形状；SHOW CREATE 片段 + 往返（由 SHOW CREATE 文本再建表输出相同）；ES 镜像 term/range 命中 = SQL 计数、`_count` 3 |
| 141 | `scenario_log_index_rollover_e2e.rs` | ES+MySQL+RESP | `GET /` tagline + 版本 `8.11.0-rdb`；`GET /_cluster/health`（green 或 yellow、timed_out false、1 节点、0 主分片）；`GET /{idx}` 回显 mappings、`number_of_shards "1"`；自动 id 201 + 20 位 hex `_id`；HEAD 200/404 空体；重复 PUT 200 updated；手动 id + 定域 `_bulk` 4 条；term 查询 error 命中 3、`_count` 6；GET 动词 `_search`/`_count`/`_refresh`；3 处 405 `method_not_allowed_exception`、5 处 404 `index_not_found_exception`；`_cat` 行；SQL `rollover_log` SUM(docs),SUM(errors) = (6,3) |
| 142 | `scenario_customer_upsert_bulk_e2e.rs` | ES+RESP+MySQL | `?op_type=create` 首写 201、再写 409 `version_conflict_engine_exception` 且原文不变、普通 PUT 200 updated；定域 `_bulk` index/update/delete：errors:true、逐项状态（201/400/404/201/200）、`_index` 盖章；非定域 `_bulk`；bulk URL `?op_type=create` 被忽略；`minimum_should_match` 2 → 双子句、1 → 并集、"75%" → 2、"50%" → 1、显式 msm 门控 must、3 个非法值 400 parsing_exception；`_count` 4；RESP HSET/HLEN/HGET/HGETALL 与 ES tier/name/due 一致；SQL gold 计数 = ES term 命中、SUM 320.00 = ES due 和 |
| 143 | `scenario_fleet_coordinator_onboarding_e2e.rs` | Kafka+RESP+MySQL | ApiVersions v0..v3：corr 回显、throttle_time_ms 位置、error 0、表按 key 排序；v4 → error 35；Metadata v1 题名回显、单分区；FindCoordinator v0/v1 type 0 → (0, node 1, 127.0.0.1, broker 端口)、type 1 → 42 + 哨兵；Produce v3 5 条状态 + 1 条 tombstone 偏移 0..5；ListOffsets latest 6/earliest 0；Fetch 一批 6 条、第 5 条 (v3, null)；RESP `XLEN orders/p0` 6、XRANGE 五个 `v` 字段 + tombstone 仅 `__null__`；SQL fleet alive 集合 = 最新记录非空的车辆 |
| 144 | `scenario_lake_listing_audit_e2e.rs` | S3+MySQL | 10 个 PUT（含 `a b.bin`、`c+d.bin`）；ListBuckets `GET /` 单桶 rdb + Owner；v1 `max-keys=4` 三页并集 = 十个键有序、NextMarker 当且仅当截断 = 末键、百分号编码、无 `<Marker>` 回显；v2 `start-after` 跳 2、continuation-token = 编码末键；`encoding-type=url` `+` → `%2B`；`max-keys=0` 空不截断、-1/abc → 400 InvalidArgument、5000 钳到 1000、伪 token → 200 空；`/UPPER` → 400 InvalidBucketName、`/ab` → 404 NoSuchBucket、HEAD 缺桶 404；对象级 GET 缺桶 → 404 NoSuchKey；MySQL manifest 行 = 键/尺寸、SUM = HEAD 尺寸和 |
| 145 | `scenario_observability_drill_e2e.rs` | monitor HTTP+raft HTTP+RESP+RocksMQ | `/metrics` text/plain、`# HELP/# TYPE`、`raft_stats{status="Leader"} 1`（等到）、`rdb_command_latency_count{ack,mode,type}` 精确计 INCR 次数、桶边界 5/30/..、GET 自带 mode 标签、其他路径 404；raft：唯一登记节点 = `cluster_slots_stable_instances`、`/get sql_nodes`、错 token → 空体、`/join`/`/depart` 错 token → 401 `unauthorized`、缺 peerAddress 200 invalid；`/sql/ts` 错 token 401、200 纯文本 `<start> <end>` 租约续期；`/sql/nodes` 错 token 401、缺 raft bind 200、未知控制路径 404；每次拒绝后 sql_nodes/稳定实例不变；RocksMQ `GET /produce` 405 + Allow、`/nope` 404、坏 `%ZZ` 400、缺 channel/空体 400、正常 produce 200 且 id = XRANGE 条目 |

## 追加钉死（写进覆盖地图的语义点）

- CLUSTER/RAFT 面板：`CLUSTER INFO` 字段全集与 `cluster_state:true` 布尔拼写、
  epoch = `term ++ commit_index` 字符串拼接；NODES id 规则、myself 标记、等宽槽带；
  SLOTS 连续铺满；`-MOVED` 可被 NODES/SLOTS 预测并双向验证；`raft get sql_nodes`
  四元登记。
- 统一 ZRANGE 与旧命令 10 对组合字节相等；BYSCORE REV 交换边界而 BYLEX REV 不交换。
- `SET EXAT/PXAT` 过去戳即刻删除、KEEPTTL 与显式 TTL 互斥、TTL/PTTL 换算。
- `XGROUP ORDERED INFLIGHT n` 配给、`XINFO STREAM … FULL [COUNT n]` 形状与截断规则、
  `XGROUP DESTROY` 计数与 NOGROUP。
- `JSON.FORGET` 别名、静态路径门；VADD VALUES/FP16 两种向量形、`VSIM WITHATTRIBS`
  布局；FT.SEARCH FP16 查询向量 = VSIM 排名；FT.BUILD 质心数 = min(K, docs)。
- SQL 时钟函数族以独立历元算法为 oracle（含 UNIX_TIMESTAMP/FROM_UNIXTIME 整数约束）；
  数值/字符串/位运算函数族 52 格 + NULL 规则 + 未实现清单。
- 会话握手：SET NAMES 静态回显、autocommit=0 1235、隔离级连字符回显、`@@` 定值、
  USER()/CONNECTION_ID()、USE/DATABASE() 与 `turso_` 前缀。
- 模式迁移：RIGHT = 交换侧 LEFT、CROSS JOIN 笛卡尔、逗号 join 1235、RENAME 保留
  PK/索引、DROP INDEX 让 EXPLAIN `IndexScan → SeqScan`、DROP DATABASE 1008 / USE 1049。
- StarRocks 方言：DUPLICATE/PRIMARY KEY + `DISTRIBUTED BY HASH` 往返、SHOW CREATE
  归一化规则、列存 append-only。
- ES REST：root/health/GET index/auto-id/doc GET-HEAD/GET 动词 `_search|_count|_refresh`
  /405/404 信封；`op_type=create` 409、定域 `_bulk` 逐项状态、`minimum_should_match`
  数值/百分比形。
- Kafka：ApiVersions v0–v3 表 + v4 error 35、FindCoordinator v0/v1 两类 key、tombstone
  在 `<topic>/p<N>` 流中的 `__null__` 形。
- S3：ListBuckets、v1 marker/NextMarker 三页游走、v2 start-after/continuation-token、
  `encoding-type=url`、max-keys 四种边界、InvalidBucketName/NoSuchBucket/NoSuchKey。
- 运维面：`/metrics` 直方图计数随命令精确增长、raft HTTP 只读端点形状、变更端点错
  token 401（从不真正 join/depart）、RocksMQ 404/405/400 合同。

## 真实行为偏差（当轮断言并记录在案，未削弱测试）

- #131 `cluster_state` 是 `true` 而非 Redis 的 `ok`（`src/command/cluster.rs:189-198`）；
  `CLUSTER test` 恒回硬编码 `-MOVED 5465 127.0.0.1:32681`（`cluster.rs:283-285`，沿用
  Go 怪癖），且子命令匹配只有 `test` 分支大小写敏感（`cluster.rs:47-51`），故
  `CLUSTER TEST` 落到帮助文本。首轮全量跑暴露 INFO 与 RAFT stats 两份快照之间的
  commit_index 推进（ticker 持续提交）——改为前后两份 stats 夹逼 INFO 的 epoch。
- #132 统一 `ZRANGE key max min BYLEX REV` **不交换边界**（`src/command/zset_range.rs:212-220`），
  旧 `ZREVRANGEBYLEX` 交换（`:391-395`），BYSCORE 路径交换（`:228-234`）；混合分数的
  BYLEX 窗按分数序回答（`:150-170`）。
- #133 `EX n KEEPTTL` → `-ERR syntax error`（`src/command/string_opts.rs:110-112`，与
  Redis 7 一致）；过去戳被接受并即刻删除（`string.rs:161-162`、`string_opts.rs:14-16`）；
  SQL DDL 必须 PRIMARY KEY（1235）。
- #134 XINFO FULL roster 条目声明 `*6` 却尾随 `$3 pel *n` + n 条 PEL 行
  （`src/lite/xinfo_full.rs:153-161`）；深视图是 `XINFO STREAM <key> FULL [COUNT n]`
  （`src/lite/info.rs:238-266`）。
- #135 VADD 多余 token 被当向量值读取（`src/command/vectorset_cmd.rs:80-120`）；VSIM
  缺键报错而非 nil（`vectorset_sim.rs:36-43`）；属性恒先于分数（`:135`）；JSON 路径
  `*` 被拒；质心数 = min(K, docs)（`src/search/ann/kmeans.rs:21`）。
- #136 无 `TIMESTAMPDIFF`：单位词被解析成列 → 1054 `unknown column 'MINUTE'`；
  DATETIME−DATETIME 1235（`src/sql/exec/expr.rs:361-366`）；DAYNAME/DAYOFWEEK/LAST_DAY/
  MAKEDATE/STR_TO_DATE/TIME → 1235 unknown function（`src/sql/exec/func/mod.rs:51`）；
  FROM_UNIXTIME 只收整数（`func/string.rs:217`）；RESP 无 TIME/LASTSAVE/EXPIRETIME
  （`src/command/mod.rs:420`）。
- #137 PI/EXP/LN/LOG/CONV/CRC32/RAND/MID/SUBSTRING_INDEX/LTRIM/RTRIM/ASCII/CHAR/FORMAT/
  STRCMP/SPACE/ELT/FIELD/QUOTE/MD5/SHA1/SHA2 → 1235 unknown function；一元 `~` 1235
  （`src/sql/parse/expr.rs:75-77`）；`1 << 64` = 0。
- #138 `DATABASE()` 在 USE 前为 `"rdb"`（`src/sql/exec/mod.rs:170-174`）；SET NAMES 与
  charset 变量静态 utf8mb4（`parse/translate.rs:538`、`front/vars.rs:81-83`）；`SET
  autocommit = 0` 1235（`parse/session.rs:33-49`）；`SET GLOBAL TRANSACTION ISOLATION
  LEVEL` 在会话回显（`exec/mod.rs:308-316`）；赋值 sql_mode/wait_timeout 被忽略
  （`front/vars.rs:69-80`）。
- #139 用户库名必须 `turso_` 前缀（`src/sql/turso/catalog.rs:59-66`）；逗号 join 是
  1235 "comma cross joins" 而非 1064（`parse/query.rs:121-124`）。
- #140 只解析 DUPLICATE/PRIMARY KEY 与 `DISTRIBUTED BY HASH`（`parse/starrocks.rs:161-177`）；
  SHOW CREATE 只打印 keys[0]、分布仅 DUPLICATE 表输出、ENGINE 归一化、COMMENT 丢弃
  （`exec/translate.rs:662-665`、`exec/show.rs:163-176`）；`SHOW TABLES STATUS` 1064；
  INSERT…SELECT 进列存 1235（`exec/write.rs:57-70`）。
- #141 GET `_search`/`_count` 无请求体且忽略 `?q=`（match_all）（`src/es/router.rs:124-146`）；
  doc GET 未命中即使索引不存在也回 404 `{"found":false}`（`src/es/reply.rs:128-145`）。
- #142 `_bulk` 的 `update` 动作逐项 400 `action_request_validation_exception`
  （`src/es/bulk.rs:137-147`）；`_bulk` 不读 `?op_type=create` 或逐动作 op_type
  （`router.rs:59-66`、`bulk.rs:104-109`）。
- #143 Kafka 键没有字符串键视图，记录只是 `<topic>/p<N>` Lite 流条目
  （`src/kafka/produce.rs:141-146`）；tombstone = `__null__` 字段（`produce.rs:254-256`）；
  ApiVersions v4 须 flexible 帧（`src/kafka/mod.rs:160-165`）。
- #144 NextMarker/NextContinuationToken 恒百分号编码（即使无 encoding-type，AWS 回原键，
  `src/s3/xml.rs:118-127`）；v1 回复无 `<Marker>` 回显；对象级 GET 缺桶 → NoSuchKey
  而非 NoSuchBucket（`src/s3/router.rs:361-369`）；max-keys 5000 钳到 1000。
- #145 `/sql/ts` 回纯文本 `<start> <end>`（`src/sql/tx/global.rs:520-523`）；`/sql/nodes`
  缺 raft bind → 200 "missing raft bind"（`src/sql/tx/nodes.rs:199-202`）；`/join`/`/depart`
  错 token → 401（`src/rcache/http.rs:12-15,304-306`）；`raft_stats` gauge 走 5s ticker
  （`src/main.rs:178-186`），整例 5.4–9.6s 随 ticker 相位浮动，是等待不是热块。

## 回归与卫生

- `cargo test --test scenario -- --nocapture` 串行全量两轮：首轮 144/145（221.87s，
  唯一失败 #131 因 INFO/stats 快照时序），修正后 145/145 绿（218.92s；批次 11 基线
  190.6–199.1s + 本批 15 例合计 ~24.7s），#131 单跑另 3/3 绿。15 例热块实测/预算：
  clock136 854ms/3s、zr132 699ms/3s、attrs135 413ms/2s、sess138 384ms/2s、srs140 338ms/2s、
  obs145 控制面 262ms/2s、mig139 259ms/2s、ops131 171ms/2s、ord134 161ms/2s、fmt137 139ms/2s、
  coord143 42ms/2s、obs145 抓取 35ms/2s、exat133 35ms/2s、upsert142 36ms/2s、roll141 30ms/2s、
  list144 23ms/2s、fmt137 S3 9ms/2s（最大占比 0.28）；exat133 三处 `at_least`：提醒停住
  ≥600ms 实测 1.06s、活券未提前 ≥1.9s 实测 2.36s、毫秒券 ≥2.9s 实测 3.12s。整例最慢
  obs145 5.56s（另一轮 9.55s，5s gauge ticker 相位）、ops131 4.06s（两节点 + 注册表
  收敛），其余 ≤3.4s。
- `cargo clippy --workspace --all-targets -- -D warnings` 零告警（修复 5 处：#136
  `useless_format`、#138 `map_clone`/`iter_cloned_collect` → `CARDS.to_vec()`、#145
  `cloned_ref_to_slice_refs`、#134 `needless_borrow`、#140 `useless_vec`）。
- 15 个新文件 235–386 行（上限 400），纯函数、ASCII、无敏感数据；`tests/scenario/main.rs`
  仅追加 15 行 `mod`（字母序）。
