# 多存储真实场景 e2e 套件（scenario_*，10 例）

日期：2026-11-19 · 提交：随本批（b7378c0 之后）

## 动机

既有 e2e 按命令族/前端分片验证「单面正确」；缺一层**真实业务场景**证明：同一份数据
在多个存储面（KV 家族 / JSON / Vector / FT / lite-MQ / SQL / RocksMQ-HTTP / ES /
Kafka / S3）之间的可见性与一致性，且全部跑在真实进程（`CARGO_BIN_EXE_rdb`）上。

## 落地

- `tests/scenario_common/mod.rs`：`FrontSpec`/`front_world`（单节点多前端装配：
  mysql/sql_rpc/es/kafka/rocksmq/s3/lite 开关）、`r/rt/rf/rb`（单行/全文/首 bulk
  回复）、`ok`/`int_of`/`bulk_at`/`stream_ids`/`until_contains`、`http`/`mq_post`/
  `json_of`/`s3_call`。
- `tests/scenario_common/kafka.rs`：手搓 Kafka Produce v3（null transactional_id）。
- `tests/scenario_common/codec.rs`：base64 解码（RocksMQ /consume 载荷镜像）。
- 10 个场景（每文件 ≤202 行）：
  1. `order_pipeline`：Hash 购物车 → SQL 订单（内联 UNIQUE KEY，dup 冲突在 COMMIT 判）
     → ZINCRBY 销量榜 → stream 事件 → XREADGROUP/XACK。
  2. `session_profile`：String+EX/PX 会话、Hash 画像、JSON 偏好、Set 在线、List 登录
     轨迹 LTRIM、过期判决、kill -9 六家族存活矩阵。
  3. `social_feed`：SADD 关注图 + SINTER（`{sf}` hash-tag 键规避 CROSSSLOT）、LPUSH
     收件箱 LTRIM、ZINCRBY/ZREVRANGE 热榜、FT.CREATE/ADD + 中文 title/body 双查询。
  4. `reco_vector`：VADD/VCARD/VSIM 召回排序 + Hash 元数据 + SQL 目录行对账。
  5. `log_pipeline`：RocksMQ /produce → /consume（base64 body）→ ERROR 行转 ES 文档
     （先显式 PUT 建索引；term 查询规避 match 的 lowercase 分析）→ SQL 台账 SUM 对账
     → RESP 侧 XLEN 互查同一条流。
  6. `delay_jobs`：XADD `DELAY` 暂存（sweep 交换后 id 作废，从投递回执取新 id）、
     SET NX PX 租约防抢、XGROUP `MAXDELIVERY 2`：`>` 一次 + XCLAIM 第二次仍投 + 第三次
     进 `<stream>/dlq`（`__dlq_group/__dlq_src`）、SQL 台账状态对账。
  7. `risk_control`：INCR 限窗 + Set 黑名单 + SETBIT 设备指纹 + ZADD/ZREMRANGEBYSCORE
     滑窗 + SQL 审计行数/理由拆分对账。
  8. `iot_telemetry`：Kafka Produce v3 上行（topic 预种子 `topic/q0`）→ JSON.SET 设备
     影子 → HGET 最新读数 → 阈值告警。
  9. `media_archive`：S3 PUT/GET/LIST（bearer token）+ RESP 签名键 + kill -9 存活。
  10. `multistore_failover`：3 节点 `start_sql_cluster`；运行时探测 leader 持有的
      hash-tag（kill 后其槽键稳定 -MOVED）与 raft 复制语义；kill -9 后：SQL 2PC 死带
      显式拒绝/活带照常提交、新 leader 续写 lite 流、respawn 后 SQL 行数追平、
      存活者持续可读双流。

## 过程中钉死的语义（写测试时的真实行为，防后人踩坑）

- 回复类型：HSET/SADD/LPUSH/RPUSH/ZADD/SETBIT/EXPIRE/SCARD/XACK → `:N`；
  GET/HGET/JSON.GET/ZSCORE/ZINCRBY/XADD → bulk；SET/LTRIM/JSON.SET/XGROUP CREATE/
  FT.CREATE → `+OK`。`rb()` 兜 `$-1`→""。
- 数组回复（FT.SEARCH/VSIM/XREADGROUP/XRANGE/HGETALL）必须用静默间隙读全
  （`rf`/`cmd_full_reply`），one-shot 只读首行。
- XGROUP CREATE 起点必须完整 `0-0`（裸 `0` 报 Invalid stream ID）。
- 流名仅 `[A-Za-z0-9_.-]`（`{}`/`:` 非法）——hash-tag 只能放普通 KV 键；单节点同样
  强制 CROSSSLOT（多键命令需同槽）。
- ES：PUT doc 前必须显式建索引；`match` 查询走 lowercase 分析而 keyword 文档存原始
  大小写（大小写敏感断言用 `term`）。
- 集群 lite 面：XADD/XGROUP/XRANGE 任意节点受理（raft 复制，全节点可读），
  `XREADGROUP >` 投递只在 leader 生效（其余节点回空数组，非 MOVED）；
  respawn 节点的 SQL 数据随 raft 追平，但其 XRANGE 对 kill 窗口内他节点写入的流
  不补读（已按「存活者可读」断言，避免把实现细节写成契约）。
- SQL：内联唯一索引用 `UNIQUE KEY uk_x (col)`；快照事务 dup INSERT 在 COMMIT 时才
  报 1062 并整笔回滚。

## 验证

- 10 例逐个跑绿（order 1.4s / session 1.9s / social 3.2–6.9s / reco 1.1s / log 0.4–10.4s /
  delay 2.4–32.8s / risk 0.7–30.3s / iot 0.7–4.2s / media 0.8–8.9s / multistore 10.5s）。
- 全量回归 `cargo test --workspace`：122 个测试目标（lib 1343 + bins 4 + bench 24 + 118 个
  集成二进制，共 ~1860 例）全绿。本机当日有外部磁盘压力（ext4 journal 积压 + 同机
  minios 压测进程），首次批量跑时 8 个目标出现「resp not accepting in 15/30s」类启动
  超时抖动（含 5 个**既有** kafka/list 套件与本套件 2 例）；磁盘静默后逐个重跑全部
  通过——判定为环境性 I/O 抖动而非回归（进程级套件的 15/30s 就绪预算为仓库既有约定，
  未放大）。批量重跑用 `--test-threads=4` 降低 fsync 并发。
- `cargo clippy --tests` 对新文件零告警（`FrontSpec` 派生 Default、`is_some_and` 收敛）。
- 未改任何 `src/` 生产代码；`tests/common/` 冻结未动；全部文件 ≤400 行。
