# 多存储场景批次 9：#91–#100 十例 e2e（2026-11-22）

承接批次 1–8（2026-11-19/20/21/22），批次 9 再扩 10 例，编号 #91–#100，只读复用
`tests/scenario_common/` 夹具（#97 额外声明 `mod kafka_front_common;`，沿用
`scenario_kafka_consumer_e2e` 的先例）、不改 `src/`。覆盖此前 scenario 层未触达的
命令面：XIDLE 空闲计时器查询/设定与家族级封存、BLMOVE/BRPOP 阻塞交接、DISCARD/
UNWATCH 事务中止面、server 控制台命令（PING/INFO/COMMAND/CONFIG/SELECT/QUIT）、
ES terms/should/match operator 布尔面、ES knn 近邻 + `_source` 投影 + `_doc` 排序、
Kafka SASL PLAIN token 门（`kafka_token`，scenario 首启）、SQL 子查询四形
（IN/NOT IN/EXISTS/标量）、SQL 集合运算全家（UNION/UNION ALL/INTERSECT [ALL]/
EXCEPT [ALL] + 复合尾 ORDER BY）、MySQL upsert 三件套（ON DUPLICATE KEY UPDATE
affected 语义 + REPLACE INTO + INSERT..SELECT 自快照）。`tests/` 净增 10 个文件、
全部 ≤152 行（上限 400）。回归：210 个 e2e 目标 + lib 1343 + bins 4 全绿，clippy
`-D warnings` 零告警（replay_rewind_consumer/sql_txn_e2e 在 4 线程重载下抖动，
单线程复跑全绿，属环境负载非回归）。

## 撞车消解（批次 9 初稿名单的两处核实）

- 初判 #96（ES knn）与 `cart_json_vector`（向量检索）疑似撞车——核实后者走 RESP
  `FT.SEARCH` KNN 面，ES `_search` knn 子句在 scenario 层零命中，face 不重复。
- 初判 #92（BLMOVE/BRPOP）与既有阻塞测试（`dispatch_blocking` 用 BLPOP、
  `ticket_redelivery` 用 XREAD GROUP BLOCK）无调用形重叠——BLMOVE/BRPOP 在
  scenario 层零命中，#92 为首次。

## 场景矩阵

| # | 文件 | 平面 | 断言要点 |
|---|---|---|---|
| 91 | `scenario_idle_session_seal_xidle_e2e.rs` | RESP | XIDLE 查询回配置秒、未配置 `-1`（无计时器）、缺流 `-2`；`XIDLE s N` `+OK` 且 `N=0` 清除；非数字 `-ERR invalid idle seconds`、缺流 `-ERR no such key`；1s 到期整个 family 封存（组记录同亡），重开后 XINFO GROUPS 空 |
| 92 | `scenario_nightshift_handoff_blmove_e2e.rs` | RESP | BLMOVE LEFT/RIGHT 对向唤醒回 moved 元素 bulk、双空道超时 `$-1`；BRPOP 立即回尾元素数组、空道超时 `*-1`；RIGHT/LEFT 双向审计序（LMOVE 源→目的两端核对）；阻塞命令须 park+drain_until 模式（`rf` 静默窗会漏 parked 回复） |
| 93 | `scenario_cashier_abort_discard_e2e.rs` | RESP | DISCARD 丢弃整队不落地 + 同时清 WATCH（回执对照）、无 MULTI `-ERR DISCARD without MULTI`；EXEC 无 MULTI 专用文案；嵌套 MULTI 报错弄脏队列 → 后续 EXEC `EXECABORT`；UNWATCH 恒 `+OK`；WATCH 竞态中止对照组（他连接改动 → EXEC nil） |
| 94 | `scenario_ops_console_server_e2e.rs` | RESP | PING 无参 PONG/payload bulk 回显；INFO Server/Cluster 节切片、Keyspace 非空含 `db0:keys=N`、未知节空；COMMAND COUNT ≥188、GETKEYS 回键、INFO 子命令；CONFIG 桩任意参数恒回 `cluster-require-full-coverage=no` 对；SELECT 0 `+OK`/他库 cluster mode 错/非整数错；QUIT `+OK` 后连接关闭 |
| 95 | `scenario_tags_terms_filter_e2e.rs` | ES | `terms` 多值 OR 并集、空 terms 零命中；裸 bool `should` = OR；`match` `operator:or/and` 分词宽严（`["i1","i5"]`）；terms + range `filter` 交集收窄 |
| 96 | `scenario_visual_search_knn_e2e.rs` | ES | `knn` 分数 ∈(0,1] 且近邻序、`filter` 限候选池；`_source:false` → `null`、`["_source":["brand"]]` 投影剪枝；`sort:["_doc"]` = 插入序且 `_score:null`（抑制不回） |
| 97 | `scenario_tenant_token_sasl_e2e.rs` | Kafka | pre-auth ApiVersions 回 22 键（含 SASL 对 17/36）；未认证 Produce 静默丢弃断连；错机制 33 + close；错 token 58 且错误消息无 token 片段 + close；PLAIN（`authz\0user\0token`）认证后单连接全通：Produce v3（base_offset 对账 RESP 种子）+ OffsetCommit v0 + OffsetFetch v0 回读 |
| 98 | `scenario_border_screening_subquery_e2e.rs` | MySQL | `IN (SELECT)` 命中（列表含 NULL 只影响 miss）；`NOT IN` 遇 NULL 列表全 UNKNOWN 零行（3 值逻辑钉死）、NULL 守卫变体放行；EXISTS/NOT EXISTS 非相关（全门/全放）与相关（`w.country = t.country`）；标量子查询 SELECT 列表（无命中 → NULL）与 `WHERE id = (SELECT MAX(id))` |
| 99 | `scenario_omnichannel_setops_e2e.rs` | MySQL | UNION 去重、UNION ALL 保重 + 字面量操作数；INTERSECT = 交集、INTERSECT ALL = min 乘数；EXCEPT = 差集、EXCEPT ALL = 多重集减法；INTERSECT 结合紧于 UNION（优先级钉死）；复合尾 ORDER BY + LIMIT 翻页整个复合体 |
| 100 | `scenario_loyalty_upsert_e2e.rs` | MySQL | ODKU 插入支路 aff 1、冲突变更支路 aff 2（`VALUES(col)` 读入行做 `points = points + VALUES(points)` 算术）、落同值 aff 0（写前比对零写）；REPLACE 现存键 = deleted+1、新键 1；INSERT..SELECT `CONCAT(id,'-snap')` 自快照分身且源行原样 |

## 追加钉死（写进覆盖地图的语义点）

- XIDLE 查询回流的配置秒；未配置 `-1`、缺流 `-2`；`XIDLE s N` 设定、`0` 清除；
  到期回收整个 family（组记录同亡），active expire 环 100ms。
- BLMOVE 成功回 moved 元素 bulk、超时 `$-1`；BRPOP 超时 `*-1`；两命令均可
  LMOVE 式双端定向（LEFT/RIGHT 源/目的独立）。
- DISCARD 同时清 MULTI 队列与 WATCH 集；EXEC/DISCARD 无 MULTI 各有专用文案；
  嵌套 MULTI 报错后队列已脏 → EXEC 收 `EXECABORT`。
- CONFIG 为桩：任意参数（GET/SET 皆然）恒回同一对；SELECT 仅 0 号库放行；
  QUIT `+OK` 后即断。
- ES knn 分数 ∈(0,1]；`_doc` 排序下 `_score` 抑制为 null；`_source` 投影按需
  剪枝、`false` 整体置 null。
- Kafka SASL：未认证数据命令静默丢弃（无错误帧）；失败消息不含 token 片段；
  认证通过后同一连接数据面全通。
- `NOT IN (SELECT ...)` 列表含 NULL → 谓词恒 UNKNOWN、零行（3 值逻辑）；
  INTERSECT ALL 取 min 乘数、EXCEPT ALL 为多重集减法；INTERSECT 优先级高于
  UNION；标量子查询空结果 → NULL。
- ODKU affected：插入 1、变更 2、同值 0（写前行比对）；REPLACE = 删除数 + 1。
