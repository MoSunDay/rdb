# 多存储场景批次 7：#71–#80 十例 e2e（2026-11-21）

承接批次 1–6（2026-11-19/20/21），批次 7 按用户要求**再扩 10 例**，
编号 #71–#80，仍只读复用 `tests/scenario_common/` 夹具、不改 `src/`。
覆盖此前未触达的 TTL 全家网格（半进位钉死）、RENAME/RENAMENX 语义、
非存储 set 代数读、MULTI/EXEC 全矩阵（EXECABORT vs 运行期错内联）、
消费者组重投递（XPENDING 两视图 + XAUTOCLAIM 三段形）、XADD 内联 trim、
SQL JOIN/USING/反连接、ES 排序分页与 10k 窗口、LPOP/RPOP count 形、
BZPOP 双端阻塞族。`tests/` 净增 10 个文件、全部 ≤190 行（上限 400）。
回归：190 个 e2e 目标 + lib 1343 + bins 4 全绿，clippy `-D warnings`
零告警（blocking_concurrency/multistore_failover/order_pipeline/
mysql_compat 四例在 4 线程重载下抖动，单线程复跑全绿，属环境负载非回归）。

## 场景矩阵

| # | 文件 | 平面 | 断言要点 |
|---|---|---|---|
| 71 | `scenario_lease_ttl_patrol_e2e.rs` | RESP | SETEX/PSETEX 铸期回显 TTL/PTTL 窗口、EXPIRE/PEXPIRE 续约、EXPIREAT/PEXPIREAT 绝对 unix 秒/毫秒、**TTL 半进位** `(ms+500)/1000`（1.6s→2 非floor 1）、PERSIST 清零 -1/二次 :0、缺键 TTL -2、`SETEX k 0 v` → `-ERR invalid expire time in 'setex' command`（psetex 同款） |
| 72 | `scenario_namespace_rebrand_e2e.rs` | RESP | RENAME 值+TTL 一起搬（旧名 -2）、覆盖已存在 dst、RENAMENX dst 存在 :0/空 :1、缺源 `-ERR no such key` 且 dst 不落地、跨 slot `-ERR CROSSSLOT Keys in request don't hash to the same slot`、TYPE 随迁 |
| 73 | `scenario_social_union_feed_e2e.rs` | RESP | SUNION/SINTER/SDIFF **非存储读变体**（同 slot 门 + CROSSSLOT 负例）、回成员数组（缺键=空集 `*0`）、SDIFF 方向性、SUNIONSTORE 物化对照 + SCARD + SMISMEMBER 整型旗标数组（按请求序） |
| 74 | `scenario_coupon_batch_multi_exec_e2e.rs` | RESP | MULTI 入队 `+QUEUED`、EXEC 按提交序回放分型回复、空队列 `*0`、无 MULTI `-ERR EXEC without MULTI`、嵌套 MULTI `-ERR MULTI calls can not be nested` + 弃批、队列期毒 → `EXECABORT Transaction discarded because of previous errors.`、运行期错内联不弃兄弟；**事务键全同 slot（`{c74}`）** |
| 75 | `scenario_ticket_redelivery_e2e.rs` | RESP | XADD 裸父回 `[解析后流名, id]`、XGROUP CREATE `0-0`/重名 -BUSYGROUP、XREADGROUP COUNT 配额、XPENDING 汇总 `*7 [3,min,max,alice :2,bob :1]`、range 行 `[id,consumer,:idle,:deliveries]`、XAUTOCLAIM Redis≥7 三段 `[cursor 0-0, entries, *0 deleted]` 换主（deliveries 1→2）、XACK :1 至 `[0,nil,nil]` |
| 76 | `scenario_telemetry_ring_buffer_e2e.rs` | RESP | XADD **内联 trim 选项先于 id**：`MAXLEN 3`/`MAXLEN ~ 2`/`MAXLEN = 1`（三种拼法同语义精确删）、`MINID 2-2` 严格小于删、选项可重复**后者胜**、NOMKSTREAM 缺流 `$-1` 不建键、XLEN 与 XINFO STREAM length 字段逐轮一致 |
| 77 | `scenario_course_enrollment_join_e2e.rs` | RESP+MySQL | 建表**复合主键** `(course, student)`（同学生可跨课）、INNER JOIN **USING (student)**、LEFT JOIN **复合 ON 条件**（AND 双等值）保留未选课 NULL 腿、`LEFT JOIN ... IS NULL` 反连接、ORDER BY+LIMIT 荣誉榜、GROUP BY COUNT/SUM 班级账 |
| 78 | `scenario_review_analytics_es_e2e.rs` | RESP+ES | `long` 数值字段 sort asc/desc、**默认 `_score` desc**（BM25 词频重的 match 文档领先）、字符串拼法 sort 默认 asc、from/size 分页保留窗口前 total、`_count` → `{"count":n}`、from+size>10000 → 400 `Result window is too large, from + size must be less than or equal to: [10000] but was [10001].` |
| 79 | `scenario_print_spooler_lpop_count_e2e.rs` | RESP | LPOP/RPOP count 正数回**数组**（LPOP 头先/RPOP 尾先）、超长 count 短取不报错、count 0 → `*0`、缺键 counted `*0` vs 单元素式 `$-1`、负数 `-ERR value is out of range, must be positive`、非整数 NOT_INT、WRONGTYPE 双端拒 |
| 80 | `scenario_auction_standby_bzpop_e2e.rs` | RESP | BZPOPMIN 立即回平铺 `*3 [key,member,score]`（score 原样回显）、到期 `*-1`（真实停车 ≥150ms）、BLOCK 0 + tokio::spawn 延迟 ZADD 唤醒、BZPOPMAX 对称取最高分、多键同槽扫描跳空首键落次键（回执键名=次键）、非浮点 `-ERR timeout is not a float or out of range` |

## 本批钉死的新语义（写前验证 + 实测确认）

- **TTL 半进位**：`TTL` 回 `(remaining_ms+500)/1000`（src 修复过 floor 为
  half-up）；1.6s 剩余读 2。PEXPIREAT/EXPIREAT 分别吃 unix 毫秒/秒绝对值。
- **XAUTOCLAIM 三段形（Redis≥7）**：`[cursor, entries, deleted-ids]`；
  min-idle 0 的合法换主把 deliveries +1 并改 owner，deleted 段空时回 `*0`。
  XPENDING 汇总形是 `[total,min,max] + [consumer,count]...`（2 消费者 3 行
  PEL = `*7`，消费者段字节序）；range 行 idle 列是 wall-clock 相关的
  `:int`，只能松断言。
- **MULTI 单 slot**：事务体整体过 slot 门，全部键须同 hash tag，否则队列期
  CROSSSLOT → EXECABORT。运行期错误（如 INCRBY 咬非数字串）在 EXEC 数组里
  **内联错误项**，兄弟命令照常提交；队列期错误（unknown command/嵌套 MULTI/
  阻塞命令）才整批 EXECABORT。EXEC 空队列回 `*0`。
- **ES 10k 窗口**：`from+size > 10000` → HTTP 400
  `illegal_argument_exception`，reason 原文 `Result window is too large,
  from + size must be less than or equal to: [10000] but was [10001].`；
  `_count` 体只含 `count` 字段。sort 字符串拼法（`"rating"`）默认 **asc**
  （仅 `_score` 默认 desc）。
- **XADD 内联 trim**：trim 选项位于 id **之前**（`XADD s MAXLEN 3 * f v`）；
  `~`/`=` 旗标接受但同为精确删；选项可重复、后者覆盖前者；NOMKSTREAM 对
  缺失流回 `$-1` 且不建键。
- **LPOP/RPOP count**：count>0 一律数组形（count=1 也是 `*1`）；count>长度
  短取；count=0 → `*0`；缺键 counted `*0`、无 count `$-1`。
- **SQL JOIN**：`JOIN ... USING (col)` 与 LEFT JOIN 复合 ON（AND 等值链）
  均支持；反连接 `LEFT JOIN ... WHERE right.key IS NULL` 可用；子查询进
  ON 条件仍拒（沿用既有语义）。建表仍强制 PRIMARY KEY（复合主键可）。
- **BZPOP**：立即命中走快路径回平铺三元组、score 按写入原样回显（"1.5"→
  "1.5"）；多键同槽按序扫描、空键跳过；timeout 解析失败文案与 BLPOP 共用。
