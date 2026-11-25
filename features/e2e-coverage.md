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
| 进程级多存储场景 | `tests/scenario/`（target `scenario`）+ `tests/scenario_common/`（多前端单节点 `front_world` + 3 节点集群） | 真实业务场景横切多存储面 |
| 前端专用 mini-harness | `kafka_front_common/`、`es_common/`、`s3/`、`rocksmq_http_e2e` | 真实二进制 + 手搓协议客户端 |
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
- **多存储真实场景套件（2026-11-19 首批 10 例 + 2026-11-20 批次 2/3/4 + 2026-11-21 批次 5/6/7 + 2026-11-22 批次 8/9/10 + 2026-11-24 批次 11 + 2026-11-25 批次 12，`tests/scenario/scenario_*_e2e.rs` 145 例 + `tests/scenario_common/` 公共夹具）**：
  业务故事横切多个存储面（KV 家族/JSON/Vector/FT/lite-MQ/SQL/RocksMQ-HTTP/ES/Kafka/S3），
  断言的是「同一份数据在不同面之间的可见性与一致性」而非单命令：
  `order_pipeline`（购物车 Hash→订单 SQL UNIQUE→ZSET 榜单→stream 事件→XACK）、
  `session_profile`（六类家族 + PX 过期判决 + kill -9 存活矩阵）、
  `social_feed`（Set 关注图 + SINTER（hash-tag 键）+ List 收件箱 LTRIM + ZSET 热榜 + FT 中文分词双查询）、
  `reco_vector`（VADD/VSIM 召回 + Hash 元数据 + SQL 目录对账）、
  `log_pipeline`（RocksMQ /produce→/consume b64 载荷→ES 需显式建索引 + term 查询→SQL 台账对账→RESP 侧 XLEN 互查）、
  `delay_jobs`（XADD DELAY 暂存换新 id + SET NX PX 租约 + MAXDELIVERY 2 的 DLQ 三跳 + SQL 台账）、
  `risk_control`（INCR 窗口 + Set 黑名单 + SETBIT 指纹 + ZADD/ZREMRANGEBYSCORE 滑窗 + SQL 审计）、
  `iot_telemetry`（Kafka Produce v3 上行→JSON.SET 影子→HGET 最新值→告警）、
  `media_archive`（S3 PUT/GET/LIST + RESP 签名键 + 重启存活）、
  `multistore_failover`（3 节点集群：SQL 2PC 死带显式拒绝 + KV 槽 MOVED 诚实停摆 + lite 流 raft 复制读 +
  kill -9 后新 leader 续写 + respawn SQL 行追平）；
  批次 2（2026-11-20，编号 #11–#20，互不重复的多平面横切）：
  `payment_ledger`（INCRBY 钱包→SQL 双分录 UNIQUE(entry_no) 重放 COMMIT 拒绝→XGROUP 消费→ZSET 收款榜→DELAY 对账 job→SUM(D)=SUM(C)=榜=170 且转账守恒 950）、
  `flash_sale`（INCRBY 库存 50→47 + SET NX PX 限购锁 nil 拒绝 + UNIQUE(order_no) 重放 + ZINCRBY 销量榜 + LPUSH 中奖名单）、
  `chat_offline`（SET NX PX 会话锁 + HSET/HGETALL 画像 + XGROUP 在线直 ACK/离线 LPUSH 落箱 + GETDEL 回执烧毁 + ES term 大小写敏感）、
  `config_center`（JSON.SET/GET/NUMINCRBY `.rev` + INCR epoch + FT 三服务检索 + S3 快照字节回读 + SET PX→EXPIRE→PERSIST(TTL -1)）、
  `metrics_rollup`（INCR 分桶 + SETBIT 重复置位 :1 不虚增 BITCOUNT UV + ZINCRBY/ZREVRANGE 热点序 + XRANGE 重放计数 + SQL 对账三面同值）、
  `inventory`（HINCRBY 出入库 + SADD/SMOVE 仓库集 + ZADD 缺口榜 ZREMRANGEBYSCORE 清队 + SQL movements SUM=HGET qty + RocksMQ 补货指令闭环）、
  `ticket_seats`（SADD 占座 + SET NX PX 座位租约 + UNIQUE(seat) 对手单 COMMIT 拒 + RPUSH/LPOP 候补 FIFO + DELAY 超时释放 job 后 SREM :1）、
  `push_news`（Kafka Produce v3 带 headers 上行→record_fields k/v/h 落流→XREADGROUP 逐条扇出 HSET/ZINCRBY/SQL/ES→ZREVRANGE 水位序）、
  `game_season`（ZINCRBY 天梯 ZREVRANGE 冠军序 + JSON 档案 NUMINCRBY + VADD/VSIM 召回 + EXPIRE/PERSIST buff + SQL 结算 SUM=ZSCORE 和）、
  `audit_archive`（RESP+MySQL+ES+S3 四前端单节点齐聚：SQL 审计 6 行 + XGROUP 消费 ACK + ES term + S3 字节回读 + ZREVRANGE 工作量榜）。
  行为要点（写作时钉死的语义）：HSET/SADD/LPUSH/ZADD/SETBIT/EXPIRE 等回 `:N`，GET/HGET/JSON.GET/ZSCORE/
  ZINCRBY/XADD 回 bulk（`scenario_common::rb` 兜 `$-1`）；XGROUP 起点 `0-0`；流名仅 `A-Za-z0-9_.-`（不能带 `{}`，
  hash-tag 只能用于 KV 键）；单节点也强制 CROSSSLOT；ES 无自动建索引且 match 查询会 lowercase 而keyword 文档
  不分词（大小写敏感场景用 term）；RocksMQ /consume 的 body 是 base64；集群上 lite 写/组操作任意节点可受理
  （raft 复制），但 `XREADGROUP >` 投递只在 leader 生效；SQL 快照事务的 dup 冲突在 COMMIT 时才判决；
  批次 2 追加钉死：SREM/SMOVE/TTL/SMEMBERS/GETDEL/JSON.NUMINCRBY 均已注册；多键 Set 运算（SMOVE）必须
  hash-tag 同槽（`{inv}wh:*`）否则 CROSSSLOT；无 PRIMARY KEY 的建表直接 1235 拒绝；`IN (...) ORDER BY`
  返回序以 IN 列表序为准；HSET 对已存在字段返回 0（只数新字段）；Kafka topic `t` 落流即 `t/q0`、
  record 字段名为 `k`/`v`/`h`（header 名 hex 编码）。
  批次 3（2026-11-20，编号 #21–#30，互不重复的多平面横切）：
  `bank_2pc_cluster`（3 节点 sql_rpc 集群：开户批 24 行跨槽 2PC 提交 + 三节点 SUM 读一致 +
  UNIQUE(txid) 重放远端 dup 否决全局消失 + /sql2pc/status unknown/committed 判决）、
  `hotel_calendar`（SETBIT 入住日历 + BITCOUNT + ZRANGEBYSCORE 价窗 + SINTER（hash-tag `{amen}`）设施交集 +
  UNIQUE(night) 重订 COMMIT 拒）、
  `exchange_orderbook`（双 ZSET 买卖簿 ZREVRANGE/ZRANGE 最优价 + ZREM 撮合双撤 + SQL trades + INCRBY/HINCRBY ticker + XADD tape）、
  `subscription_renewal`（DELAY 续费波 XLEN 不可见→交换→XREADGROUP 按 churn 名单分支 SMOVE + UNIQUE(user_cycle) 账单重放拒）、
  `election_ballots`（UNIQUE(ballot_key) 防重投 + HINCRBY 汇总与 SQL GROUP BY 对账 + ES `_doc/{ballot_key}` 幂等镜像 term 区域计数）、
  `courier_dispatch`（RocksMQ /produce→/consume 双派单员竞争 SET NX PX 认领 + UNIQUE(order_key) + ack 清零 + TTL 审计）、
  `insurance_claims`（S3 证据字节级回读 + 前缀 list + 证据 GET 200 才准核的 gate + INCRBY 赔付台与 SQL SUM 对账）、
  `clinic_slots`（UNIQUE(slot_key) 双订 COMMIT 拒 + DELAY 提醒波 + 投递按 SQL status 分支（cancelled 不发短信）+ XPENDING 清零）、
  `flight_ground`（ZADD 时刻表 + ZRANGEBYSCORE 晨窗 + WHERE 守卫的 UPDATE 轮转（尾号未落地零行生效）+ IN(...) 重订 + ZREM/ZCARD 清板）、
  `smartmeter_grid`（Kafka Produce v3 带 region headers→XRANGE hex 头互证 + INCRBY 分表 + SQL GROUP BY 区域汇总 +
  last_batch 守卫离网翻转 + S3 原始批字节归档）。
  批次 3 追加钉死：XREADGROUP 的 GROUP 参数是组名（写流名必得 NOGROUP）；XPENDING 汇总回复是
  `*3\r\n:0\r\n$-1\r\n$-1\r\n` 形数组非单整数；ZADD/ZREM 多成员回 `:N` 非 +OK；HGET 回 bulk 需
  `rb().parse()`；SQL 唯一性裁决只认 `UNIQUE KEY` 子句（PRIMARY KEY 列不裁重）；dup 冲突即便显式
  BEGIN..COMMIT 也只在 COMMIT 边界回 1062；DELAY 交换后建组/读组均正常（组与流元数据独立存储）。
  批次 4（2026-11-21，编号 #31–#40，互不重复的多平面横切，changelog
  `changelog/2026-11-20/multistore-scenarios-batch4.md`）：
  `kafka_consumer`（RESP 侧 XADD 裸父 topic ×6→手搓 Fetch v0 整批读回 hwm=6/batch(0,6)→OffsetCommit v0 ship@4→
  OffsetFetch 回 4→@4 续读尾两单 batch(4,2)→未提交新组 -1 哨兵）、
  `saga_compensation`（SAVEPOINT sp_car 部分补偿只回滚租车腿、ROLLBACK TO 幂等/RELEASE 后报 does not exist、
  1305 失败腿、三路 LEFT JOIN NULL 腿渲染 `<NULL>`）、
  `dispatch_blocking`（BLPOP 唤醒 `*2`<4s/空队列 1s 超时 `*-1`/BRPOPLPUSH `{job}` 同槽唤醒+落 done 列/超时 `$-1`/
  BZPOPMIN 唤醒 `*3` 含 9.5）、
  `checkout_watch`（双连接 WATCH 竞争胜者 EXEC `*2 :2 :1` 败者原始 `*-1`、重试胜出、EXEC 消耗 watch、
  CROSSSLOT/blpop 入队拒绝原文+EXECABORT）、
  `checkpoint_dr`（S3 灾备演练：等 id≥写完成时刻且 meta.json 在位的 ckpt→kill_now→抹数据目录保 `raft/` 子目录→
  拷回 ckpt 文件跳 meta.json/边车→respawn→预写数据全复活）、
  `key_lifecycle`（EXPIRE/TTL/PERSIST、RENAME 迁 TTL、8 类 TYPE、SCAN 游标全走+MATCH+COUNT 分页+TYPE 过滤、
  KEYS 对照、多键 DEL/EXISTS `{kc}` 同槽、惰性过期、流名 TYPE quirk：`TYPE <流名>` 回 none 但 SCAN TYPE stream 能见）、
  `event_sourcing`（XADD 回执↔XRANGE id 对账、XREADGROUP 全量投递+XACK :4、空 PEL `*3`、XPENDING 非空扩展 `*5`、
  XAUTOCLAIM `*3` 重领、XINFO GROUPS/CONSUMERS）、
  `cart_json_vector`（JSON 购物车 ARRAPPEND/ARRLEN/STRAPPEND/OBJKEYS/DEL+SQL 结账+FT.CREATE TEXT+VECTOR DIM 4/
  FT.BUILD/FT.INFO ann_centroids:2/KNN 召回/FT.DEL+VADD 方言 `VALUES <dim> <member> <f...>`×3/VSIM 最近邻）、
  `es_bulk_reindex`（目录 ETL：显式 `PUT /<index>` 带 mappings 后 _bulk created×4 errors:false、update 翻 errors:true、
  doc DELETE→deleted→not_found 非 404、_count/match 收缩）、
  `gdpr_erase`（SQL 伪名化 UPDATE、ES doc 删除幂等、DEL/JSON.DEL :1→:0、u2 完好 KEYS 对照）。
  批次 4 追加钉死：ES _bulk 不自动建索引（须先 PUT 索引，items[].index.status=201）；SCAN/KEYS 是白名单全局命令
  （空前缀跨 slot 扫全库）；流名禁 hash tag、`TYPE <流名>` 回 none；BRPOPLPUSH src/dst 不同槽停车时立即回
  CROSSSLOT（永不唤醒）；BLPOP 超时 `*-1`/BRPOPLPUSH 超时 `$-1`/BZPOPMIN 唤醒 `*3`；XPENDING 非空=扩展 `*5`；
  VADD 方言 member 在浮点前；JSON.DEL 数组索引删除后重排（幂等断言须用对象路径）；checkpoint 恢复须保留数据目录下
  `raft/` 子目录、s3data 根是 ProcNode.dir 的兄弟目录、首 ckpt 在 3s 启动宽限+interval 后。
  批次 5（2026-11-21，编号 #41–#60 再扩 20 例复杂场景，changelog
  `changelog/2026-11-21/multistore-scenarios-batch5.md`）：
  `airline_seatmap_upgrade`（HSETNX 锁座、ZADD 里程榜、ZPOPMAX `*2 [member,score]` 授升舱、
  LMOVE 候补→确认、LRANGE/LPOS 对账 + SQL 航班表）、
  `fraud_ring_detection`（SINTERCARD 设备交集、SDIFF 独占设备、SUNIONSTORE 全足迹、SETBIT+BITOP
  AND/OR 指纹合并 + HINCRBY 风险分 + SQL 案件表）、
  `content_moderation_queue`（RocksMQ 种子→consume b64 解码→LPUSH 分诊→LMPOP RIGHT 首非空队列→
  ZADD+ZREMRANGEBYRANK 留 top3→SPOP 抽查→SQL 台账 + ack 清零 pending）、
  `ride_hailing_match`（ZRANGEBYSCORE 半径带、ZCOUNT 含/排除界、ZRANK/ZREVRANK/LPOS、ZINCRBY 热度、
  HINCRBYFLOAT 计价、GETSET 换令牌、LREM+ZREM 收口）、
  `warehouse_wave_pick`（ZINTERSTORE 求和+WEIGHTS、ZDIFFSTORE 独占拣、SMISMEMBER 批查 + SQL 拣货行）、
  `dns_record_zone`（SETEX/TTL、APPEND/STRLEN/GETRANGE 双端含/SETRANGE、GETSET 换 SOA serial、
  MSET/MGET nil 槽、INCRBYFLOAT、SQL zone 注册）、
  `crypto_exchange_candles`（ZCOUNT/ZMSCORE nil 槽/ZPOPMAX `*2` 清算、XADD `mkt/tape` 成交带回 `$id`、
  SQL OHLC、ES 显式建索引+_bulk+term 单命中）、
  `loyalty_tier_sunset`（ZREVRANGEBYSCORE 铂金带、ZRANDMEMBER 非破坏、ZSCAN WITHSCORES 全走、
  SUNIONSTORE 礼包、EXPIREAT 2s 落日+PTTL）、
  `chat_edit_history`（RPUSH/RPUSHX 回新长度、LINSERT BEFORE/AFTER+失锚 -1、LSET `+OK`、LINDEX bulk、
  LREM 0 全删、LPOS、RPOPLPUSH 同槽轮转入档）、
  `image_thumbnail_pipeline`（JSON.ARRINSERT 插队回新长度/越界拒、ARRLEN/OBJLEN/STRLEN/TYPE、
  S3 `/rdb/<key>` 字节级回程+LIST、SQL 资产台账）、
  `feature_flag_rollout`（JSON.MGET 多键批+nil 槽、JSON.TYPE 状态行 `+integer/+object`、JSON.STRLEN
  字符串字段、NUMINCRBY 10→25.5→40、HDEL kill、SQL 审计）、
  `blackfriday_pipeline`（真 RESP pipelining：六命令单写回复严格按提交序、批中未知命令不毒化后续
  （错误内联+后续写落地）、整段 MULTI..EXEC 单写 `+QUEUED`×3+嵌套 `*3`）、
  `rate_limit_token_bucket`（INCR 计数+EXPIREAT 窗口、GETSET 原子清零、DECR/DECRBY 跨负数、
  APPEND/STRLEN/GETRANGE 429 文案 + SQL 配额）、
  `job_scheduler_cron`（ZRANGEBYLEX `[0 (1` 窗口+LIMIT 分页、ZLEXCOUNT 空键 0、ZREMRANGEBYLEX 执行
  退役、XADD `jb/fire` `$id`+XREVRANGE 新→旧/XRANGE 旧→新 + SQL 运行台账）、
  `product_search_autocomplete`（FT.SEARCH 精确词、FT.DROP :1→unknown index、FT.DROPINDEX 即 FT.DROP
  别名（无留文档变体，偏差）、ZRANGE 建议排名+ZSCORE、SQL LIKE 前缀）、
  `cdn_edge_logs`（RocksMQ 三 PoP 日志→consume/ack 清零 pending、SETBIT+BITOP OR 合并+BITCOUNT、
  ES _bulk 5xx+term 单命中、SQL QoS 台账）、
  `versioned_config_publish`（HMSET/HMGET nil 槽、HLEN/HKEYS/HVALS、HINCRBYFLOAT 3+1.0→"4" 去尾 .0、
  HSETNX 一次性、HSCAN 全页游标 0、HRANDFIELD、TYPE `+hash`）、
  `vector_dedup_cleanup`（VADD :1/:0 去重复写向量仍保属性、VSETATTR/VGETATTR bulk/nil、VSIM 复写后
  重排、VREM :1/:0 末元素抹族、SQL 镜像清理）、
  `stream_compaction`（XADD 显式 id `$` 批、XTRIM MINID :2、XDEL :1/:0、XREVRANGE 新→旧、
  XREAD 非组 `0-0`（裸 `0` 报 invalid）、SQL 归档）、
  `dr_drill_rpo_audit`（显式 `s3_checkpoint_retention: 1`：第二个 distinct ckpt id 出现后清扫只剩
  最新完整集（本 FS 单次 publish 为 staging+fsync 秒级、节律由 publish 决定）、DBSIZE/RANDOMKEY 盘点）。
  批次 5 追加钉死：JSON.TYPE 回简单串 `+integer/+object/...`、缺键/缺路径 `$-1`（非 bulk）；JSON.ARRINSERT
  值参须为合法 JSON（裸字符串报 invalid JSON）、插队回新数组长度、越界拒；JSON.STRLEN 只吃字符串路径
  （数值路径报 wrong type of path value）；XADD 回执形状随名而变：斜杠流名（父子段）回单 bulk `$id`、
  裸父 topic 回 `*2 [stream,id]`、topic 名禁 hash tag 大括号；XREAD 非组 id 须全形 `0-0`（裸 `0` 报
  invalid）；BITOP 回**结果字节长度**（Redis 语义），popcount 须 BITCOUNT 另读；FT.DROPINDEX 是 FT.DROP
  的别名（全族同删、无留文档变体）、FT.DROP 回 :1 二次 unknown index、FT.ADD 回 :1；HINCRBYFLOAT 整型
  现值+浮点增量回紧凑形式（"4" 非 "4.0"）；SDIFF/SMISMEMBER 等数组回复经 `bulk_at` 逐元素断言。
  批次 6（2026-11-21，编号 #61–#70 再扩 10 例，changelog
  `changelog/2026-11-21/multistore-scenarios-batch6.md`）：
  `sports_playoff_seeding`（ZADD GT/LT/CH/NX 旗标语义、NX+GT 组合 rdb 特有文案、ZINCRBY、
  ZPOPMIN/ZPOPMAX 平铺对、ZREVRANGEBYSCORE+LIMIT、ZUNIONSTORE、SQL 终榜）、
  `url_shortener_slug`（SETNX 占位、INCR 铸 id、GETDEL 烧令牌、SETEX+TTL/PERSIST、
  SETRANGE/GETRANGE、WRONGTYPE、SQL 注册表）、
  `sensor_alarm_bitmap`（SETBIT/GETBIT 车道位图、BITPOS 字节偏移、BITCOUNT、SSCAN 分页、
  SINTERSTORE/SDIFFSTORE 预计算、SMOVE 隔离）、
  `deli_counter_queue`（LINSERT 失锚 -1/缺键 0、LSET、LINDEX 双端 nil、LTRIM 服务窗、
  LPUSHX/RPUSHX 拒建、LREM、SQL 号同步）、
  `org_chart_json`（OBJLEN/OBJKEYS、ARRINDEX 排他 stop、ARRTRIM/ARRPOP/ARRAPPEND、
  STRAPPEND 数值路径报错、NUMINCRBY 紧凑、JSON.DEL、HGETALL 族+HSTRLEN、SQL rollup）、
  `parcel_backlog_xinfo`（XADD 裸父回 [解析后流名,id]、XINFO STREAM 分型字段、XGROUP 0-0、
  XINFO GROUPS、XREAD 重放、XTRIM/XDEL、裸父/缺全名两态）、
  `slot_migration_drill`（双节点：cluster init 等分槽带、IMPORTING 门、MIGRATE ≥6 参 KEYS
  形式、单帧 ASKING 读副本、STABLE 复位、COPY 保源、批量+缺席跳过、RESTORE BUSYKEY 先行）、
  `vector_capacity_gauge`（VDIM/VCARD、VSETATTR/VGETATTR、VSIM VALUES 无 dim 参、VREM 只减计数、
  ES range+size）、
  `energy_settlement_ledger`（INCRBYFLOAT 紧凑、SETRANGE/GETRANGE、HINCRBYFLOAT+HSTRLEN、
  GETDEL、SQL GROUP BY 复合主键）、
  `audience_scan_retire`（SINTERSTORE/SDIFFSTORE、SRANDMEMBER 正负语义、SSCAN/SCAN MATCH+COUNT
  分页、UNLINK 同 hash tag 过 CROSSSLOT、KEYS 清场）。
  批次 6 追加钉死：MIGRATE ≥6 参（单键无 COPY 用 `"" 0 5000 KEYS <key>`）；裸父 topic XADD 回
  **解析后队列流名**（parent/q0）且后续命令沿用；XGROUP CREATE id 须全形 `0-0`；XINFO STREAM
  拒裸父名（要 parent/child 全名）；VSIM VALUES 尾**无 dim 参数**（VADD 才有）；多键命令
  CROSSSLOT 门须同 hash tag；SQL 建表强制 PRIMARY KEY（无则 1235）；raft-http join 不写
  `cluster_slots_stable_instances`——须 `cluster init` + 3s topology tick 才有 -MOVED；
  IMPORTING 门非 ASKING 连接回 -MOVED、ASKING 连接级单帧放行、STABLE 复位；GETRANGE 越界
  回 `$0` 空 bulk。
  批次 7（2026-11-21，编号 #71–#80 再扩 10 例，changelog
  `changelog/2026-11-21/multistore-scenarios-batch7.md`）：
  `lease_ttl_patrol`（SETEX/PSETEX/EXPIRE/PEXPIRE/EXPIREAT/PEXPIREAT 网格、TTL 半进位
  (ms+500)/1000、PERSIST -1、无效期 verbatim）、
  `namespace_rebrand`（RENAME 值+TTL 随迁覆盖、RENAMENX :1/:0、缺源 no such key、
  CROSSSLOT、TYPE 保持）、
  `social_union_feed`（SUNION/SINTER/SDIFF 非存储读 + CROSSSLOT 门、缺键空集、
  SUNIONSTORE 对照、SMISMEMBER 旗标）、
  `coupon_batch_multi_exec`（MULTI +QUEUED、EXEC 有序回放、空队列 *0、EXEC without
  MULTI、嵌套 MULTI、EXECABORT 整批弃、运行期错内联不弃兄弟、全键同 slot）、
  `ticket_redelivery`（裸父 XADD 回执、XGROUP 0-0、XREADGROUP COUNT、XPENDING 汇总/
  range 两视图、XAUTOCLAIM 三段形换主、XACK 清 PEL）、
  `telemetry_ring_buffer`（XADD 内联 trim 先于 id：MAXLEN/~//=/MINID、选项重复后者胜、
  NOMKSTREAM、XLEN=XINFO length）、
  `course_enrollment_join`（MySQL 复合主键、INNER JOIN USING、LEFT JOIN 复合 ON + NULL
  腿、反连接、ORDER BY+LIMIT、GROUP BY COUNT/SUM）、
  `review_analytics_es`（ES sort asc/desc、默认 _score desc、from/size、_count、
  10k 窗口 verbatim 错）、
  `print_spooler_lpop_count`（LPOP/RPOP count 数组形/短取/0/缺键两态、负数与非整数
  verbatim、WRONGTYPE）、
  `auction_standby_bzpop`（BZPOPMIN/MAX 平铺三元组、超时 *-1、BLOCK 0 spawn 唤醒、
  多键同槽跳空落次、非浮点 timeout 错）。
  批次 7 追加钉死：TTL 半进位 `(ms+500)/1000`；XAUTOCLAIM Redis≥7 三段
  `[cursor,entries,deleted]`、XPENDING 汇总 2 消费者 3 行 PEL 为 `*7`；MULTI 事务体
  单 slot、队列期错 EXECABORT vs 运行期错内联；ES 窗口上限 `[10000] but was [10001]`、
  sort 字符串拼法默认 asc；XADD trim 选项先于 id 且重复后者胜；LPOP count=1 仍 `*1`
  数组形；JOIN USING/复合 ON/IS NULL 反连接可用。
  批次 8（2026-11-22，编号 #81–#90 再扩 10 例，changelog
  `changelog/2026-11-22/multistore-scenarios-batch8.md`）：
  `shard_route_xpick`（XPICK 三策略：round_robin 发现序轮转、hash CRC16 稳定命中、
  least_backlog 随积压摆动；回执全形 parent/child；三错误 verbatim）、
  `replay_rewind_consumer`（XINFO CONSUMERS *6 扁平花名册、XGROUP SETID 0-0 回拨重放、
  重放 PEL 行随条目换主、DELCONSUMER 回清除 pending 数、NOGROUP/Invalid stream ID）、
  `session_flag_matrix`（SET GET 回旧值/nil、NX/XX 否决形与 GET 并存、NX+GET 新键不报错、
  KEEPTTL 保 TTL vs 裸 SET 清 -1、NX+XX 与 KEEPTTL+EX 互斥门）、
  `warranty_ttl_fences`（EXPIRE NX/XX/GT/LT 围栏只朝单侧动、:1/:0 诚实回、未知旗标
  错误文案全列表）、
  `score_guard_zadd_flags`（ZADD GT 挡降/LT 挡升、CH 计变更同分不计、INCR 回新分 bulk、
  NX/XX 门、三互斥错误）、
  `grade_curve_case_having`（CASE WHEN 表列分档、GROUP BY 别名+HAVING 滤瘦桶、AVG
  scale-4 十进制、双列 ORDER BY、LIMIT n OFFSET m）、
  `roster_groupconcat`（GROUP_CONCAT 默认/SEPARATOR/DISTINCT 首见序/多参、CONCAT_WS
  跳 NULL、CONCAT NULL 传播、REGEXP 字节级大小写）、
  `lexicon_prune_bylex`（ZREVRANGEBYLEX 倒序全窗/反转界拼写/LIMIT、ZREMRANGEBYLEX
  前缀 `[a (b` 裁剪——与 #54 中窗形不重复）、
  `festival_program_mustnot`（ES bool must_not 裸排/must 并置/filter+_count、空值全集；
  槽位替换：裸 SUNION 已被 #73 盖、EXPIRETIME 未实现，取备选 2 未盖面 must_not）、
  `sqlrpc_twopc`（**首启 `sql_rpc` 前端**：三节点 RESP+MySQL+sql_rpc、cluster init +
  sql_nodes 收敛轮询、39 腿跨槽 INSERT 2PC 提交、三节点 gather 一致、dup 远端否决零落地、
  /sql2pc/status 401/unknown）。
  批次 8 追加钉死：XPICK 回执全形 parent/child、发现序=字节序、least_backlog 比 XLEN；
  SETID 回拨重放且 PEL 行随条目换主（条目级 PEL，Redis 语义）、空史消费者注册行残留；
  SET 否决+GET 仍回旧值；EXPIRE 围栏单侧性；ZADD 同分重加 CH 也计 0、INCR 否决回 null；
  GROUP_CONCAT DISTINCT 首见序、AVG(int) scale-4；ZREVRANGEBYLEX 界拼写随向反转；
  must_not 可与 must/filter 并置；sql_nodes 收敛前含空 "sql_rpc":"" 占位。

  批次 9（2026-11-22，编号 #91–#100 再扩 10 例，changelog
  `changelog/2026-11-22/multistore-scenarios-batch9.md`）：
  `idle_session_seal_xidle`（XIDLE 查询回配置秒/未配置 -1/缺流 -2、`XIDLE s N` 设定
  与 0 清除、两错误 verbatim、1s 到期整个 family 封存——组记录同亡、重开后
  XINFO GROUPS 空）、
  `nightshift_handoff_blmove`（BLMOVE LEFT/RIGHT 对向唤醒回 moved 元素 bulk、双空道
  超时 `$-1`、BRPOP 立即回尾元素/空道超时 `*-1`、RIGHT/LEFT 双向审计序）、
  `cashier_abort_discard`（DISCARD 弃队不落地且同时清 WATCH、无 MULTI 专用文案、
  嵌套 MULTI 弄脏队列 → EXECABORT、UNWATCH 恒 +OK、WATCH 竞态中止对照组）、
  `ops_console_server`（PING/PONG/payload bulk、INFO 三节切片 + `db0:keys=N`、
  COMMAND COUNT/GETKEYS/INFO、CONFIG 桩恒回 cluster-require-full-coverage=no 对、
  SELECT 矩阵、QUIT +OK 后断连）、
  `tags_terms_filter`（ES terms OR 并集/空集零命中、裸 bool.should=OR、match
  operator or/and、terms+range filter 交集）、
  `visual_search_knn`（ES knn 分数 ∈(0,1] 近邻序、filter 限池、_source:false→null
  与字段剪枝、sort _doc=插入序且 _score:null）、
  `tenant_token_sasl`（**首启 `kafka_token` scenario**：pre-auth ApiVersions 22 键含
  17/36、未认证 Produce 静默丢弃断连、错机制 33/错 token 58（消息无 token 片段）均
  +close、PLAIN 认证后单连接 Produce v3 + OffsetCommit/OffsetFetch v0 全通）、
  `border_screening_subquery`（IN (SELECT) 命中、NOT IN 遇 NULL 列表全 UNKNOWN 零行
  ——3 值逻辑钉死、NULL 守卫放行、EXISTS/NOT EXISTS 非相关+相关、标量子查询列表
  空中 → NULL 与 WHERE id=(SELECT MAX(id))）、
  `omnichannel_setops`（UNION 去重、UNION ALL 保重+字面量操作数、INTERSECT [ALL]
  =min 乘数、EXCEPT [ALL]=多重集减法、INTERSECT 优先级紧于 UNION、复合尾
  ORDER BY+LIMIT 翻页全复合体）、
  `loyalty_upsert`（ODKU 插入 1/变更 2/同值 0 三态 affected、VALUES() 读入行算术、
  REPLACE 现存=deleted+1/新键 1、INSERT..SELECT CONCAT 后缀自快照且源行原样）。
  批次 9 追加钉死：XIDLE 家族级到期封存；BLMOVE/BRPOP 超时各 `$-1`/`*-1`；DISCARD
  连带清 WATCH、嵌套 MULTI 后 EXEC 收 EXECABORT；CONFIG 桩任意参同答、SELECT 仅
  0 号库；ES knn 分数上界 1、_doc 排序抑制 _score；SASL 失败消息无 token 片段、
  未认证数据命令静默丢弃；NOT IN NULL 列表零行、INTERSECT ALL/EXCEPT ALL 乘数语义、
  标量子查询空→NULL；ODKU affected 1/2/0 与 REPLACE=删除数+1。
  批次 10（2026-11-22，编号 #101–#110 再扩 10 例，changelog
  `changelog/2026-11-22/multistore-scenarios-batch10.md`）：
  `factory_reset_flushdb`（FLUSHDB 五家族 + stream 根整体擦除：DBSIZE 0/每族读空/
  RANDOMKEY nil、组记忆随擦除回收——重建流 XINFO GROUPS 裸 `*0`、junk 参数
  `-ERR syntax error` 不触发、`ASYNC`/`SYNC` 直通）、
  `sales_report_cte`（CTE 别名自连接、带别名表的链式引用、体内子查询、CTE ∘
  UNION ALL 保重 vs UNION 去重、`WITH RECURSIVE` 1235 点名 recursive）、
  `invoice_formatting_funcs`（字符串族 + 控制四件 + CAST 在列数据上 crib：LPAD `003`、
  NULLIF 精确匹配→NULL、DECIMAL(6,2) 保 `1250.00`、CONCAT_WS 跳过 NULL 而
  COALESCE 版留占位、CONCAT NULL 传播）、
  `shift_schedule_datetime`（DATE_ADD DAY/MONTH 钳闰年 02-29/HOUR 升格 DATETIME/
  MINUTE、DATEDIFF 正反 `1`/`-1`、DATE_FORMAT 列上 `%H:%i:%s` + `%W`/`%M`/`%j`
  定值 crib）、
  `leaderboard_trim_dml_limit`（UPDATE/DELETE `ORDER BY … LIMIT` 字面拼写：前二
  +100、先筛后切、尾二出队、`LIMIT 0` 零 affected）、
  `schema_audit_show`（SHOW TABLES/DATABASES、DESCRIBE 6 列 PRI/MUL/UNI、SHOW INDEX
  三行 Non_unique 0/1/0、EXPLAIN SeqScan vs IndexScan `-> 2 pks`、SHOW CREATE 渲染
  DROP→重建逐字节往返、ALTER RENAME TO 后旧名 1146）、
  `draft_editor_rollback`（`START TRANSACTION` 作者私有舞台、ROLLBACK 全弃/COMMIT
  全落、跨连接可重复读快照至自身 COMMIT、事务内 DDL 四形 1235 事务不废）、
  `donation_audit_aggdistinct`（SUM 350 vs SUM(DISTINCT) 150、字符串 MIN/MAX 字节序
  `ali_ce`<`alice`/`DON%24A`<`DON24C`、LIKE `DON\%%` 3 行 vs `DON%` 4 行、
  `ali\_%` 排除 alice、空窗 SUM NULL）、
  `index_lifecycle_cat_bulk`（空节点 `_cat/indices` 仅表头、`_bulk` index 201
  created、create 撞现存 item 400（真 ES 409，已知偏差）+
  `version_conflict_engine_exception` 翻
  errors:true、delete 命中 deleted/未命中 404 不翻、refresh 后 docs.count 1）、
  `consumer_fleet_metadata`（XADD `*2` 种子 + Produce v3 base=1、Metadata v1
  Some(1)/None、ListOffsets v5 latest=2/earliest=0、JoinGroup 79→gen1 自任
  leader、SyncGroup `[fleetp0]`、DescribeGroups Stable、OffsetCommit v2 /
  OffsetFetch v0 回 2）。
  批次 10 追加钉死：FLUSHDB 组记忆随擦除回收、junk 不触发、ASYNC/SYNC 直通；CTE
  别名链/体内子查询/与 UNION [ALL] 复合、RECURSIVE 1235；CONCAT NULL 传播、
  CONCAT_WS 跳过 NULL；DATE_ADD 月钳/HOUR 升格、DATEDIFF 日期部分；`ORDER BY …
  LIMIT` 字面拼写 aff=n、LIMIT 0 零；DESCRIBE 恒 6 列、SHOW CREATE 可往返、EXPLAIN
  `IndexScan <ix> -> <n> pks`；`START TRANSACTION`=BEGIN、快照至自身 COMMIT、事务内
  DDL 1235；SUM(DISTINCT) 折叠、LIKE `\%`/`\_` 字面通配；`_bulk` create 冲突 item
  400 + error.type、delete 未命中不翻 errors；Metadata None/ListOffsets epoch -1 哨兵/
  JoinGroup 79 取 broker id。
  批次 11（2026-11-24，编号 #111–#130 再扩 20 例，changelog
  `changelog/2026-11-24/multistore-scenarios-batch11.md`；本批新增
  `scenario_common::perf`（now/within/at_least/rate/scenario）与 `s3_extra`
  （带请求头的 S3 往返），每例钉死性能预算并以 `perf[...]` 日志为证据）：
  `video_streaming_range`（S3 3×4KiB 分片 + 20 次 Range GET 字节精确/Content-Range、
  10 HEAD、3 LIST 折 CommonPrefix、416 InvalidRange；SQL 登记 = RESP HSET 缓存）、
  `telecom_cdr_batch_spool`（/produce_batch 50 + /range 回放 + 消费 10/ack_batch +
  XLEN 互查 + 700ms 延迟唤醒长轮询 + 空 park 满 wait_ms + SQL 台账）、
  `event_platform_topic_admin`（CreateTopics 3→5、重复 36、DescribeConfigs 4 项、
  DeleteTopics 后 Metadata 空、XINFO TOPICS events 5 个 p0..p4）、
  `consumer_fleet_churn`（JoinGroup 79/gen0→gen1、SyncGroup/Heartbeat/OffsetCommit v2/
  ListGroups Stable、已提交流 XTRIM 被守卫拒、组删后 MAXLEN 0 回 3、10 组 churn）、
  `kafka_autocreate_registry`（Produce 打未知 auto126 自动建题、XLEN auto126/p0==2、
  重复 CreateTopics 36、10 轮往返）、
  `security_log_drill`（一次 _bulk 100 条、query_string/match_phrase/prefix/_source
  includes+excludes、三方计数一致、HEAD→DELETE→HEAD 404）、
  `product_search_ab`（FT.CREATE ON HASH/TAG 被拒 + 真实 FT.ADD JSON 面、命中 doc id
  作 HASH 键 HGETALL 读回逐字段 = FT 文档、SORTBY/RETURN/RETURN 0/HIGHLIGHT/
  WITHSCORES、ES term + SQL WHERE 同判）、
  `bank_reconciliation_fullouter`（FULL OUTER JOIN 7 行、USING 双 id、反连接、
  INTERSECT/EXCEPT、歧义 ORDER BY 1054、S3 报告往返、200 join+3 set op <2s（≥50 stmt/s））、
  `visual_sql_ann`（VECTOR(4) 三度量 KNN、向量函数往返、vector index = SHOW INDEX
  BTREE 行、ES knn 同 top-5）、
  `iot_columnar_analytics`（1000 行 columnar 分块插入、跨 segment 聚合、append-only
  1235、SHOW CREATE TABLE、TRUNCATE、S3 往返）、
  `saas_dual_engine_turso`（rocks + turso 双引擎同节点、SHOW DATABASES 合并、
  AUTO_INCREMENT/JOIN/UNION/CTE、T3 allowlist 6 形 1235、USE 未知库 1049）、
  `order_history_prepared_pagination`（prepared LIMIT/OFFSET 两页、LIMIT 0、负 LIMIT 与
  LIMIT ?,? 1064、20 次重执行、DEALLOCATE 后 stale handle 绝不答原页、ES from/size 互查）、
  `hotel_row_lock_hold`（FOR UPDATE 锁持 1205、异行可锁、COMMIT 释放、FOR SHARE 共存、
  NOWAIT/SKIP LOCKED 1235、10 轮 <2s）、
  `regulatory_typed_report`（BIGINT/BOOLEAN/BLOB/DATE/DATETIME/DECIMAL 往返、DESCRIBE
  类型名、宽松日期规范化、1292、BOOLEAN 比较 1235、会话面定形、ES 镜像）、
  `telemetry_shard_topics_lite`（telemetry q0/q1/q2 = 5/3/2、XINFO TOPICS 字节序扁平对、
  XINFO STREAM、组排空+XACK、XINFO LITE messages/acks 计数、SQL 分片登记）、
  `data_lake_export_lifecycle`（自建 lake125 桶全生命周期：Create 200/409、25 对象、
  ListObjectsV2 10/10/5 分页 + CommonPrefixes、非空删桶 409、25 删 204、删桶 204、
  HEAD 404）、
  `lite_gc_redelivery`（300ms 双 sweep：idle 重投 deliveries 1→≥2、MAXDELIVERY 2 +
  DLQ 死信、XACK 清兄弟、空 PEL 消费者 GC、SQL 对账）、
  `sql_dr_checkpoint`（20 SQL 行 + 10 RESP 键、等晚于写入的 checkpoint、SIGKILL +
  擦 KV 保 raft/ + 拷回、重启后行值/HGETALL/meta.json 全在）、
  `kafka_offset_epoch`（XADD 10 → ListOffsets v5 10/0、OffsetForLeaderEpoch v2
  (0,-1,10)/(3,-1,-1)、OffsetCommit/Fetch v0 从 7 尾批、SQL 水位表）、
  `cluster_migrate_list`（真两节点 + cluster init、100 键单槽、手开 IMPORTING 证
  ASKING 一次性、migrate task +OK、migrate list JSON done/moved 100、属主切换、
  leader MySQL 审计行）。
  批次 11 追加钉死：S3 Range/HEAD/List 分页 + 416；Kafka topic admin 全生命周期 +
  auto-create + 79/gen0；Lite 双 sweep 重投/死信/GC + XINFO LITE；SQL FULL OUTER/
  USING/反连接/歧义列 1054 + vector index BTREE + columnar 1235 + prepared 翻页/
  DEALLOCATE stale handle + 1205 行锁 + 类型化 DDL + 双引擎合并；FT 真实语法面 +
  RETURN 0/HIGHLIGHT/WITHSCORES；migrate task/list JSON + ASKING 一次性。
  批次 12（2026-11-25，编号 #131–#145 再扩 15 例，changelog
  `changelog/2026-11-25/multistore-scenarios-batch12.md`；选题来自 130 例覆盖地图的
  已实现未覆盖缺口审计，每例同样钉死性能预算）：
  `ops_dashboard_cluster`（真两节点 `CLUSTER INFO/NODES/SLOTS/KEYSLOT` + `RAFT stats/
  leader/nodes` 面板互查、epoch = term++commit_index、`-MOVED` 由 NODES/SLOTS 预测并
  双向验证、`raft get sql_nodes` 四元登记、leader MySQL 快照行复制到 follower）、
  `auction_ladder_zrange`（统一 ZRANGE 10 对组合与旧命令字节相等、11 条统一错误、
  BYLEX REV 不交换边界、XADD 尾条 = ZREVRANGE 0 0、SQL ORDER BY LIMIT OFFSET 窗口）、
  `coupon_exat_window`（SET EXAT/PXAT 未来/过去、KEEPTTL 与 EX 互斥、TTL/PTTL 换算、
  SQL 账本 `expires_at` 集合 = RESP 存活集、XADD DELAY 提醒、三处 at_least）、
  `ordered_ticket_queue`（XGROUP ORDERED INFLIGHT 配给、XINFO GROUPS/STREAM FULL 形状
  与 COUNT 截断、DESTROY :1/:0 → NOGROUP、审计表 = PEL 行数）、
  `profile_merge_vector_attrs`（JSON.FORGET 别名与静态路径门、VADD VALUES/FP16、VSIM
  WITHATTRIBS 布局与 Rust 独立排名、FT.SEARCH FP16 = VSIM、FT.BUILD 质心、SQL 计数）、
  `attendance_clock_funcs`（Hinnant 历元 oracle 对 NOW/CURDATE/CURTIME/YEAR../DATE_SUB/
  UNIX_TIMESTAMP/FROM_UNIXTIME/DATE_FORMAT/DATEDIFF、TIMESTAMPDIFF 1054、RESP TTL 互查）、
  `partner_export_funcs`（数值 24 格 + 字符串 28 格 + 位运算 u64 + 21 个 1235、
  CSV → S3 PUT/GET/HEAD ETag = md5、SQL COUNT = RESP HLEN）、
  `bi_session_handshake`（SET NAMES 静态回显、autocommit=0 1235、隔离级连字符回显、
  @@version/autocommit/max_allowed_packet、USER()/CONNECTION_ID()、USE/DATABASE()
  turso_ 前缀、RESP 哈希 = SQL）、
  `warehouse_schema_migration`（RIGHT = 交换侧 LEFT、CROSS JOIN 笛卡尔、逗号 join
  1235、RENAME 保 PK、DROP INDEX 让 EXPLAIN IndexScan → SeqScan、DROP DATABASE 1008/
  USE 1049）、
  `starrocks_vendor_contract`（DUPLICATE/PRIMARY KEY + DISTRIBUTED BY HASH 解析、7 条
  1235、SHOW CREATE 往返、INSERT…SELECT 去重/append-only、ES 镜像命中 = SQL 计数）、
  `log_index_rollover`（ES root/health/GET index/自动 id/doc GET-HEAD/GET 动词
  _search|_count|_refresh/405/404 信封、SQL rollover_log SUM = _count/命中）、
  `customer_upsert_bulk`（op_type=create 201/409、定域 _bulk index/update/delete 逐项
  状态、minimum_should_match 数值/百分比 + 400、RESP 哈希 + SQL billing 三面一致）、
  `fleet_coordinator_onboarding`（ApiVersions v0–v3/v4 35、Metadata v1、FindCoordinator
  v0/v1 两类 key、Produce tombstone → 流 `__null__`、ListOffsets/Fetch、SQL alive 集合）、
  `lake_listing_audit`（ListBuckets、v1 marker/NextMarker 三页、v2 start-after/
  continuation-token、encoding-type=url、max-keys 0/-1/abc/5000、InvalidBucketName/
  NoSuchBucket/NoSuchKey、SQL manifest = 列表并集）、
  `observability_drill`（/metrics 直方图精确计 INCR、raft /get、/sql/ts 纯文本租约、
  /sql/nodes、/join //depart 错 token 401 且登记不变、RocksMQ 404/405/400）。
  批次 12 追加钉死：CLUSTER/RAFT 面板全集 + `-MOVED` 预测；统一 ZRANGE 矩阵；SET
  EXAT/PXAT/KEEPTTL；XGROUP ORDERED INFLIGHT + XINFO STREAM FULL；JSON.FORGET/VSIM
  WITHATTRIBS/FT FP16；SQL 时钟函数族 + 数值/字符串/位运算函数族；会话握手 1235/回显
  矩阵；RIGHT/CROSS JOIN + RENAME/DROP INDEX/DROP DATABASE；StarRocks 方言子集；ES
  REST 生命周期 + op_type/_bulk/msm；Kafka ApiVersions/FindCoordinator/tombstone；S3
  ListBuckets/v1 marker；运维面 /metrics + raft HTTP + RocksMQ 错误合同。
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
- **turso 引擎（2026-11-09 turso-engine，首次入台账；计划
  `plans/2026-11-09-turso-engine/`，摘要 `changelog/2026-11-09/turso-engine.md`，
  契约/偏差台账 `COMPAT.sql.md` "turso engine" 节）**——八套件均为真进程 +
  yaml 注入 `turso_engine`/`turso_db_dir`（临时目录），双引擎偏差在引擎侧断言：
  （2026-11-10 allowlist batch 1 增 `turso_setops_e2e`、同日验收收口增
  `turso_sqlite_interop_e2e`，见下。）
  - `turso_smoke_e2e`（2）：T1 路由骨架——`turso_` 库上常量算术由嵌入式引擎应答、
    引擎绑定未实现语句统一 1235、默认（rocks）库零行为变化；独立节点用例：
    未开引擎节点 `USE turso_x` 仍 1049。
  - `turso_catalog_e2e`（1）：T2 库对象面——CREATE/DROP DATABASE（1007/1008/IF
    形状 + raft 存在性条目 + 库文件）、SHOW DATABASES 合并（rdb ∪ `*.db`）、
    sqlite_master 三件 SHOW（SHOW TABLES/DESCRIBE/SHOW CREATE TABLE）、最小
    INSERT/SELECT 往返、文件权限位决定的只读重开（写 → 1290）。
  - `turso_crud_e2e`（1）：T3 CRUD 干线——类型化建表（pk AUTO_INCREMENT/NOT
    NULL/内联二级索引）、多行 INSERT（引擎侧 id 分配 + last-insert id）、SELECT
    干线（WHERE/ORDER/LIMIT 含 `LIMIT o,n` 交换/GROUP BY 聚合）、UPDATE/DELETE
    affected 检查、TRUNCATE/DROP、allowlist 外 1235 拒绝矩阵。
  - `turso_parity_e2e`（1）：同 schema 同语句双引擎（`USE rdb` vs `USE turso_parity`）
    对照——一致面全等 + 五条台账偏差引擎侧断言（DECIMAL→REAL 渲染、AVG 格式、
    no-op UPDATE affected、多行 INSERT last-insert id、TRUNCATE 自增水位）。
  - `turso_vector_e2e`（1）：T4 向量面——VECTOR(N) DDL/DESCRIBE、应用侧维度校验
    写路径、STRING_TO_VECTOR/VECTOR_TO_STRING/VECTOR_DIM/DISTANCE 函数族、精确
    KNN（ORDER BY DISTANCE … LIMIT）、1292/1582 错误矩阵、CREATE VECTOR INDEX
    1235、与 rocks 同数据同查询的 id 序 parity（数值断言 epsilon=1e-6，f32 vs
    f64 纪律）。
  - `turso_txn_e2e`（1）：T5 事务/预编译/会话边角——字符串直通 BEGIN/COMMIT/
    ROLLBACK（含 MySQL BEGIN 隐式提交语义）、txn 外 no-op COMMIT/ROLLBACK、
    prepared INSERT/SELECT `?` 绑定、事务内 USE 换库 1235、双连接脏读检查
    （WAL 式快照无脏读）、隔离级别回显（会话记录，引擎忽略）。
  - `turso_setops_e2e`（1）：allowlist batch 1（2026-11-10）——JOIN 全矩阵
    （INNER/裸 JOIN/LEFT/RIGHT/FULL OUTER/CROSS/USING×各 join 类、别名、限定列、
    链式 join）、子查询（[NOT] IN/[NOT] EXISTS/标量/投影相关标量/内层 JOIN）、
    集合运算（UNION vs UNION ALL 去重、INTERSECT/EXCEPT、外层 ORDER/LIMIT、
    括号操作数定形、无优先级混合链、首分支列名）、CTE（含列别名/链式/join 体）；
    引擎侧断言两条台账：(z1) USING 下 `SELECT *` 列数合并、(z2) INTERSECT/
    EXCEPT ALL turso 1235（rocks 多重集语义照答），另断言派生表维持 1235。
  - `turso_sqlite_interop_e2e`（2）：验收收口（2026-11-10）——双向文件级互操作：
    MySQL 写 → stock sqlite3 CLI 读（存活期外部读被拒 SQLITE_BUSY——turso WAL
    写者独占、无 -shm；优雅退出后 CLI 恢复 WAL 全量可读，`hex()` 钉死裸 LE
    f32 向量布局）；反向 stock CLI 预建 `.db` 经 `CREATE DATABASE IF NOT
    EXISTS` 收养、MySQL 侧 `VECTOR_TO_STRING`/`VECTOR_DIM` 读回（偏差 (z3)；
    宿主 sqlite3 3.37.2 为 e2e 依赖，缺失则跳过）。
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
