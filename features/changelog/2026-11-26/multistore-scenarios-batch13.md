# 多存储场景批次 13：#146–#160 十五例 e2e（2026-11-26）

承接批次 1–12（2026-11-19/20/21/22/24/25，累计 145 例），批次 13 再扩 15 例，编号
#146–#160，不改 `src/`、不改 `tests/common/`、`tests/kafka_front_common/` 与
`tests/scenario_common/`。选题来自对 145 例覆盖地图的缺口审计：已实现但从未被场景
钉死的协议面（XREAD BLOCK 停住/唤醒、XPENDING/XCLAIM/XAUTOCLAIM 分页、FT.CREATE 真实
语法面、SETSLOT MIGRATING 的 ASK 语义、ZUNIONSTORE/ZINTERSTORE 权重聚合、LPOS/LMPOP、
JSON.SET NX/XX 与 TTL 保持、DDL 幂等脚本、AUTO_INCREMENT 预留批次、turso 租户库生命
周期、Kafka topic admin 校验梯、长轮询 Fetch、S3 硬化合同、ES 索引合同、指标族普查）。
每例仍是一个真实业务故事横切 ≥2 个存储面，断言同一份数据在不同面之间的一致性；每例
钉死性能预算（`serial_guard` → `t0` → ≥1 `perf::within` → `perf::scenario(tag, t0)`，
预算 = max(2s, 3× 实测) 向上取整到整秒，`at_least` 只用于「真的停住/没提前过期」）。

## 场景矩阵

| # | 文件 | 平面 | 断言要点 |
|---|---|---|---|
| 146 | `scenario_alert_tail_xread_block_e2e.rs` | RESP streams + Kafka + MySQL | 告警尾随：裸父 `XADD alerts` 自动落 `alerts/q0`、XLEN/XRANGE 递增、`XREAD COUNT 1` 只回首条；空闲 `XREAD BLOCK 1500 $` 真停 ≥1.4s 后 `*-1`；先停住的读被新 XADD 唤醒且只拿到新条；XREADGROUP 两消费者只投一人（另一 `*-1`，PEL 记胜者）；手搓 Produce v3 base=5 + Fetch v0 hwm 6/续读；MySQL `alert_log` 行 = XADD 字段、COUNT=3；消费后 tail 再读 `*-1` |
| 147 | `scenario_stale_consumer_triage_e2e.rs` | RESP + MySQL | 工单分诊：8 条 XADD 显式 id；alice 4/bob 1；XPENDING 摘要逐字节、区间行 `[id,consumer,idle,deliveries]`、consumer 过滤、`IDLE 3600000` 空；XCLAIM 7 例（IDLE/TIME 回拨、JUSTID+RETRYCOUNT、FORCE 铸造、无 FORCE 未知 id 空、min-idle 门）；XACK :6；XAUTOCLAIM 游标翻页 `0-0→2-3→4-5→0-0` 与 JUSTID 只回 id；MySQL `triage_claims` GROUP BY = 摘要 consumer 段 |
| 148 | `scenario_job_board_search_e2e.rs` | RESP FT + MySQL + ES | 招聘板：FT.CREATE 真实语法（选项在 SCHEMA 前、TEXT/KEYWORD/NUMERIC/VECTOR、重复/未知索引错误）；HASH 不自动入索引、JSON 走 FT.ADD；`@salary:[90000 130000]`、短语/前缀/并集/排除/标签查询逐个 = Rust 集合；SORTBY/RETURN/HIGHLIGHT TAGS；ES 409 + `match` = FT 短语集合；SQL prepared BETWEEN = FT 数值集合 |
| 149 | `scenario_slot_dryrun_asking_e2e.rs` | 2-node RESP cluster + MySQL | 槽位干跑：slot 11520（CRC16 位运算重算）+ `-MOVED` 探属主；MIGRATING 期间命中本地服务、未命中精确 `-ASK`、缺字段 `$-1`；GETKEYSINSLOT 是物理行预算（10 行=5 键、1000 钳到现存键）；SETSLOT 全错误集、IMPORTING 读 `-MOVED`、STABLE 回 nil；SETSLOT NODE → 源 `-MOVED`、新属主空 `$-1`、数据留源；`raft get sql_nodes` 双登记；SQL `slot_audit` 9 行 = RESP 结果 |
| 150 | `scenario_fantasy_league_zunion_e2e.rs` | RESP zset/set/hash + MySQL | 梦幻联赛：6 个 STORE（WEIGHTS 1 2 3 SUM、plain、ZINTERSTORE MIN/MAX ± WEIGHTS）逐成员 = Rust 折叠；完整错误集；WRONGTYPE/CROSSSLOT；失败 STORE 不建键；ZRANDMEMBER/HRANDMEMBER WITHVALUES 与 SPOP 按集合断言（长度+成员+分数，不比顺序）；SQL `SUM(score*CASE)`/`MIN/MAX … HAVING COUNT(*)=3` = 折叠结果 |
| 151 | `scenario_waitlist_lpos_lmpop_dispatch_e2e.rs` | RESP list + RocksMQ + MySQL | 等位名单：LPOS 首/次/负 RANK/缺省 nil/COUNT/MAXLEN + 错误集；LMPOP 语法与 COUNT 边界、五轮派单 = Rust 座位 oracle（RIGHT 尾先、跨列表、空列表跳过）；RocksMQ produce_batch/consume/ack/ack_batch/pending + 401/405+Allow/404/400；SQL `seating` = 派单顺序 |
| 152 | `scenario_flag_doc_json_nx_xx_e2e.rs` | RESP JSON + MySQL | 特性开关文档：NX 已存在 nil、NX 缺省创建、XX 缺省 nil、XX 更新回读；字段级覆盖保 TTL（190..=200）、新键 -1；多路径 JSON.GET 扁平数组 + 缺路径 nil；INDENT 被当路径 `-ERR wrong static path`；TYPE/OBJLEN/STRLEN；MySQL `flags` = 独立 registry |
| 153 | `scenario_idempotent_migration_script_e2e.rs` | MySQL + RESP | 幂等迁移脚本：跑两遍 `IF NOT EXISTS` 两次 +OK、裸 CREATE TABLE/INDEX → 1050；UNIQUE 重复数据 1062、未知列 1054、未知表 1146；RENAME TABLE/ALTER RENAME AS 保留索引；inline KEY 进 SHOW INDEX 与 EXPLAIN `IndexScan`；SHOW TABLES LIKE 忽略 pattern、SHOW KEYS → 1235；INSERT…SELECT…WHERE NOT EXISTS 不翻倍；RESP INCR 计数 = 2 |
| 154 | `scenario_order_intake_sequences_e2e.rs` | MySQL + RESP | 订单录入：AUTO_INCREMENT 1/2/3；NOT NULL 1048 不烧号、唯一 1062 烧预留（RESERVE_BATCH=64）；TRUNCATE 复位且事务内 1235；无 PK 建表 1235；二进制协议 prepared 导入 + SAVEPOINT/ROLLBACK TO/RELEASE（已释放 1305）；列数/类型 1136；BEGIN 隐式提交；RESP `order:seq` = SQL `MAX(id)` |
| 155 | `scenario_turso_tenant_lifecycle_e2e.rs` | MySQL(turso) + RESP | 租户库生命周期：CREATE DATABASE 建库文件 + USE；SHOW CREATE TABLE 精确（双引号、inline KEY 不打印）；复合 PK/inline KEY、DESCRIBE 形状；TRUNCATE 保自增（next 3）；1062/1048/1235/1146/1290（0444 库只读）；DDL 在事务内允许（宿主 1235）；DROP DATABASE 删文件 + 1049/1007/1008；RESP 镜像 |
| 156 | `scenario_topic_admin_validation_e2e.rs` | Kafka + RESP + MySQL | topic admin 校验梯：28 探针逐条入 `topic_admin_audit`；ApiVersions 表；CreateTopics v4 11 例（坏名 17、assignment 39、rf 38、partitions 37、validate_only、(-1,-1)、重复 36）；CreatePartitions 9 例（未知 3/0/37/39/shrink）；DescribeConfigs 6 例（BROKER 42、未知 3、坏名 17、默认行）；Metadata v1 brokers/controller/partitions/unknown；Fetch 未知 topic (3,-1)；XADD 裸父 + Fetch 回读 |
| 157 | `scenario_telemetry_longpoll_fetch_e2e.rs` | Kafka + RESP + MySQL | 长轮询：空 p0 Fetch max_wait=1000 真停 ≥900ms（at_least）+ 上限 4s、0 条；先停后产 205–213ms 唤醒、1 条；hwm+5 → error 1；ListOffsets 时间戳矩阵 `[-2,-1,0,ms0,ms0+1,ms1+3600000]`；acks=0 无响应帧但仍落盘；XADD 裸父 XLEN = hwm；MySQL `telemetry_lag` 行/计数/lag |
| 158 | `scenario_object_store_hardening_e2e.rs` | S3 + MySQL | 对象存储硬化：本地 `http_raw` 打 401/405（无 Allow）/411/413（公告长度）/501（仅 Transfer-Encoding）/431（缺空行）；桶级空键 400、HEAD 缺桶 404 空体、XML `<Code>`；身份头、Content-Type 回显、幂等 DELETE 204、Range 206/206/416/200；manifest = GET/HEAD 尺寸 = 前缀列表 |
| 159 | `scenario_search_index_contract_e2e.rs` | ES + MySQL + RESP | 搜索索引合同：错误信封 `{error:{root_cause,type,reason},status}`；409 resource_already_exists、400 mapper/document/invalid name、404 index_not_found/no_handler、405、413、501；`_source: 5` → 400；三文档 `_bulk` 201 跨 ES / RESP FT.SEARCH / SQL `es_docs` 一致 |
| 160 | `scenario_metrics_census_raft_kv_e2e.rs` | monitor + raft + RESP + Kafka + MySQL | 指标普查：指数族 10（启动）→ 13（有流量），TYPE/HELP/无未列族；12 INCR → 直方图 +12、GET +1、MULTI 流水线 queued +2/commit +1、SQL kinds +1、XADD+2 Produce → `rdb_lite_messages` +3/produce +2/live streams +2、Fetch +1；RAFT SET/GET/nil/arity；`/get`、`/join` 401/无 peerAddress；COMMAND DOCS/INFO 线形、INFO server、`INFO nosuchsection` `$0`；mux 持有期间全部 2s 内应答；join ≥30s / depart ≥28s → 200 `internal error`，Leader + FSM 存活 |

## 追加钉死（写进覆盖地图的语义点）

- XREAD/XREADGROUP `BLOCK` 的停住与唤醒：空闲超时 `*-1`、新条只投一个消费者、PEL
  记胜者、消费后的 tail 不重复投递。
- XPENDING 摘要与区间行、consumer 过滤；XCLAIM IDLE/TIME/JUSTID/RETRYCOUNT/FORCE 与
  min-idle 门；XAUTOCLAIM 游标翻页与 JUSTID 只回 id；XACK 后 PEL 收缩。
- FT.CREATE 真实选项面（选项在 SCHEMA 前、四种字段类型、重复/未知索引错误）、HASH 与
  JSON 的入索引进洞差异、数值范围/短语/前缀/并集/排除/标签查询、SORTBY/RETURN/
  HIGHLIGHT TAGS。
- CLUSTER SETSLOT MIGRATING 的 ASK 语义（命中本地、未命中 `-ASK`）、GETKEYSINSLOT 的
  物理行预算、SETSLOT NODE/STABLE/IMPORTING 的回复与路由变化。
- ZUNIONSTORE/ZINTERSTORE 的 WEIGHTS/AGGREGATE 组合与错误集；ZRANDMEMBER/HRANDFIELD/
  SPOP 的集合式断言。
- LPOS RANK/COUNT/MAXLEN 与 LMPOP 的语法/边界/错误；RocksMQ produce_batch/consume/ack/
  ack_batch/pending 与 401/405/404/400 合同。
- JSON.SET NX/XX 与 TTL 保持、多路径 JSON.GET 扁平数组、INDENT 未实现、TYPE/OBJLEN/
  STRLEN。
- DDL 幂等（IF NOT EXISTS/1050/1061/1062/1054/1146）、RENAME、inline KEY → IndexScan、
  SHOW TABLES LIKE 与 SHOW KEYS 的真实偏差。
- AUTO_INCREMENT 的 RESERVE_BATCH 预留与冲突前分配、LAST_INSERT_ID 语义、TRUNCATE 复位、
  SAVEPOINT/ROLLBACK TO/RELEASE、1136/1048/1062/1235。
- turso 租户库文件生命周期、`turso_` 命名门、复合 PK/inline KEY、TRUNCATE 保自增、
  1062/1048/1235/1146/1290、事务内 DDL 双引擎差异、DROP DATABASE 1007/1008。
- Kafka CreateTopics/CreatePartitions/DescribeConfigs/Metadata 的错误码梯与 validate_only
  不落库；Fetch max_wait/min_bytes 的停住边界、OFFSET_OUT_OF_RANGE、ListOffsets 时间戳、
  acks=0 无响应帧。
- S3 的 401/405/411/413/431/501 触发条件、key-path 400、Content-Type 回显、幂等 DELETE、
  Range 206/416/200。
- ES 的错误信封与 409/400/404/405/413/501 合同、bulk 逐项状态、DSL 未解析选项被忽略。
- 指标族普查与夹逼增量（before/after 快照，不比较两份独立快照）、RAFT SET/GET 与
  `/get?key=`、`/join` 不可达 → 200 `internal error`、COMMAND DOCS/INFO、INFO 节过滤。

## 真实行为偏差（当轮断言并记录在案，未削弱测试）

- #146 裸父 `XADD alerts` 自动选 `alerts/q0`（`src/lite/entries.rs:97-99`）；流 id `0`
  被拒 `-ERR Invalid stream ID...`（`src/lite/model.rs:42-50`）。
- #147 XPENDING 无 `COUNT` 关键字（`src/lite/pending.rs:126-171`）；XINFO GROUPS 是 13 元
  素扁平数组、无 pending/lag（`src/lite/info.rs:56-100`）；「只认 PEL 头」仅 ORDERED 组
  （`src/lite/claim.rs:281-283`）。
- #148 FT.CREATE 选项在 `SCHEMA` 前、类型仅 TEXT|KEYWORD|NUMERIC|VECTOR；HASH 键不自动
  入索引、JSON 走 FT.ADD（`src/search/ft_cmd.rs:69-124,150-153`）；ES 索引与 FT 索引共
  用 `KIND_SEARCH_META` 键空间，ES 镜像必须换名（`src/es/write.rs:107-116`）。
- #149 slot 11520 落在 node1 频带，迁移方向为 node1→node0（角色由 `-MOVED` 探出，未硬
  编码）；GETKEYSINSLOT 是物理行预算（`src/command/cluster.rs:77-115`）；路由锚
  `src/command/mod.rs:321`（`redirect_line`）。
- #150 缺失源键按空 zset 处理，STORE 以幸存成员成功（`src/command/zsetops_cmd.rs:144`）；
  ZINTERSTORE 含缺失源 → `:0` 且 `EXISTS` 0。
- #151 `/ack` 对 PEL 外的合法 id 回 200 ok（`src/rocksmq/api.rs:216`）；`LMPOP 3 w1 w2 LEFT`
  先撞 CROSSSLOT 而非键数错误（`src/command/list_mpop.rs:31-38`）。
- #152 INDENT 被当路径 → `-ERR wrong static path`（`src/command/json_cmd.rs:184-192`、
  `src/command/json_path.rs:32-79`）；多路径 JSON.GET 是扁平数组（`json_cmd.rs:204-216`）。
- #153 `SHOW TABLES LIKE` 忽略 pattern（`src/sql/parse/translate.rs:489`、
  `src/sql/exec/show.rs:185-200`）；`SHOW KEYS` → 1235（`translate.rs:498-509`）。
- #154 `RESERVE_BATCH=64` 预留 + 冲突前分配（`src/sql/exec/sequence.rs:37,84-88`、
  `src/sql/exec/write.rs:125`）；`LAST_INSERT_ID()` = 语句首 id（`sequence.rs:46-47`、
  `write.rs:127-128`）；无 PK 建表 → 1235（`translate.rs:661-668`）；未知表 1146 而非
  1305（`src/sql/parse/error.rs:84-88`）。
- #155 可建库必须 `turso_` 前缀（`src/sql/turso/catalog.rs:58-66`）；DEFAULT 子句被丢弃
  （`src/sql/parse/translate_type.rs:35`）；TRUNCATE 保自增（`src/sql/turso/route.rs:71-81`）。
- #156 DescribeConfigs 坏名 → 17 + "no such topic"；TOPIC 默认行连错误/BROKER 行一起输出
  （`src/kafka/admin_configs.rs:77-84`）；CreatePartitions 未知 topic 的 message 为 NULL
  （`src/kafka/admin_topics.rs:223-225`）；重复 topic 实际回 36（非 17）。
- #157 `min_bytes` 只在全空时生效（`src/kafka/fetch.rs:23-29`）；acks=0 是 fire-and-forget、
  无响应帧（`src/kafka/produce.rs:17-19`）。
- #158 405 不带 `Allow`（`src/s3/router.rs:262`）；501 仅由 Transfer-Encoding 触发
  （`src/s3/http.rs:215`）；`/<bucket>/` 是桶级（`router.rs:30`）；413 来自公告
  Content-Length（`http.rs:161`）；431 只在空行缺失期间（`http.rs:356-369`）。
- #159 `timeout`/`track_total_hits`/`highlight` 未解析 → 忽略并 200（`src/es/dsl.rs:165-215`）；
  `_refresh` 是 no-op ack。
- #160 `rdb_sql_tx_active`/`rdb_sql_tx_total` 从不移动（无 sql_tx_enter/exit 调用者，
  `src/monitor.rs:209-219`）；counter 无 `_total` 后缀（prometheus 0.13.4 text.rs:79-81）；
  `/join` 30s MEMBERSHIP_TIMEOUT（`src/rcache/http.rs:65`）→ `add_learner(blocking)` 立即
  返回（`replication_lag_threshold`，`raft/mod.rs:735-775`），`change_membership` 追加一条
  永不提交的 JOINT 配置 → 超时后死 peer 显示为 voter（`state.rs:236-262`）、`/depart`
  失败于 `ensure_committed`；此类失败 join 后 raft 日志提交被楔住（测试不断言，避免挂死）。

## 回归与卫生

- 串行跑 15 例：15/15 绿，57.08s。全量 `cargo test --test scenario`：160/160 绿，
  273.02s（workspace 全量内的另一轮 273.65s）；批次 12 基线 218.92s + 本批 15 例
  合计 ~54s（含 #160 故意的 30s join 停住与 #149 两节点 7.5s）。
- 15 例热块实测/预算：kpoll157 长轮询 801–825ms/3s、seq154 导入 446–457ms/2s、
  turso155 生命周期 337–359ms/2s、eserr159 bulk 275–284ms/3s、eserr159 一致性 260–262ms/2s、
  tail146 唤醒 225–323ms/2s、tail146 再读 121ms/2s、wait151 派单 177–196ms/2s、
  triage147 清扫 183–193ms/2s、fuse150 融合 182–187ms/2s、jobs148 检索 186–213ms/2s、
  triage147 认领 164–170ms/2s、dry149 ASK 探针 135–138ms/2s、kadmin156 梯 35–40ms/2s、
  mig153 两跑 28–42ms/2s、s3hard158 探针 13–14ms/2s、flag152 写入 13–23ms/2s、
  eserr159 DSL 2–3ms/2s；#146/#157/#160 的 `at_least`：停住 ≥1.4s、≥900ms、
  ≥30s/≥28s（实测 1.54s、1.02s、30.003s/29.49s；#146 原有的 ≥200ms 项为同义反复，收尾时删除）。
  整例最慢 census160 35.5–35.8s
  （故意 30s join 探针）、dry149 7.49–7.67s（两节点 + 3s topology ticker ×2）、kpoll157
  2.26–2.70s，其余 ≤3.4s。
- 推送前门禁：`cargo test --workspace --no-fail-fast` 全量 2171 例全绿 = 基线 2156 +
  本批 15（`RUST_TEST_THREADS=4` 并发负载下 scenario 160/160 273.65s、sql 139/139、lib
  1432、其余 target 全绿），本轮未出现既有负载 flake（`sql_2pc_e2e::
  dead_participant_aborts_and_restart_recovers`、`sql_join_cluster_e2e::
  cluster_joins_gather_both_sides` 均过）。
- `cargo clippy --workspace --all-targets -- -D warnings` 零告警（过程中修掉 2 处：
  #158 `needless_borrow`、#159 `needless_range_loop`）。
- 15 个新文件 268–398 行（上限 400），纯函数、ASCII、无敏感数据、每文件 ≤1 个 `rf(`；
  `tests/scenario/main.rs` 仅追加 15 行 `mod`（字母序）。
- 附带发现（既有、未改）：MySQL 前端每个结果集语句固定 ~42ms（`SELECT 1` 也如此，OK 包
  语句亚毫秒），`src/sql/front/serve.rs:74` 的 socket 未 `set_nodelay`，疑似 Nagle ×
  delayed-ACK；与批次 12 记录的 ~30–42ms/SELECT 一致。
- 评审后收尾加固（评审结论"有条件上线"；只动批次内 5 个文件，不改 `src/`/公共夹具）：
  #146 `spawn_block` 改为到达握手——先等 AUTH `+OK` 再写阻塞读，200ms 让渡只覆盖 XREAD
  派发而不再覆盖连接/AUTH 往返（`$` 在 XADD 前定型），并删除同义反复的
  `at_least(200ms)`（空闲停住 ≥1.4s 的那项保留）；#160 `session()` 静默间隙 60ms→150ms
  （文件已顶 400 行，行数不变）；#155 0444 只读块整体 `#[cfg(unix)]`（检测按权限位，
  非 unix 下原本会假失败）+ 用例末尾 `drop(front)` 后删除库根目录（不在自清理 world 内，
  此前每跑残留一个 `/tmp/rdb-scenario-turso155-db-<pid>`）；#151 LPOS COUNT 无匹配改断言
  线上形态 `*0
`（原 `cells()==[]` 对 nil 同样成立）；#157 修正 SUM(lag) 断言文案
  （和为 hwm..hwm-3 而非 0+1+2+3），并把 acks=0 的"500ms 静默 + 落地"从 3s 长窗口拆出
  独立 `within` 2s（实测 503ms）。复验：5 例各 3 轮 15/15、scenario 160/160（275.97s）、
  clippy 零告警。
