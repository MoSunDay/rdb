# 多存储场景批次 2：#11–#20 十例 e2e（2026-11-20）

承接 2026-11-19 首批 10 例（`2026-11-19/` 目录），批次 2 在同一夹具
（`tests/scenario_common/`，只读复用）上新增十个互不重复的多平面横切场景，
编号 #11–#20。`tests/` 净增 10 个文件、全部 ≤200 行（上限 400），不改
`src/`、不改公共夹具。

## 场景矩阵

| # | 文件 | 前端 | 故事线要点 |
|---|------|------|-----------|
| 11 | `scenario_payment_ledger_e2e.rs` | RESP+MySQL+DELAY | INCRBY 钱包(500/300/150)→SQL 双分录 UNIQUE(entry_no)、重放 COMMIT 拒且 COUNT=4→XGROUP 消费+XACK 二读空→ZSET 收款榜 bob(120)>alice(50)→DELAY 400 对账 job（XLEN ":3" 轮询+新 id XACK）→SUM(D)=SUM(C)=榜=170、转账守恒 950 |
| 12 | `scenario_flash_sale_e2e.rs` | RESP+MySQL | INCRBY 库存 50→47；SET NX PX 限购锁二次 `$-1`；UNIQUE(order_no) 重放拒；ZINCRBY/ZSCORE 销量榜；LPUSH+LRANGE 中奖名单；XADD flash/events XLEN=3 |
| 13 | `scenario_chat_offline_e2e.rs` | RESP+MySQL+ES | SET NX PX 会话锁；HSET/HGETALL 画像；XGROUP 逐条 COUNT 1：在线直 XACK、离线 LPUSH 落箱；GETDEL 首读 bulk 再读 nil；ES term 大小写敏感（u1 2 命中、U1 0 命中） |
| 14 | `scenario_config_center_e2e.rs` | RESP+MySQL+S3 | JSON.SET/GET/NUMINCRBY `.rev` 7→9；INCR epoch；FT.CREATE/FT.ADD/FT.SEARCH 三服务；S3 快照（非 UTF8 字节）回读逐字节相等；SET PX→EXPIRE(:1)→PERSIST(:1)→TTL -1→EXISTS |
| 15 | `scenario_metrics_rollup_e2e.rs` | RESP+MySQL | INCR 分桶；SETBIT 重复置位回 :1 且 BITCOUNT=UV 不虚增、GETBIT 抽查；ZINCRBY+ZREVRANGE 热点序；XADD+XRANGE(stream_ids) 重放计数；SQL rollup 三面对账同值 6 |
| 16 | `scenario_inventory_e2e.rs` | RESP+MySQL+RocksMQ | HSET/HINCRBY 出入库；SADD 仓库集(:3/:0)+SISMEMBER+SMOVE（`{inv}` hash-tag 同槽）；ZADD 缺口榜+补货后 ZREMRANGEBYSCORE :2 清队；SQL movements（含初始入库）SUM=HGET qty；RocksMQ /produce→/consume b64d→/ack→pending 0 |
| 17 | `scenario_ticket_seats_e2e.rs` | RESP+MySQL+DELAY | SADD 占座(:3/:0)；SET NX PX 座位租约；UNIQUE(seat) 对手单 COMMIT 拒且 COUNT 守恒；RPUSH/LPOP 候补 FIFO；DELAY 400 超时释放 job（XLEN ":1" 轮询+新 id）后 SREM holds :1 |
| 18 | `scenario_push_news_e2e.rs` | RESP+MySQL+ES+Kafka | XADD `news/q0` 种子→Produce v3 带 headers 三条（base_offset=1）→XGROUP 逐条扇出：HSET 画像/ZINCRBY 水位/SQL 台账/ES 文档→XACK；ZREVRANGE u1(2)>u2(1)；ES term 验证 |
| 19 | `scenario_game_season_e2e.rs` | RESP+MySQL | ZINCRBY 天梯 150/90/45 + ZREVRANGE 冠军序；JSON 档案 NUMINCRBY wins 13；VADD×2+VSIM 首位召回 p1；EXPIRE buff :1+PERSIST :1+TTL -1；XADD season/mvp；SQL 结算 SUM=285=ZSCORE 和 |
| 20 | `scenario_audit_archive_e2e.rs` | RESP+MySQL+ES+S3 | 四前端单节点齐聚：SQL 审计 6 行（zed3/amy2/bo1）；XGROUP 消费 6 条+XACK+二读空；ES term（actor/action/doc 三查询）；S3 归档字节回读；ZREVRANGE 工作量榜与 SQL 计数一致 |

## 本批钉死的新语义（写前验证 + 实测确认）

- **命令注册**：`srem`/`ttl`/`smembers`/`smove`/`getdel`/`json.numincrby`/
  `setbit`/`getbit`/`bitcount`/`rpush`/`lpop`/`zremrangebyscore`/`vadd`/
  `vsim`/`ft.*` 全部在册（grep `src/command/mod.rs` 逐条确认后才写断言）。
- **CROSSSLOT**：SMOVE 等多键 Set 运算两键必须 hash-tag 同槽
  （本批用 `{inv}wh:west`/`{inv}wh:east`）；单节点同样强制。
- **SQL**：无 PRIMARY KEY 建表直接 `1235 need a PRIMARY KEY` 拒绝（movement
  台账补 id 列）；`WHERE ... IN (...) ORDER BY` 的返回序以 IN 列表序为准，
  不是字典序。
- **HSET 返回值**：只数**新**字段——对已存在字段再 HSET 回 0（#18 画像断言
  由此从 `==1` 改为 `<=1`）。
- **Kafka topic→stream**：topic `t` 即流 `t/q0`（iot 模板复认）；Produce v3
  落流字段名为 `k`/`v`/`h`，header 名以 hex 编码（`lang`→`6c616e67`）。
- **DELAY**：复用批次 1 语义——staged 行 XLEN 不可见、交换铸新 id（#11/#17
  显式 `assert_ne!` staged 收据与投递 id）。

## 回归与证据

- 单跑：10 个新目标逐一 `cargo test --test scenario_<name>_e2e` 全绿
  （期间两次 `resp not accepting in 30s` 环境抖动，静默期重跑即过，未改代码）。
- 全量：`/tmp/chunk.sh` 分片跑 131 个目标（129 个 tests/*.rs + lib/main
  unittests），`--test-threads=4`，全部 rc=0。
- clippy：`cargo clippy --tests` 对新文件零告警。
- 文档同步：`features/e2e-coverage.md` 套件小节 10→20 例（含批次 2 十条
  一行式条目与追加钉死语义）、`agents/rust/index.md` 场景计数 10→20。
- 未提交：批次 1 遗留（10 测试 + `tests/scenario_common/` + changelog 目录）
  与本批（10 测试 + changelog）均未入库，加 2 个已修改文档
  （`features/e2e-coverage.md`、`agents/rust/index.md`），`git status` 共 25 个
  路径留待用户决定提交。
