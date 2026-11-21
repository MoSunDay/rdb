# 多存储场景批次 4：#31–#40 十例 e2e（2026-11-21）

承接批次 1/2/3（2026-11-19/20），批次 4 继续在只读复用的 `tests/scenario_common/`
夹具上新增十个互不重复的多平面横切场景，编号 #31–#40，覆盖此前套件未触达的
Kafka 消费面、S3 checkpoint 灾备恢复、WATCH 乐观锁、阻塞列表弹出、SAVEPOINT
部分回滚、ES _bulk 重索引与键空间生命周期。`tests/` 净增 10 个文件、全部
≤190 行（上限 400），不改 `src/`、不改公共夹具。

## 场景矩阵

| # | 文件 | 前端 | 故事线要点 |
|---|------|------|-----------|
| 31 | `scenario_kafka_consumer_e2e.rs` | RESP+Kafka | 订单管道 RESP 侧 XADD 裸父 topic ×6（自动轮询落隐式 `orders/q0`）；手搓 Fetch v0 直连（无握手/SASL）整批读回 error=0/hwm=6/batch(0,6) 且六条值随批；OffsetCommit v0 组 ship@4→0、OffsetFetch 回 4；@4 续读尾两单 batch(4,2)；未提交新组回 -1 哨兵 |
| 32 | `scenario_saga_compensation_e2e.rs` | RESP+MySQL | 差旅 saga：订票+租车+酒店 BEGIN 内三腿；1305 酒店腿失败后 `SAVEPOINT sp_car` 之上的租车腿 `ROLLBACK TO` 回滚、机票腿存活；标记回滚后仍可再 ROLLBACK（幂等）、RELEASE 后再 ROLLBACK 报 `SAVEPOINT sp_car does not exist`；三路 LEFT JOIN 报表 NULL 腿渲染 `<NULL>` |
| 33 | `scenario_dispatch_blocking_e2e.rs` | RESP | 夜班派工：park()/drain_until() 静默窗帮手；外部 XADD 前 BLPOP 阻塞 <4s 唤醒 `*2`；空队列 1s 超时回 `*-1`（计时基点在 park 前）；BRPOPLPUSH `{job}` 同槽 src/dst 唤醒回成员且落 done 列表、超时回 `$-1`；BZPOPMIN 唤醒 `*3 (key,member,score)` 含 9.5 |
| 34 | `scenario_checkout_watch_e2e.rs` | RESP | 秒杀结账双连接 WATCH 同键竞争：败者先 EXEC 成功 `*2 :2 :1`，胜者（后写）EXEC 回原始 `*-1`；调整后重试胜出；EXEC 后 watch 消耗（再改他人键不再影响）；毒队列：CROSSSLOT 命令/blpop 入队即拒（原文断言），EXEC 回 `-EXECABORT` 前缀 |
| 35 | `scenario_checkpoint_dr_e2e.rs` | RESP+S3 | 灾备演练：三形态写入后等「创建时刻 ≥ 写完成时刻且 meta.json 在位」的 ckpt（id=unix_ms，首 ckpt 在 3s 启动宽限+interval 后）；kill_now → 抹数据目录但**保留 `raft/` 子目录**（ckpt 不含它）→ 拷回 ckpt 文件（跳过 meta.json 与 `.s3meta.json` 边车）→ respawn → 全部预写数据（含首 ckpt 后写入）在 RESP 上复活 |
| 36 | `scenario_key_lifecycle_e2e.rs` | RESP | 键空间生命周期：EXPIRE/TTL(-1)/PERSIST；RENAME 迁 TTL；8 类 TYPE 全验；SCAN 游标循环走全（含 MATCH/真实 COUNT 分页/TYPE 大小写不敏感过滤）；KEYS 对照；多键 DEL/EXISTS 同槽检查（`{kc}` tag）；惰性过期（GET 触发 purge 后 SCAN 不可见）；流名 quirk：`TYPE <流名>` 回 `none` 但 `SCAN TYPE stream` 能见 |
| 37 | `scenario_event_sourcing_e2e.rs` | RESP+MySQL | 账本事件溯源：XADD×4 回执与 XRANGE id 双向对账；XREADGROUP GROUP proj worker 全量投递+XACK :4；空 PEL 汇总 `*3 (:0,$-1,$-1)`；增量投递；XPENDING 非空汇总为**扩展 `*5`**（count,min,max,consumer,consumer-count）；XAUTOCLAIM `*3` 重领再 XACK；XINFO GROUPS/CONSUMERS 收口 |
| 38 | `scenario_cart_json_vector_e2e.rs` | RESP+MySQL | JSON 购物车（ARRAPPEND :3/ARRLEN/STRAPPEND :4/OBJKEYS/DEL）+SQL 结账；FT.CREATE TEXT+VECTOR DIM 4、FT.ADD 精确扫描、FT.BUILD 后 FT.INFO `ann_centroids:2`、KNN 召回、FT.DEL；VADD 方言 `VALUES <dim> <member> <f...>` ×3 + VSIM 最近邻 |
| 39 | `scenario_es_bulk_reindex_e2e.rs` | MySQL+ES | 目录 ETL 重索引：`PUT /<index>` 显式建索引（带 mappings）后 _bulk created×4 `errors:false`；`update` action 翻 `errors:true`（item 级 status 400/非 2xx）；doc DELETE→200 `result=deleted`、再删 `result=not_found` 非 404；_count 与 match 查询随删除收缩 |
| 40 | `scenario_gdpr_erase_e2e.rs` | RESP+MySQL+ES | GDPR 跨面抹除：SQL UPDATE 伪名化（affected :1）；ES doc 删除幂等（found→not_found）；RESP DEL/JSON.DEL `:1`→`:0`；u2 完好（KEYS 对照证只删了 u1 族） |

## 本批钉死的新语义（写前验证 + 实测确认）

- **ES _bulk**：**不自动建索引**——须先 `PUT /<index>` 带 mappings，否则
  400 `index_not_found_exception`；成功 items[].index.status=**201**；
  `update` action 翻 `errors:true`；keyword 字段查询用 `term` 非 `match`；
  doc DELETE 恒 200（`result=deleted`→`not_found`，**非 404**）。
- **SCAN/KEYS**：SCAN 回复 `*2 [cursor-bulk(hex 或 "0"), keys-array]`；
  SCAN/KEYS 是路由白名单**全局命令**（空前缀扫全库、跨 slot）；SCAN COUNT
  真分页；TYPE 过滤大小写不敏感。
- **流名语义**：流名禁 hash tag（`{kt}/stream`→`-ERR invalid stream name`）；
  裸名 XADD = parent topic 自动轮询落隐式子队列（name→name/q0）；
  **`TYPE <流名>` 回 `none`**（meta 物理槽=父 topic 槽，TYPE 按路由槽
  resolve 不到；`SCAN TYPE stream` 反而能看见）；XRANGE/KEYS 都能看到流键。
- **多键命令同槽检查**：DEL/EXISTS 多键 CROSSSLOT，多键场景须 `{tag}` 键。
- **TTL/RENAME**：RENAME 迁 TTL；TTL 无=-1、EXPIRE :1、PERSIST :1；惰性过期
  GET 触发 purge 后 SCAN 也看不到。
- **VADD 方言**：`VADD key VALUES <dim> <member> <f...>`（member 在浮点前）；
  TYPE vectorset 正常。
- **JSON.DEL 数组索引路径**：删除后数组**重排**，重放同索引会删下一个元素；
  幂等 `:0` 断言必须用不存在的**对象**路径。
- **XPENDING 双形**：非空汇总=扩展 `*5`（count,min,max,consumer-name,
  consumer-count）；空 PEL 才是 `*3 (:0,$-1,$-1)`。
- **阻塞弹出**：BRPOPLPUSH src/dst 需同槽——不同槽在「停车」时**立即**回
  `-ERR CROSSSLOT`（错误字节即刻到达、永不唤醒，测试侧表现为 drain 超时），
  用 `{job}inbox`/`{job}done`；BLPOP 超时 `*-1`（nil array）、BRPOPLPUSH 超时
  `$-1`（nil bulk）、BZPOPMIN 唤醒 `*3 (key,member,score)`。
- **MULTI 毒队列**：入队拒绝原文 `-ERR CROSSSLOT Keys in request don't hash
  to the same slot`、`-ERR command 'blpop' is not allowed in transactions`；
  EXEC 回 `-EXECABORT` 前缀；WATCH 冲突 EXEC=原始 `*-1`；EXEC 消耗 watch。
- **SAVEPOINT**：`SAVEPOINT sp`/`ROLLBACK TO SAVEPOINT sp`/`RELEASE SAVEPOINT
  sp`；未知/已 RELEASE 标记报 `SAVEPOINT <名> does not exist`；ROLLBACK TO
  后标记仍存活（幂等）、RELEASE 后消失；LEFT JOIN NULL 腿渲染 MVal::NULL。
- **checkpoint 恢复布局（#35 实测定案）**：数据目录=`<node_dir>/<resp_bind>`；
  raft 状态在**同数据目录的 `raft/` 子目录**（ckpt 不含它，恢复必须保留，
  否则单节点集群身份丢失）；ckpt id=创建时刻 unix_ms，须等「id ≥ 写完成时刻
  且 meta.json 在位」的 ckpt 才含该数据；恢复=kill→抹数据目录（保 raft/）→
  拷回 ckpt 文件（跳过 meta.json 与 `.s3meta.json` 边车）→respawn；
  front_world 的 s3data 根是 ProcNode.dir 的**兄弟目录**（`<world>/node0/`
  下并列）；首个 ckpt 在 3s 启动宽限+interval 之后才落盘（不能对 ckpt 根做
  即时 is_dir 断言）。
- **Kafka 消费面（#31）**：XADD 回复是 `*2` id 对（ms+seq）非单 bulk；Fetch v0
  响应 v0 无 throttle/lso/aborted 字段直落 records；records 原文含 UTF-8 值
  （lossy contains 可断言）；batch 头 records[..8]=base、[23..27]=last_delta。

## 回归与证据

- 单跑：10 个新目标逐一 `cargo test --test scenario_<name>_e2e` 全绿
  （#35 首跑两处修正：`ok()` 双参签名、ckpt 根路径/首盘延迟；#31 首跑两处
  修正：XADD 回复形状、windows 长度误配；其余一次过或按既有抖动重跑转绿）。
- 全量：`/tmp/chunk2.sh`（400s 预算版）分片跑 150 个目标（149 个 tests/*.rs
  含批次 4 十个新目标 + s3_e2e 子目录目标，`--test-threads=4`）+ lib 单测
  1343 例 + bins 4 例；首轮 `rocksmq_http_e2e` 1 例负载抖动失败，单跑复验
  3/3 绿，其余全部 rc=0。
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` 零告警。
- 文档同步：`features/e2e-coverage.md` 套件小节 30→40 例（批次 4 十条一行式
  条目 + 追加钉死语义）、`agents/rust/index.md` 场景计数 30→40、本 changelog。
