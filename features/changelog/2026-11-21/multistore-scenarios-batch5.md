# 多存储场景批次 5：#41–#60 二十例 e2e（2026-11-21）

承接批次 1–4（2026-11-19/20/21），批次 5 按用户要求**再扩 20 例**复杂场景，
编号 #41–#60，仍只读复用 `tests/scenario_common/` 夹具、不改 `src/`。覆盖
此前未触达的 RESP pipelining、lex 有序队列、FT 索引拆除、向量去重属性、
流压缩归档与显式 checkpoint 保留策略。`tests/` 净增 20 个文件、全部
≤190 行（上限 400）。回归：170 个 e2e 目标 + lib 1343 + bins 4 全绿，
clippy `-D warnings` 零告警。

## 场景矩阵

| # | 文件 | 前端 | 故事线要点 |
|---|------|------|-----------|
| 41 | `scenario_airline_seatmap_upgrade_e2e.rs` | RESP+MySQL | HSETNX 锁座一次性、ZADD 里程榜、ZPOPMAX `*2 [member,score]` 授最高里程升舱、LMOVE standby→confirmed、LRANGE+`bulk_at`/LPOS 对账、SQL 航班座位表 |
| 42 | `scenario_fraud_ring_detection_e2e.rs` | RESP+MySQL | SINTERCARD（numkeys 前缀）设备交集、SDIFF 独占设备（`bulk_at` 断言）、SUNIONSTORE 全足迹 :4、SETBIT 指纹+BITOP AND/OR 合并、HINCRBY 风险分、SQL 案件表 |
| 43 | `scenario_content_moderation_queue_e2e.rs` | RESP+RocksMQ+MySQL | mq 种子→consume b64 解码→LPUSH 分诊→LMPOP RIGHT 首非空队列弹出→ZADD+ZREMRANGEBYRANK 只留 top3→SPOP 抽查→SQL 台账+ack 清零 pending |
| 44 | `scenario_ride_hailing_match_e2e.rs` | RESP | ZRANGEBYSCORE 半径带、ZCOUNT 含/排除界两种写法、ZRANK/ZREVRANK/LPOS 定位、ZINCRBY 热度、HINCRBYFLOAT 计价、GETSET 换令牌、LREM+ZREM 收口 |
| 45 | `scenario_warehouse_wave_pick_e2e.rs` | RESP+MySQL | ZINTERSTORE 求和+WEIGHTS 加权、ZDIFFSTORE 独占拣、SMISMEMBER 批查（`bulk_at` 逐位）、SQL 拣货行 |
| 46 | `scenario_dns_record_zone_e2e.rs` | RESP+MySQL | SETEX/TTL 窗口、APPEND/STRLEN/GETRANGE（双端含）/SETRANGE 改写、GETSET 换 SOA serial、MSET/MGET（nil 槽）、INCRBYFLOAT 权重微调、SQL zone 注册 |
| 47 | `scenario_crypto_exchange_candles_e2e.rs` | RESP+MySQL+ES | ZCOUNT 界内/外、ZMSCORE nil 槽、ZPOPMAX `*2` 清算最高价、XADD `mkt/tape` 成交带（回 `$id`）、SQL OHLC、ES 显式建索引+_bulk+term 单命中 |
| 48 | `scenario_loyalty_tier_sunset_e2e.rs` | RESP | ZREVRANGEBYSCORE 铂金带、ZRANDMEMBER 非破坏抽奖、ZSCAN WITHSCORES 全走游标归 0、SUNIONSTORE 礼包并集、EXPIREAT 2s 落日+PTTL 证钟 |
| 49 | `scenario_chat_edit_history_e2e.rs` | RESP | RPUSH/RPUSHX 回新长度、LINSERT BEFORE/AFTER 包夹编辑标记+失锚 -1、LSET `+OK`、LINDEX bulk、LREM 0 全删重发、LPOS 命中、RPOPLPUSH 同槽轮转入档（跨槽 CROSSSLOT） |
| 50 | `scenario_image_thumbnail_pipeline_e2e.rs` | RESP+MySQL+S3 | JSON.SET 管线文档、JSON.ARRINSERT 插队（值须合法 JSON）回新长度、越界索引拒、ARRLEN/OBJLEN/STRLEN、JSON.TYPE 状态行、S3 `/rdb/<key>` 字节级回程+LIST XML、SQL 资产台账 |
| 51 | `scenario_feature_flag_rollout_e2e.rs` | RESP+MySQL | JSON.MGET 同槽多键批（nil 槽 `$-1`）、JSON.TYPE `+integer/+object`（trim `+` 断言）、JSON.STRLEN 字符串字段、NUMINCRBY 10→25.5→40、HDEL kill、SQL 审计表 |
| 52 | `scenario_blackfriday_pipeline_e2e.rs` | RESP | 真 RESP pipelining：六命令单 TCP 写、回复严格按提交序逐行对齐；批中未知命令不毒化（错误内联、后续写落地）；整段 MULTI..EXEC 同批（`+QUEUED`×3 后嵌套 `*3 [+OK,:3,:1]`） |
| 53 | `scenario_rate_limit_token_bucket_e2e.rs` | RESP+MySQL | INCR 桶计数、EXPIREAT 窗口+TTL、GETSET 原子清零回旧值、DECR/DECRBY 跨负数、INCRBY 回冲、APPEND/STRLEN/GETRANGE 429 文案、SQL 配额表 |
| 54 | `scenario_job_scheduler_cron_e2e.rs` | RESP+MySQL | ZRANGEBYLEX `[0 (1` 字节序窗口+LIMIT 分页、ZLEXCOUNT（空键 0）、ZREMRANGEBYLEX 执行退役 :2、XADD `jb/fire` 显式段名（回 `$id`）、XREVRANGE 新→旧 vs XRANGE 旧→新同 id 集、SQL 运行台账 |
| 55 | `scenario_product_search_autocomplete_e2e.rs` | RESP+MySQL | FT.CREATE/FT.ADD（:1）精确词 FT.SEARCH、缺索引 `-ERR unknown index`、FT.DROP :1 后查询同错、FT.DROPINDEX 别名同语义（无留文档变体）、ZRANGE 建议榜+ZSCORE、SQL LIKE 前缀 |
| 56 | `scenario_cdn_edge_logs_e2e.rs` | RESP+RocksMQ+ES+MySQL | 三 PoP 五行访问日志入 mq→consume 整批 b64 解码→SETBIT 每 PoP 位图+BITOP OR 全局触达（结果 1 字节）+BITCOUNT=2 去重客户端→ES _bulk 5xx 两行+term(pop) 单命中→ack 清零 pending→SQL QoS 台账 |
| 57 | `scenario_versioned_config_publish_e2e.rs` | RESP+MySQL | HMSET 一次三字段、HMGET nil 槽、HLEN/HKEYS/HVALS 投影、HINCRBYFLOAT 3+1.0→"4"（去尾 .0）、HSETNX 一次性审批、HSCAN COUNT 全页游标 "0"、HRANDFIELD、TYPE `+hash`、SQL 发布台账 |
| 58 | `scenario_vector_dedup_cleanup_e2e.rs` | RESP+MySQL | VADD `VALUES <dim> <elem> <f...>` :1/:0 去重（重复写**覆盖向量但保属性**）、VCARD、VSETATTR :1/:0、VGETATTR bulk/nil、VSIM COUNT 重排验证覆盖生效、VREM :1/:0 末元素抹全族、SQL 镜像清理 |
| 59 | `scenario_stream_compaction_e2e.rs` | RESP+MySQL | XADD 显式 id（回单 bulk）、XTRIM MINID :2 头部压缩、XDEL :1/:0 点删、XREVRANGE 新→旧、XREAD 非组 `0-0`（裸 `0` invalid）、SQL 归档表承接裁剪 id |
| 60 | `scenario_dr_drill_rpo_audit_e2e.rs` | RESP+S3 | 显式 `s3_checkpoint_retention: 1`（默认 2）：第二个 distinct ckpt id 出现后清扫只留最新完整集（meta.json 在位）；本 FS 单次 publish=staging 拷贝+fsync 秒级、节律 publish-bound；DBSIZE≥3 / RANDOMKEY 盘点 |

## 本批钉死的新语义（写前验证 + 实测确认）

- **JSON.TYPE** 回**简单串** `+integer/+object/+array/...`（非 bulk）；缺键/缺
  路径回 `$-1`。整数与浮点区分 `integer`/`number`（i64/u64 才是 integer）。
- **JSON.ARRINSERT** 的值参数必须是**合法 JSON**（裸字符串报 `invalid JSON`）；
  成功回**新数组长度**；越界索引拒绝且不动文档。
- **JSON.STRLEN** 只吃字符串路径：数值路径报 `wrong type of path value`。
- **XADD 回执形状随名而变**：斜杠流名（父/子段，如 `jb/fire`、`mkt/tape`）回
  **单 bulk `$id`**；裸父 topic 名回 `*2 [stream-bulk, id-bulk]`（自动轮询隐式
  子队列）。topic 名禁 hash tag 大括号（`{jb}fire` 报 invalid topic name）。
- **XREAD 非组**：起始 id 须全形 `0-0`，裸 `0` 报 `Invalid stream ID`。
- **BITOP** 回**结果字节长度**（Redis 语义），不是 popcount——popcount 须
  BITCOUNT 另读（AND 结果 1 字节与 popcount 1 的巧合曾掩盖该形状）。
- **FT 拆除族**：FT.DROP 回 `:1`，二次/查询均 `-ERR unknown index`；
  **FT.DROPINDEX 是 FT.DROP 的别名**（全族含文档同删，无留文档变体——与
  Redis 的 DROPINDEX 留文档语义偏差，记录在案）；FT.ADD 回 `:1`（非 +OK）。
- **HINCRBYFLOAT** 整型现值+浮点增量回**紧凑形式**（"4" 非 "4.0"）。
- **S3 夹具形状**：`s3_call` 第二返回值是**响应头**、XML 载荷在第三返回值；
  键读写走 `/rdb/<key>` 路径、列表 `/rdb?list-type=2&prefix=...`。
- **ckpt 保留策略**：`s3_checkpoint_retention` 显式生效（默认 2）；publish 是
  staging 目录拷贝+fsync，单次秒级，interval 只是睡间隔——观测节律由 publish
  时长决定；清扫在 promote 后瞬间完成。
- **数组回复断言纪律**（本批两处返工的教训）：SDIFF/SMISMEMBER/MGET 等数组类
  回复一律 `rf` 全排空 + `bulk_at(reply.as_bytes(), i)` 逐元素断言，禁止拿
  `rf` 原文与裸字面量做整串相等。

## 回归与收尾

- 170 个 e2e 目标（150 旧 + 20 新）全量分片回归绿（3 例负载抖动单跑复验：
  fraud/image/dr60 修复后 3/3 过）；`cargo test --lib` 1343 过、`--bins` 4 过。
- `cargo clippy --workspace --all-targets -- -D warnings` 零告警（loyalty 一处
  `&tmp.to_string().as_bytes()` needless_borrow、crypto 一处未用 `resp`）。
- 文档：`features/e2e-coverage.md` 40→60 例 + 批次 5 小节；`agents/rust/index.md`
  计数同步。
