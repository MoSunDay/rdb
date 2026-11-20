# 多存储场景批次 3：#21–#30 十例 e2e（2026-11-20）

承接批次 1（2026-11-19 首批）/批次 2（同日 `multistore-scenarios-batch2.md`），
批次 3 继续在同一夹具（`tests/scenario_common/`，只读复用）上新增十个互不
重复的多平面横切场景，编号 #21–#30，其中 #21 首次把 sql_rpc 3 节点集群
带进场景套件。`tests/` 净增 10 个文件、全部 ≤250 行（上限 400），不改
`src/`、不改公共夹具。

## 场景矩阵

| # | 文件 | 前端 | 故事线要点 |
|---|------|------|-----------|
| 21 | `scenario_bank_2pc_cluster_e2e.rs` | MySQL×3+sql_rpc+raft | `start_sql_cluster(3)`：开户批 24 行跨槽 2PC 提交、三节点 COUNT/SUM 全表读一致；转账批 UNIQUE(txid) 三行提交 SUM=830；重放 T-0001 远端 dup 否决（`dup:`/Duplicate）全局消失、三节点 COUNT=3 守恒；/sql2pc/status 对新 id `unknown\n`、ts 游标步进探测到 `committed` |
| 22 | `scenario_hotel_calendar_e2e.rs` | RESP+MySQL | SETBIT 入住日历（重复置位 :1 不虚增）+BITCOUNT；ZRANGEBYSCORE 800..1000 价窗两档；SINTER `{amen}:R1`×`{amen}:R3` 设施交集；UNIQUE(night `R3-10`) 重订 BEGIN 内 stage、COMMIT 1062 拒且日历守恒 |
| 23 | `scenario_exchange_orderbook_e2e.rs` | RESP+MySQL | 双 ZSET 买卖簿（bid ZREVRANGE/ask ZRANGE 最优价）；限价单 ZADD 歇簿；撮合价差命中 ZREM 双撤 :2；SQL trades 两条落账；INCRBY mkt:vol 与 HINCRBY ticker last/buy/sell；XADD tape/matches 成交带 |
| 24 | `scenario_subscription_renewal_e2e.rs` | RESP+MySQL+DELAY | 三用户 DELAY 400 续费波 XLEN 不可见→":3" 交换；XREADGROUP COUNT 1 逐条：active INCRBY +30 天+UNIQUE(user_cycle) 账单、churned u3 SMOVE `{subs}:active`→`{subs}:churn`；账单重放 COMMIT 拒；SCARD/SCARD/SUM(198) 三面收口 |
| 25 | `scenario_election_ballots_e2e.rs` | RESP+MySQL+ES | 投票落 SQL（UNIQUE(ballot_key) `vN-mayor`）；HINCRBY tally:mayor 运行计票（回新值须对 running 计数）；重投 BEGIN..COMMIT 1062 拒且 tally 不动；SQL GROUP BY=Redis HGET=3/3/1；ES `_doc/{ballot_key}` 幂等 PUT 镜像 + term region=east 计数=SQL COUNT |
| 26 | `scenario_courier_dispatch_e2e.rs` | RESP+MySQL+RocksMQ | /produce 五包裹→desk 组 /consume b64d 排水；每件 SET NX PX `claim:` 认领+按区派单 INSERT+逐条 /ack pending 0；二派单员 beta 重放消费全量认领失败（`$-1` 计数=条数）；UNIQUE(order_key) 二次 INSERT 拒；TTL≤60 且 owner=desk 审计 |
| 27 | `scenario_insurance_claims_e2e.rs` | RESP+MySQL+S3 | 三报案：S3 证据 PUT/GET 非 UTF8 字节级回读+`prefix=insure/evidence/` 列表；UNIQUE(claim_no) 重复报案 COMMIT 拒；核赔 gate=证据 GET 200（CLM-002 单据指向错 key 永远 filed）；INCRBY payout:total=SQL SUM(approved)=2000 |
| 28 | `scenario_clinic_slots_e2e.rs` | RESP+MySQL+DELAY | 三预约各带 DELAY 400 提醒 job（XLEN 不可见→":3" 交换后建组）；p4 双订同槽 COMMIT 1062 拒且无提醒；p2 取消 UPDATE 后投递按 SQL status 分支：confirmed→reminded+短信计数、cancelled 静默；XPENDING 汇总 `*3 :0 $-1 $-1` 清零 |
| 29 | `scenario_flight_ground_e2e.rs` | RESP+MySQL | ZADD 时刻表 :3 + ZRANGEBYSCORE 晨窗 0..60 两腿；WHERE 守卫 UPDATE（尾号未落地 `status='awaiting_tail'` 前零行生效）轮转 FL201→FL202；ZREM :3 清板+ZCARD 0；IN(1,2) 多行重订 UPDATE+IN(...) COUNT 对账 |
| 30 | `scenario_smartmeter_grid_e2e.rs` | RESP+MySQL+Kafka+S3 | XADD `meter/q0` 种子→Produce v3 三条带 region headers（base_offset=1）；XRANGE 载荷+`726567696f6e` hex 头互证；INCRBY 分表 m1=12/m2=4；last_batch 守卫 UPDATE 仅静默 m3 翻 outage；SQL GROUP BY region=12/4；S3 `grid/batch-2.lines` 字节归档+前缀列表 |

## 本批钉死的新语义（写前验证 + 实测确认）

- **XREADGROUP 参数**：`GROUP <组名> <consumer>`——把流名当组名传必得
  `-NOGROUP No such key '<stream>' or consumer group '<stream>'`（#28 排障
  时用最小探针复现，错误文案里 group 位就是流名）。
- **XPENDING 汇总**：`XPENDING <s> <g>` 回 `*3\r\n:0\r\n$-1\r\n$-1\r\n`
  形数组（count/min/max），非单整数，不能过 `int_of`。
- **ZADD/ZREM 多成员**：回 `:N`（新增/移除计数），不是 `+OK`。
- **HGET 计数断言**：回 bulk（`rb(...).parse::<i64>()`），`int_of` 只吃 `:N`。
- **SQL 唯一性只认 UNIQUE KEY 子句**：`PRIMARY KEY` 列上的重复值**不裁重**
  （#25 首版 ballot_key 当 PK 时 dup INSERT 连 COMMIT 都放行）；对账走
  `id BIGINT PRIMARY KEY + UNIQUE KEY uk_x(...)` 模式；dup 判决仍在 COMMIT
  边界（autocommit 单条 INSERT 亦不报错，须显式 BEGIN..COMMIT 才见 1062）。
- **DELAY 交换与组**：交换只动流元数据+条目，组记录独立存储——交换前或后
  `XGROUP CREATE` 均可正常投递（#28 实测两种时序皆绿；批次 1/2 的「交换前
  建组」结论不受影响）。
- **Kafka header 名 hex**：`region`→`726567696f6e` 落 `h` 字段（批次 2
  `lang`→`6c616e67` 结论的再确认）。

## 回归与证据

- 单跑：10 个新目标逐一 `cargo test --test scenario_<name>_e2e` 全绿
  （期间多次 `resp not accepting in 30s` I/O 风暴抖动，静默重跑即过；
  #24/#27/#28 排障轮次中的失败均为代码/断言修正后转绿）。
- 全量：`/tmp/chunk2.sh`（400s 预算版）分片跑 141 个目标（139 个 tests/*.rs
  + lib/main unittests），`--test-threads=4`；首轮 `kafka_fetch_e2e`/
  `process_sigterm_e2e` 两例负载抖动失败，单跑复验均绿，其余 139 例 rc=0。
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` 零告警
  （仅依赖 proc-macro-error2 future-incompat 提示，非本仓代码）。
- 文档同步：`features/e2e-coverage.md` 套件小节 20→30 例（批次 3 十条
  一行式条目 + 追加钉死语义）、`agents/rust/index.md` 场景计数 20→30、
  本 changelog。
