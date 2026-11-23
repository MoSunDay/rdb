# 多存储场景批次 10：#101–#110 十例 e2e（2026-11-22）

承接批次 1–9（2026-11-19/20/21/22），批次 10 再扩 10 例，编号 #101–#110，只读复用
`tests/scenario_common/` 夹具（#110 额外声明 `mod kafka_front_common;`，沿用 #97
`scenario_tenant_token_sasl` 的先例）、不改 `src/`。覆盖此前 scenario 层未触达的
命令面：FLUSHDB 多家族整体擦除（stream 组回收、ASYNC/SYNC 拼写）、SQL CTE（别名
自连接/别名表链/体内子查询 + 与 UNION [ALL] 复合、`WITH RECURSIVE` 1235）、SQL
字符串 + 控制族在列数据上的形状化（CONCAT/CONCAT_WS 的 NULL 传播与跳过）、
DATE_ADD 多 INTERVAL 单位走表（月钳/HOUR 升格 DATETIME）+ DATEDIFF 双向 +
DATE_FORMAT 定值 crib、UPDATE/DELETE `ORDER BY … LIMIT` 字面拼写（非 prepared）、
SHOW/DESCRIBE/SHOW INDEX/SHOW CREATE 往返/EXPLAIN/RENAME 审计台、`START TRANSACTION`
拼写 + 可重复读快照 + 事务内 DDL 四形 1235、`SUM(DISTINCT)`/字符串 MIN/MAX/LIKE
转义、ES `_bulk` 索引生命周期（`_cat/indices` 空表头/create 冲突/delete 命中与
未命中/refresh）、Kafka Metadata v1 / ListOffsets v5 / 两段 JoinGroup+SyncGroup 握手 /
OffsetCommit v2 消费群控制室（Api keys 3/2/11/14/15/8/9 scenario 首启）。`tests/` 净增
10 个文件、全部 ≤133 行（上限 400）。回归：220 个 e2e 目标 + lib 1343 + bins 4 全绿，
clippy `-D warnings` 零告警（bits_e2e/blocking_concurrency_e2e 在 4 线程重载下各抖
1 例，单线程复跑均 9/9 绿，属环境负载非回归）。

## 撞车消解（批次 10 初稿名单的核实）

- 初判 #101（FLUSHDB）与 `flushdb_lite_e2e` 疑似撞车——核实后者只擦 lite-MQ 面
  （stream/组），`backup_*` 只测 -READONLY 拒写；#101 是 RESP 五家族 + stream 根的
  整体普查（DBSIZE/RANDOMKEY/每族读空）+ 组记忆随擦除回收 + ASYNC/SYNC 直通，
  scenario 层首次。
- 初判 #102（CTE）与 `mysql_compat_e2e` 疑似撞车——核实后者已驱自连接/别名表链/
  体内子查询三形（即本例 crib 源），#102 在故事语境重钉三形并新增 CTE ∘ UNION ALL
  vs UNION 复合（4 行 vs 2 行）与 `WITH RECURSIVE` 1235 回执；
  `sql_setop_cluster_e2e` 的 WITH 仅作集群面集合运算前置。
- 初判 #109（`_bulk`）与 `cdn_edge_logs`/`crypto_exchange_*` 疑似撞车——核实两者只把
  `_bulk` 当摄入工具（纯 `index` 动作），`_cat/indices` 此前只在 `es_e2e` 出现；
  create 冲突/delete 命中未命中/refresh→docs.count 的生命周期在 scenario 层零命中。
- 初判 #110（Kafka 握手）与 `scenario_kafka_consumer_e2e` 疑似撞车——核实后者虽挂
  `kafka_front_common` 但只用 fetch_v0/commit_v0/fetch_offset_v0，JoinGroup/SyncGroup/
  DescribeGroups 此前只在非 scenario 的 `kafka_group_e2e`；Metadata v1 Some/None、
  ListOffsets v5 earliest/latest、OffsetCommit v2（#97 为 v0）均为 scenario 首次。
- 其余核实无撞车：#103/#104 以列数据走 funcs 套件的 crib 值（不新增函数面）；
  #105 对 `sql_query_semantics_e2e` 仅驱 prepared 的字面拼写补位；#106 的 SHOW
  TABLES 在 `scenario_bank_2pc_cluster_e2e` 仅作可见性核对，DESCRIBE 6 列/SHOW
  INDEX/SHOW CREATE 往返/EXPLAIN 为 scenario 首次；#107 `START TRANSACTION`、
  #108 `SUM(DISTINCT)`/LIKE 转义在 tests/ 全域零命中。

## 场景矩阵

| # | 文件 | 平面 | 断言要点 |
|---|---|---|---|
| 101 | `scenario_factory_reset_flushdb_e2e.rs` | RESP | 五家族播种后 DBSIZE ≥5、RANDOMKEY 命中、XLEN 1；`FLUSHDB FOO` `-ERR syntax error` 且数据原样；`FLUSHDB` `+OK` 后 DBSIZE 0、GET `$-1`/HGETALL `*0`/LRANGE `*0`/ZRANGE `*0`、stream 根 EXISTS 0、RANDOMKEY nil；重建 XADD 全名回 bulk id、XLEN 1、XINFO GROUPS 裸 `*0`（组记忆未幸存，沿用 #91 重开契约）；`FLUSHDB ASYNC`/`SYNC` 均 `+OK` 且仍空 |
| 102 | `scenario_sales_report_cte_e2e.rs` | MySQL | CTE 别名自连接 `a.id + 1 = b.id` 回 (1,2)(2,3)；`WITH x (n) AS …, y (m) AS (SELECT n + 10 FROM x)` 别名表链回 11/12；CTE 体内 `IN (SELECT …)` 回 COUNT 2；CTE ∘ UNION ALL 保重 4 行、UNION 去重 2 行；`WITH RECURSIVE` 1235 且消息含 recursive |
| 103 | `scenario_invoice_formatting_funcs_e2e.rs` | MySQL | UPPER/LOWER/SUBSTRING(7,4)/LEFT(6)/RIGHT(4) 列上 crib；LPAD(CAST(qty AS CHAR),3,'0')=`003`、REPLACE、REPEAT(qty=3)=`===`；COALESCE/IFNULL 回填 `standard`、NULLIF(qty,7) 保 7/NULLIF(qty,3)→NULL；CAST AS DECIMAL(6,2) 保 `1250.00`、`' 42 '`→42；CONCAT_WS 跳过 NULL note 而 COALESCE 版留占位；`CONCAT(note,'!')` NULL 传播 `<NULL>` |
| 104 | `scenario_shift_schedule_datetime_e2e.rs` | MySQL | `DATE_ADD(day, INTERVAL 3 DAY)`=`2024-02-03`、`INTERVAL 1 MONTH` 自 01-31 钳到闰年 `2024-02-29`；`INTERVAL 25 HOUR` 把裸 DATE 升格 `2024-01-03 01:00:00`、MINUTE 走时钟列 `04:34:05`；DATEDIFF 对 `'2024-01-01'` 正反 `1`/`-1`；DATE_FORMAT 列上 `%H:%i:%s`/`%Y-%m-%d` + 定值 crib `%T`/`%W`=Thursday/`%M`=February/`%j`=060 |
| 105 | `scenario_leaderboard_trim_dml_limit_e2e.rs` | MySQL | `UPDATE … SET score + 100 ORDER BY score DESC LIMIT 2` aff 2 且仅前二 ≥140；`WHERE score > 100 ORDER BY … LIMIT 1` 先筛后切 aff 1；`DELETE … ORDER BY score ASC LIMIT 2` aff 2 尾二出队；`LIMIT 0` 对 UPDATE/DELETE 均 aff 0 且榜面不变（字面拼写，text 协议） |
| 106 | `scenario_schema_audit_show_e2e.rs` | MySQL | SHOW TABLES 含表、SHOW DATABASES 恰 `["rdb"]`；DESCRIBE 3 行 × 6 列，Field/Type/Key = bigint PRI / varchar MUL / varchar UNI；SHOW INDEX 三行 PRIMARY-id/ix_src-src/uk_tag-tag，Non_unique `0,1,0`；EXPLAIN `SeqScan audit_events` vs `IndexScan ix_src -> 2 pks`；SHOW CREATE 片段（反引号/换行/ENGINE=InnoDB）+ DROP 后按渲染重建 → 渲染逐字节相同；ALTER RENAME TO 后 SHOW TABLES 换名、数据随迁 COUNT 3、旧名 1146 |
| 107 | `scenario_draft_editor_rollback_e2e.rs` | MySQL | `START TRANSACTION`（BEGIN 别名）内 INSERT 作者 COUNT 1 / 同伴 0；ROLLBACK 后双方 0；重暂存 INSERT+UPDATE 后 COMMIT 同伴读到 `v2 draft`；B 开快照后 A 自动提交插行，B 仍 1 行、自身 COMMIT 后 2 行（可重复读）；事务内 CREATE TABLE/DROP TABLE/CREATE INDEX/ALTER RENAME 四形均 1235，事务仍可 DML，ROLLBACK 后干净 |
| 108 | `scenario_donation_audit_aggdistinct_e2e.rs` | MySQL | SUM=350 vs SUM(DISTINCT)=150、COUNT(*)=4 vs COUNT(DISTINCT donor)=3；字符串 MIN/MAX 字典序：MIN `ali_ce` < MAX `alice`（`_` 0x5F < 字母）、ref_code `DON%24A` < `DON24C`（`%` 0x25 < `2`）；LIKE `'DON\%%'` 只配字面 % 3 行 vs `'DON%'` 4 行、`'ali\_%'` 3 行排除 alice、`'ali\__e'` 回 ids 1,2,4；分组 ali_ce 200/100；空窗 SUM `<NULL>` |
| 109 | `scenario_index_lifecycle_cat_bulk_e2e.rs` | ES | 空节点 `_cat/indices` 200 仅表头；建索引后 `_bulk` 两 `index` 201 created、errors:false；`create` 撞现存 id：items[].create.status 400 + error.type `version_conflict_engine_exception`、errors:true；`delete` 命中 `deleted`/未命中 404 `not_found` 且 errors 仍 false；`_refresh` 后 cat 表头 + 1 行 `green open fleetops …` docs.count 1 |
| 110 | `scenario_consumer_fleet_metadata_e2e.rs` | Kafka | 裸父 XADD 回 `*2` 种子、Produce v3 base_offset=1（hwm 2）；Metadata v1 `fleet` Some(1)/`no-such-topic` None；ListOffsets v5 latest 回 offset 2 / earliest 回 0（leader epoch -1 哨兵）；JoinGroup 空成员 79 + `rdb-fleetops-` 前缀 id、重入 generation 1 自任 leader、protocol range、members 1；SyncGroup 回 `[fleetp0]`；DescribeGroups v0 Stable/consumer/range/成员 1；OffsetCommit v2 0 + OffsetFetch v0 回 2 |

## 追加钉死（写进覆盖地图的语义点）

- FLUSHDB 擦除五家族 + stream 根，组记忆随之回收（重建流 XINFO GROUPS 裸 `*0`）；
  junk 参数 `-ERR syntax error` 且不触发擦除；`ASYNC`/`SYNC` 直通恒 `+OK`。
- CTE 别名可自连接、带别名表的链式引用（后者读前者）、体内可嵌子查询；CTE 与
  UNION [ALL] 复合保各自乘数语义；`WITH RECURSIVE` 1235（消息点名 recursive）。
- COALESCE/IFNULL 回填、NULLIF 精确匹配→NULL；CONCAT 任一 NULL 操作数 → NULL，
  CONCAT_WS 跳过 NULL 片段；CAST DECIMAL(p,s) 保 s 位小数。
- DATE_ADD 月加钳到目标月末（闰年 02-29）；HOUR 加法把 DATE 升格 DATETIME；
  DATEDIFF 只算日期部分、参数反向取负；DATE_FORMAT `%W`/`%M` 英文名、`%j` 三位。
- UPDATE/DELETE `ORDER BY … LIMIT n` 字面拼写：aff = n（或筛后不足 n）、
  `LIMIT 0` 零 affected。
- DESCRIBE 恒 6 列，Key 旗标 PRI/MUL/UNI；SHOW INDEX 每索引列一行、Non_unique
  唯一 0/普通 1；SHOW CREATE 渲染可往返（DROP→重建→同渲染）；EXPLAIN 头条
  `SeqScan <table>` / `IndexScan <ix> -> <n> pks`；RENAME 后旧名 1146。
- `START TRANSACTION` = BEGIN；事务内写仅作者可见；ROLLBACK 全弃、COMMIT 全落；
  快照跨连接可重复读至自身 COMMIT 刷新；事务内 DDL（CREATE/DROP/INDEX/RENAME）
  1235 且不废事务。
- SUM(DISTINCT) 折叠同额、COUNT(DISTINCT col) 去重计数；字符串 MIN/MAX 按字节序
  （`%`<`2`、`_`<字母）；LIKE `\%`/`\_` 匹配字面通配符（sqlparser MySQL 直通，
  需配尾通配符才有非空命中）；空窗 SUM 为 NULL。
- ES `_bulk`：空节点 `_cat/indices` 仅表头；`index` 新建 201 created；`create` 撞
  现存 → item status 400 + `error.type` version_conflict_engine_exception、
  errors:true（真 ES 回 409，400 为实现现状，已知偏差）；`delete` 未命中 404 not_found 不翻 errors；`_refresh` 后 docs.count
  准确。
- Kafka Metadata v1 未知 topic → None；ListOffsets v5 timestamp -1 latest = hwm、
  -2 earliest = 0，leader epoch -1 哨兵；JoinGroup 空成员 79 取 broker 分配 id、
  重入 generation 1；SyncGroup 单片 `[<topic>p<partition>]`；DescribeGroups Stable；
  OffsetCommit v2 / OffsetFetch v0 跨版本回读。
