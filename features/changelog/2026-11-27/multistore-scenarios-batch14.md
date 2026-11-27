# 多存储场景批次 14：#161–#175 十五例 e2e（2026-11-27）

承接批次 1–13（2026-11-19/20/21/22/24/25/26，累计 160 例），批次 14 再扩 15 例，编号
#161–#175，不改 `src/`、不改 `tests/common/`、`tests/kafka_front_common/` 与
`tests/scenario_common/`。选题来自对 160 例覆盖地图的第二轮缺口审计：已实现但从未被场景
钉死的语义面（FT BM25 的长度/词频/参数面、WATCH 跨前端竞争、RENAME 对全部类型族的搬迁、
Lite DLQ 合同、RocksMQ HTTP 路由梯与跨协议身份、多流 XREADGROUP 扇入与组退役、Kafka
session expiry / leader assignment / 围栏提交、SQL 执行计划面与 SHOW INDEX、双引擎连接/
NULL 语义、多行 INSERT 语义、S3 100-continue/IncompleteBody 原始传输、ES 两节点健康/普查、
MOVED/ASK 重定向记账与备份监听）。每例仍是一个真实业务故事横切 ≥2 个存储面，断言同一份
数据在不同面之间的一致性；每例钉死性能预算（`serial_guard` → `t0` → ≥1 `perf::within` →
`perf::scenario(tag, t0)`，预算 = max(2s, 3× 实测) 向上取整到整秒，`at_least` 只用于「真的
停住/没提前过期」）。

## 场景矩阵

| # | 文件 | 平面 | 断言要点 |
|---|---|---|---|
| 161 | `scenario_search_bm25_doclen_e2e.rs` | FT (CREATE/ADD/DEL/INFO/SEARCH) + MySQL | 法律发现台：同 tf 文档按长度排序（m1 > m2，均等于 Rust 重算 BM25）、同 tf 稀有词压过常见词、隐式 AND 求和、`\|` 并集、NUMERIC range 加常数 1.0、K1/B 调参（B=0 抹平长度惩罚）、LIMIT 翻页 vs SORTBY 计全量、FT.INFO 语料统计、两面上派生 shortlist/budget，关闭 m1（UPDATE + FT.DEL）后 m2 重排 |
| 162 | `scenario_watch_json_crossfront_e2e.rs` | RESP MULTI/WATCH/EXEC + JSON + FT + XADD/HINCRBY + Kafka Produce v3/ListOffsets | 新闻发布台乐观并发：WATCH JSON 草稿 + 看板，一事务排队 JSON.SET + FT.ADD + XADD + HINCRBY，EXEC 全提交 `*N` 或对手先动（第二台的 Kafka 记录 / 另一连接 JSON.NUMINCRBY）时 `*-1`；FT 索引只跟已提交事务；读保持 watch、MULTI 外自写/DISCARD 清除；排队时 CROSSSLOT 整体中止；ListOffsets latest == XLEN |
| 163 | `scenario_rename_typed_families_e2e.rs` | RESP RENAME/RENAMENX/TYPE/TTL/PEXPIRE/PERSIST + HASH/ZSET/SET/LIST/JSON/VECTORSET/STREAM + FT + MySQL | 图书馆改架码：按 MySQL `catalog_moves` 计划（`old_name` 序）逐族 RENAME，`shelf_items` JOIN+SUM 与新名 RESP 基数对账；旧名消失，TTL 随迁（索引）或替换（string↔hash），RENAMENX 拒绝占用码 |
| 164 | `scenario_lite_dlq_contract_e2e.rs` | Lite streams + monitor HTTP | 支付 DLQ 合同：MAXDELIVERY 2 组的选项矩阵（DLQ 须随 MAXDELIVERY；空/自身/跨槽/裸名拒绝；默认 `<stream>/dlq`），XCLAIM 抬 delivery（JUSTID 不触发），超限 claim 把条目按原 id 搬进 DLQ 并带 `__dlq_group/__dlq_consumer/__dlq_times/__dlq_src`（源保留、PEL 行删除），XAUTOCLAIM 只在第三段列出死信 id，`rdb_lite_messages{op="dlq"}` + `rdb_lite_dlq_depth`，末尾 XACK 落水位 |
| 165 | `scenario_rocksmq_http_contract_e2e.rs` | RocksMQ HTTP + RESP lite + Kafka (ListOffsets v5/OffsetFetch v0/Produce v3) + MySQL | 包裹事件 HTTP 桥：bearer 梯（无/错 bearer 401 先于路径分类、已知路由 GET 405 + `Allow: POST`、未知路径 404、`bad query string: malformed % escape`、`delay_ms` 非法/越界 400）；`/produce` 6 条、`/consume n=3` base64 按序、自动建组 `hub-a`@0-0（消费者 `http`）、XINFO GROUPS 13 元、`/pending` JSON、`/ack` 幂等/未知组 404、`/range` `-`/`%2B`/`(id`；XLEN == ListOffsets latest 6，Kafka 投 2 条后 XLEN 8 且可经 `/consume` 读出；MySQL `parcel_audit` |
| 166 | `scenario_lite_fanin_group_retire_e2e.rs` | RESP lite + Kafka (CreateTopics v4/Produce v3/ListOffsets v5/Join+Sync v1/OffsetCommit v2/OffsetFetch v0) + MySQL | 事件扇入：两 RESP 流 + 一 Kafka 建的流（`inc166k/p0`）喂同一组 `ops`，一次 XREADGROUP `*3` 段按参数序；两阶段 join（79 → `rdb-ops-<n>` → generation 1 leader）后提交位移，Kafka 账本钉住该流（XTRIM 被拒）而 RESP 流可裁；退役：XACK :1、DELCONSUMER 回清掉的 pending 数、DESTROY :1 再 :0（兄弟组存活）、XINFO GROUPS 掉行、`>` NOGROUP、DESTROY Kafka 流上的组折叠账本（OffsetFetch -1、XTRIM 放行）；MySQL `notify_log` GROUP BY 对账 |
| 167 | `scenario_kafka_session_expiry_rebalance_e2e.rs` | Kafka wire + RESP stream | 双成员消费舰队：leader 持续心跳、follower 静默；协调者在 session 超时后（500ms sweep）驱逐静默者，leader 收 Heartbeat 27 重入新 generation 后再 Stable；被逐者晚到调用 25；流日志不受 churn 影响 |
| 168 | `scenario_kafka_leader_assignment_e2e.rs` | Kafka wire + RESP stream | 分配排练：leader 在 JoinGroup 回复里拿到全部成员订阅，经 SyncGroup 下发不透明逐成员分配，被停住的 follower 各收自己的字节（未知 id 静默忽略）；22/23/24/25 错误梯、leader 先定协议规则；分配后的舰队读日志 |
| 169 | `scenario_kafka_fenced_commit_e2e.rs` | Kafka wire + RESP stream | 检查点台：leader 提交组检查点（序号 3）；陈旧/未来 generation（22）与已离开成员（25）的提交被围栏且不触碰；跨 LeaveGroup 触发的 rebalance 进 generation 3 仍存活，独 leader 推到日志尾；两面读回同一日志 |
| 170 | `scenario_sql_pk_lookup_plan_e2e.rs` | MySQL + RESP ZSET | 隔离卖家管理器预览执行计划：EXPLAIN 单列 `plan` 逐行——整键 `=`/去重排序的 pk IN → `PkLookup sellers -> N pks`；文本序第一个可索引的合取（=、IN、BETWEEN）→ `IndexScan <ix> -> N pks`（无统计）；否则 `SeqScan` + `Filter:`；`Order:`/`Limit: n Offset: m`；残余过滤重放；`EXPLAIN INSERT/DELETE/DDL` → `Direct execution (<kind>)` 且不执行；SHOW INDEX 6 列 PRIMARY 先行按创建序，INDEXES/KEYS 1235；RESP ZSET（score=优先级）驱动 `id IN (...)` |
| 171 | `scenario_sql_conjunct_join_null_e2e.rs` | MySQL host + turso | 客户普查双引擎：INT 键 INNER 等值 join 不匹配 NULL、LEFT 补空、RIGHT 已实现；2/3 项合取链（含 LENGTH）换序结果不变；ORDER BY NULL 升序在前/降序在后（host 与 turso 同）；`NOT IN (1, NULL)` 为空、`IN (1, NULL)` 保留 city-1 行；LEFT JOIN + COUNT GROUP BY 未匹配城市计 0；`<=>` host 专用 |
| 172 | `scenario_sql_multirow_insert_e2e.rs` | MySQL + RESP ZSET | 货运清单装载：affected = 行数、LAST_INSERT_ID = 首个自增 id、计数器每条自增语句预留 64（65 → 129 → 257）；pk 重复静默覆盖；UNIQUE 命中 1062 整句原子但预留烧号；1136 行数/Str→Int、1048 NOT NULL、1292 坏 DATE 都不进分配器；RESP ZSET 记被拒 ref，Rust 校验过的行才装载 |
| 173 | `scenario_s3_expect_continue_incomplete_e2e.rs` | S3 raw HTTP + RESP + MySQL | 数据湖导出：`Expect: 100-continue` 的 100 行先于 auth（无 bearer 仍得 100 再 401 AccessDenied）；零长 PUT 与 GET 带 Expect 无 100；截断上传（CL 10 写 4 字节半关）400 `IncompleteBody` 且无对象无暂存文件；`abc`/`-1` Content-Length 400 `malformed content-length`；ETag == 引号 md5（空 = d41d8c…）、HEAD Content-Length、默认 octet-stream、无 sidecar 退化 `"<size>-<mtime_secs>"`、`application/x-ndjson` 回显、`x-amz-meta-*` 不回；list 前缀下实体转义 `&lt;&gt;&amp;&apos;&quot;`；RESP hash 目录 + MySQL `s3_catalog` 与服务体 md5 三方相等 |
| 174 | `scenario_es_health_init_census_e2e.rs` | ES HTTP + RESP (CLUSTER INIT/KEYSLOT)，两节点 | 两节点上线：INIT 前每节点 health YELLOW `number_of_nodes: 1` 且 `_cat/indices` 仅表头，INIT 后双绿 2；按槽带规则布点含边界（8192 → node0、8193 → node1），CLUSTER KEYSLOT 与预测一致；外槽 PUT 400 `routing_exception` 带槽号与 owner；普查行 `green open <name> <md5+8> 1 0 <docs> 0b` 按名排序，`?v=true` 同文；create 201/update 200 不变/delete 200 减一、miss 404 `found:false`、`_search` hits.total、`_count`；`_cat/health` 与 `/_cat/indices/x` 落到 `/{index}` 形式（owner 404，否则 400） |
| 175 | `scenario_redirect_metrics_backup_e2e.rs` | monitor HTTP + RESP + backup listener，两节点 | SRE 重定向记账：共享直方图把每次 -MOVED/-ASK 记为 `ack="true"`、本地执行记 `ack="false"`，按进程 `type="normal"\|"backup"`——非 owner 上 SET+GET 落 node0 ack=true set/get +1、node1 本地 SET ack=false set +1 且真正持有键；`SETSLOT MIGRATING` 下缺键 -ASK 记 ack=true get，STABLE 后 `$-1`；备份口（自有 RESP 地址）INIT 前本地服务，INIT 后对每个键读都 -MOVED 到带主，SET/CLUSTER INIT/NODES 皆 READONLY，PING +PONG，node1 无 backup 序列；`raft_stats{status="Leader"}` 合计 1 |

## 追加钉死（写进覆盖地图的语义点）

- FT：同 tf 的 BM25 按文档长度排序、稀有词 > 常见词、隐式 AND 求和、`|` 并集、K1/B
  调参（B=0 抹平长度项）、LIMIT 翻页时 SORTBY 的计数口径、FT.INFO 语料统计、UPDATE +
  FT.DEL 后重排。
- 事务：WATCH JSON + HINCRBY 看板跨连接竞争、读保持 watch、MULTI 外自写/DISCARD 清除、
  排队时 CROSSSLOT 整体中止、FT 索引只跟已提交事务。
- RENAME 对 hash/zset/set/list/JSON/vectorset/FT 索引的搬迁 + TTL 随迁/替换 +
  RENAMENX 占用拒绝；MySQL 计划表驱动。
- Lite DLQ：选项矩阵、XCLAIM 抬 delivery（JUSTID 不触发）、超限转移四件套、XAUTOCLAIM
  第三段、`op="dlq"` 计数与 `rdb_lite_dlq_depth`、committed-id = 连续 ACKED 前缀。
- RocksMQ HTTP：auth → 404 → 405+Allow → 400 路由序、`/produce` 文本 id 回复、`/consume`
  base64 + 自动建组（消费者 `http`）、`/pending` 形状、`/ack` 幂等与未知组 404、`/range`
  begin/end 三种界；裸通道 == Kafka topic p0 == RESP `<name>/q0`，Kafka 投递经 HTTP 可消费。
- 多流 XREADGROUP 扇入的段序、`$` 起始、Kafka 账本对 XTRIM 的守卫与 DESTROY 折叠、
  DELCONSUMER 回 purged 数、XPENDING 摘要扁平形状、NOGROUP。
- Kafka：session expiry 驱逐 → 27 → 新 generation；leader 分配的 SyncGroup 往返与
  22/23/24/25 梯；跨 generation 的围栏提交与 LeaveGroup rebalance 存活。
- SQL：EXPLAIN 行形状（PkLookup/IndexScan/SeqScan/Filter/Order/Limit）、`Direct execution`、
  SHOW INDEX 6 列、残余过滤；INNER/LEFT/RIGHT join 的 NULL 语义、合取换序、NULL 排序、
  `NOT IN (.., NULL)`、`<=>`；多行 INSERT 的 affected/LAST_INSERT_ID/64 预留/1062 烧号/
  1136/1048/1292 不进分配器；turso 库上同一组读取的逐行相等。
- S3：100-continue 先于 auth、零长无 100、IncompleteBody 半关 400、坏 Content-Length、
  ETag = md5、sidecar 退化 ETag、content-type 回显、`x-amz-meta-*` 不存、list XML 转义。
- ES 两节点：INIT 前 YELLOW/1 → INIT 后 GREEN/2、带边界布点、routing_exception 文案、
  `_cat/indices` 本地普查 + uuid + docs.count 增减、`?v` 忽略、`_cat/health` 与
  `/_cat/indices/<pattern>` 的落点。
- 指标：`ack="true"` 只在 MOVED/ASK 行、`type="backup"` 与 normal 共用 collector、备份口
  READONLY 门（SET/CLUSTER INIT/NODES）、备份口 INIT 后对全部键读 -MOVED、MIGRATING
  源本地服务已有键、`raft_stats{status="Leader"}` 唯一。

## 真实行为偏差（当轮断言并记录在案，未削弱测试）

- #161：普通 FT.SEARCH 回复是单扁平数组且 `:total` 为页大小（RediSearch 为全量命中
  数，`src/search/ft_search.rs:181`），全量计数只在 SORTBY 下出现（`ft_search.rs:232-259`）；
  NUMERIC range 节点记常数 1.0 而非 BM25（`eval_tree.rs:198-208`）。
- #162（缺陷疑似）：对 lite 流键 WATCH 永不触发——router 用全名哈希
  （`src/command/mod.rs:449-450`），lite 存在父名的槽前缀下（`src/lite/append.rs:56`）；
  该腿移除，不断言。
- #163（缺陷疑似）：普通流名对 RENAME 不可见（`-ERR no such key`，XLEN 1 / EXISTS 0
  同样成立），RENAME 不带 delivered 水位（`src/lite/offset.rs:57`），旧 (stream, group)
  的 offset 缓存无驱逐（`offset.rs:57-115`、`keys_core.rs:342`：`XREADGROUP <old> >`
  得 `*-1` 而非 NOGROUP，`XACK <old>` :1 复活组）；流相关腿移除，其余类型族照常钉死。
- #164：committed-id 是连续 ACKED 前缀（id1 pending 时 `0-0`，转移后变 id1；
  `offset.rs:164-207`，`resolve` 同规则 `:192-198`）；last-delivered-id 是持久化投递水位（`read.rs:420`），由下一次
  committed 推进落盘。
- #165/#166：`/produce` 回 TEXT（体 = id，`api.rs:176`）；`/range` 参数名 `begin`/`end`
  （`range.rs:35`）；`/ack` 不查 pending/归属（`api.rs:184-216`）；lite 组的 OffsetFetch
  为 `(0, -1)`（`offsets_fetch.rs:161-185`）；last-delivered-id 是持久化头（`offset.rs:143`）；
  `delay_ms` 错误无 `bad query string:` 前缀（`api.rs:124-126`）；CreateTopics 拒绝含 `/`
  的 topic（17，`mapping.rs:39-45`，故用 `inc166k` = 流 `inc166k/p0`）；XGROUP
  DELCONSUMER 回清掉的 pending 数（`group.rs:458-491`）；XINFO GROUPS 13 元无
  consumers/pending（`info.rs:57-105`）；XPENDING 摘要扁平 `*5`（`pending.rs:71-81`）；
  `CREATE .. $` 用流尾 id 播种 committed-id（`group.rs:236-254`）；裁剪后 Kafka log end
  跟随流存活长度（`append.rs:369`、`mapping.rs:68-70`）。
- #167/#169：Kafka 面创建的 topic 流名是 `<topic>/p0`（`mapping.rs:25-33`），非 brief
  的 `q0`；#169 的 Fetch session 腿因已知缺陷疑似（`fetch.rs:196-199`）不写。
- #170：pk 上的 OR 在 `CREATE INDEX ix_id` 后仍走 SeqScan（`plan/mod.rs`
  collect_conjuncts）；`>` 对 IndexScan 不可 sargable（BETWEEN 可以）。
- #171（缺陷疑似）：turso 渲染器把 `x IS NULL` 写成 `(x IS) NULL` → 1064
  （`src/sql/turso/translate/expr.rs:33-36`），`IS NULL`/`IS NOT NULL`/`<=>` 改为 host
  专用块，turso 跑其余全部读取并逐行相等。host NULL 排序与 MySQL 相反（`select.rs:671`）。
- #172：自增计数器每条自增语句预留 64（65 → 129 → 257，`sequence.rs:37,84-89`），UNIQUE
  命中整句回滚但预留已烧（下一条 257 = 193 + 64）；pk 重复静默覆盖（`write.rs:165-195`）。
- #173：`x-amz-meta-*` 从不存储/回显（`router.rs:283-285`）；checkpoint 发布者持续向桶
  `rdb` 写 `rocksdb/...`，list 只在导出前缀下读；plain INSERT 重注册已有 PK 静默覆盖
  （`write.rs:182-190`）。
- #174：`/_cat/health`、`/_cat/indices/<pattern>` 不是 `_cat` 路由，落到 `/{index}` 形式
  （`router.rs:75-94, 216-222`：槽 owner 404 `no_handler_found_exception`，否则 400
  routing）；`?v` 从不读取；`cat_indices` 只扫本地存储（`misc.rs:76-102`）；ES `slot_check`
  无 hash tag（`write.rs:76-102`）。
- #175：备份口共享拓扑但不共享 RESP bind，INIT 后对每个键读都 -MOVED 到带主，包括
  node0 自有槽（`main.rs:493` 换 bind、`:506` 克隆拓扑；`router.rs:30-48`）；MIGRATING 源对已有键本地服务、
  仅缺键 -ASK（`command/mod.rs:366-371`）；`ack="true"` 仅见于 MOVED/ASK（`mod.rs:468-472`）。

## 回归与卫生

- 15 个新文件 256–369 行（上限 400），纯函数、ASCII、无敏感数据、每文件 0 个 `rf(`；
  `tests/scenario/main.rs` 仅追加 15 行 `mod`（字母序，`sort -c` 校验）。
- 性能（串行实测，预算按 max(2s, 3× 实测) 向上取整）：
  `bm25/8-search+2-sql` 337–439ms、`watchjson/5-tx+1-produce` 27–67ms、
  `rentyped/8-renames+plan` 84–214ms、`dlq/transfers` 254–350ms、
  `rmqhttp/bridge-cycle` 85–102ms、`fanin/read-fanin` 48–54ms、
  `kexpiry/rejoin-dance` ~54ms（另 `at_least kexpiry/eviction-honors-session` ≥1s，
  实测 ~1.9s 真实 session 等待）、`kassign/sync-handoff` ~303ms（`at_least
  kassign/follower-parked` ~250ms）、`kfence/fence-ladder` ~30ms、
  `pkplan/explain-ladder` 0.97s（≤3s，~20 个结果集语句 × 42ms）、`pkplan/after-ix-id` 0.17s、
  `conjnull/join-null` 0.55s + `conjnull/turso-join-null` 0.57s、
  `multirow/error-ladder` 0.13s、`s3cont/incomplete-body` <2ms +
  `s3cont/put-get-etag` 19–46ms、`eshealth/census` 5–8ms、
  `redirmetrics/redirect-accounting` 4–8ms；整例 0.3s–5.3s（两节点各 3.6s/5.1s，后者含
  5s raft tick 等待）。
- 既有、未改：MySQL 前端每个结果集语句固定 ~42ms（`src/sql/front/serve.rs:66` 的
  监听 socket 未 `set_nodelay`），与批次 12/13 记录一致。
- 门禁：15 例串行各 3 轮全绿（写者）+ 编排者串行 1 轮；scenario 175/175（291.69s，
  `RUST_TEST_THREADS=4`）；`cargo clippy --test scenario -- -D warnings` 零告警；全量
  `cargo test --workspace --no-fail-fast`（`RUST_TEST_THREADS=4`）2186/2186 绿（scenario
  175/175 300.20s、sql 139、lib 1432）。首轮全量跑出 2 个既有夹具负载 flake
  （`rocksmq_http_e2e::produce_group_consume_ack_flow`：`resp_cmd` 固定 300ms 读窗在满载下
  读不到 AUTH 回复，`tests/rocksmq/rocksmq_http_e2e.rs:171` 的 `split_at(5)` 越界；
  `sql_2pc_e2e::follower_coordinated_update_delete_reaches_all_slot_owners` 与一次 scenario
  全量中的 `scenario_sqlrpc_twopc_e2e::settlement_spans_shards_over_sql_rpc_and_vetoes_dupes`：
  多节点 join `head timeout`），三例单独复跑各 2–5 轮全过，第二轮全量 2186/2186；与批次 13
  记录的 `sql_2pc_e2e`/`sql_join_cluster_e2e` flake 同类，均为既有、未改。
- 评审收尾（只改注释/文档，不动断言）：修正 changelog 三处 src 行号引用（#162 `mod.rs:449-450`、
  #164 `offset.rs:164-207`、#175 `main.rs:493,506`）与 #163 文件头同步，性能表补
  `pkplan/after-ix-id`；增量构建复验通过。
