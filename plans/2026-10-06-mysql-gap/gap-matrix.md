# MySQL 兼容差距矩阵（2026-10-06）

全量差距按域 A–G 分组。列：feature | MySQL 行为 | rdb 现状（含 `src/sql/` 证据路径） |
优先级 P0/P1/P2 | 里程碑。优先级口径：P0 = 静默错误或主流客户端/ORM 首屏必踩；P1 = 常用兼容面；
P2 = 记录在案但本期不做（须写明理由）。行数为 `wc -l` 实测（2026-10-06，commit 98e17a5）。

## 落地状态 2026-10-06

M0-M5 **全部执行完毕**：P0/P1 共 41 行标记 `landed M<n>`（各里程碑记录见
`features/changelog/2026-10-06/mysql-m{0..5}-*.md`）；P2 行保持 deferred。各表
"rdb 现状"列是**计划期（M0 前）调查快照**，保留作历史证据，落地后的契约以
`COMPAT.sql.md`（原 `COMPAT.md` SQL 节）为准。实际选型与补充发现：

- 决策点 1 实选 **(b)**：集群模式 ODKU/REPLACE（含 ODKU-on-SELECT）从任一节点
  发起均 ER 1235 "not supported in cluster mode"；per-key gather 冲突读超点读
  RPC 预算，留待重估。plain `INSERT ... SELECT` 集群可用（SELECT 走 gather、行写
  2PC）。
- e2e 发现的两个计划外缺口：**CEIL/FLOOR 关键字形态**（`CEIL(x)` 专用 AST 节点
  曾 1235）——已在 M1 窗口补齐；**prepared 二进制协议数值绑定进文本型占位符列**
  （如 `COALESCE(NULL, ?)` 静态定型 VAR_STRING）编码器 io 错断连——**开放跟进项**
  （兼容编码或响亮错误），记录于 `COMPAT.sql.md` deviation ledger。
- 偏差按计划固化：plain INSERT pk 重复静默 upsert（决策 2）、byte-wise 大小写
  （决策 3）、GROUP_CONCAT 无内层 ORDER BY（决策 4）、TRUNCATE 按 DDL 语义
  （决策 5，实现为同名换 table_id）。

## A. 表达式与函数

现状基线：标量函数仅 13 个入口（`src/sql/exec/expr.rs` 的 `eval_func`，708 行，:535 起）：
LENGTH/CHAR_LENGTH/UPPER/LOWER/ABS/VERSION/NOW(含 CURRENT_TIMESTAMP/SYSDATE/LOCALTIME/
LOCALTIMESTAMP)/CURDATE(含 CURRENT_DATE)/LAST_INSERT_ID。聚合仅 COUNT/SUM/AVG/MIN/MAX
（`src/sql/parse/expr.rs` :177）。无 CASE、无 CAST、无字符串/数值/日期函数族。函数 arity 校验
在 `src/sql/parse/expr.rs` :145 `translate_function`。

| feature | MySQL 行为 | rdb 现状（证据） | 优先级 | 里程碑 |
| --- | --- | --- | --- | --- |
| CASE WHEN（简单式 + 搜索式） | 两形态均可，惰性求值，ELSE 缺省 NULL | 不支持：`parse/expr.rs` 无 CASE 分支，translate 报 unsupported | P0 | landed M1 |
| IF(cond,a,b) / IFNULL / NULLIF / COALESCE | 三值逻辑，NULL 传播 | 不支持（同上，函数注册表无条目） | P0 | landed M1 |
| CAST(x AS type) / CONVERT(x, type) | 显式类型转换，错误按 sql_mode | 不支持：`parse/expr.rs` :33 仅 TypedString 字面量特判 | P0 | landed M1 |
| CONCAT / CONCAT_WS | NULL 参数使结果 NULL（CONCAT_WS 除外） | 不支持 | P0 | landed M1 |
| SUBSTRING(s,pos[,len]) | 1-based，负 pos 从尾部数 | 不支持 | P0 | landed M1 |
| TRIM/REPLACE/LEFT/RIGHT/REPEAT | 标准字符串编辑 | 不支持 | P1 | landed M1 |
| LPAD/RPAD/LOCATE/HEX | 补齐/定位/十六进制 | 不支持 | P1 | landed M1 |
| ROUND(x[,d]) / CEIL / FLOOR / MOD | DECIMAL 输入产出精确值 | 不支持；DECIMAL 精确算术基建已存在：`exec/expr_decimal.rs`(237 行) | P0 | landed M1 |
| POW/SQRT/TRUNCATE/SIGN/GREATEST/LEAST | 数值杂项 | 不支持 | P1 | landed M1 |
| DATE_ADD/DATE_SUB + INTERVAL | 日期位移，类型保持（DATE→DATE） | 不支持；时间表示在 `temporal.rs`(252 行) | P0 | landed M1 |
| DATEDIFF / DATE_FORMAT | 日差 / 格式化 | 不支持 | P1 | landed M1 |
| YEAR/MONTH/DAY/HOUR/MINUTE/SECOND | 字段提取 | 不支持 | P1 | landed M1 |
| UNIX_TIMESTAMP / FROM_UNIXTIME | epoch 秒互转 | 不支持 | P1 | landed M1 |
| NOW/CURDATE（fsp） | 已支持；MySQL 默认会话时区，rdb 固定 UTC 微秒 | 已有：`exec/expr.rs` :608 起（行为差异记 COMPAT.md） | 已有 | — |
| `<=>`（NULL-safe 等号） | NULL<=>NULL 为真 | 不支持：`parse/expr.rs` :125 `translate_binop` 无映射 | P0 | landed M1 |
| REGEXP / RLIKE | 正则匹配（NULL 传播） | 不支持 | P1 | landed M1 |
| 位运算 & \| ^ << >> 与 XOR | 整数按位，逻辑 XOR | 不支持（同 translate_binop） | P1 | landed M1 |
| GROUP_CONCAT([DISTINCT] x [SEPARATOR s]) | 分组拼接，缺省 `,` | 不支持：聚合枚举在 `parse/ast.rs` :219 `AggFunc` | P1 | landed M1 |
| GROUP_CONCAT 内层 ORDER BY | 拼接顺序可指定 | 不支持；需分组内排序基建 | P2 | deferred（理由：v1 无分组内排序执行器，单独立项） |

## B. DML 冲突路径

现状基线：`parse/translate.rs`(727 行) `translate_insert`（:582）对 ON DUPLICATE / INSERT…SET /
REPLACE 一律 `SqlError::unsupported`；写入主路径在 `exec/write.rs`(749 行)。

| feature | MySQL 行为 | rdb 现状（证据） | 优先级 | 里程碑 |
| --- | --- | --- | --- | --- |
| INSERT … ON DUPLICATE KEY UPDATE（含 VALUES(col) 引用） | 冲突则更新，affected rows 1/2/0 | 大声拒绝：`parse/translate.rs` :584-586 | P0 | landed M2 |
| REPLACE INTO | 冲突先删后插，affected=2 | 大声拒绝：`translate.rs` :593 | P1 | landed M2 |
| INSERT … SELECT | 读侧物化后走行构造路径，同表读写允许 | 不支持（SELECT 源可用表，INSERT 目标拼接未实现） | P1 | landed M2 |
| INSERT … SET col=val | 列名=表达式形态 | 大声拒绝：`translate.rs` :589 | P1 | landed M2 |
| PK 重复 INSERT 静默 upsert | MySQL 报 ER 1062 | **有意偏离**：静默覆盖（StarRocks PK 模型导入依赖，`exec/write.rs` 主路径） | 记录 | —（决策点 2） |
| 唯一索引冲突 | MySQL 报 1062 | 已按 1062 拒绝：`tests/sql_types_e2e.rs` :371 | 已有 | — |

## C. 子查询与集合操作

现状基线：`exec/subquery.rs`(207 行) 已支持**非相关** IN 子查询重写为字面量集合
（`rewrite_expr` :58 / `rows_literals` :205）与非相关标量子查询（`scalar_of` :185）；
`exec/set_ops.rs`(398 行) 支持 CTE（非递归）与 UNION [ALL]（含去重、列宽化）。

| feature | MySQL 行为 | rdb 现状（证据） | 优先级 | 里程碑 |
| --- | --- | --- | --- | --- |
| EXISTS / NOT EXISTS（非相关） | 半连接，返回布尔 | 不支持：`subquery.rs` 无 EXISTS 重写 | P0 | landed M3 |
| EXISTS（相关） | 外层每行重求值 | 不支持（无相关子查询绑定机制） | P1 | landed M3 |
| 标量子查询（相关） | 外层列可被内层引用 | 仅非相关：`subquery.rs` :185 | P1 | landed M3 |
| IN 子查询（相关） | 同上 | 仅非相关：`subquery.rs` :58 | P1 | landed M3 |
| NOT IN + 空集/NULL 语义 | 空集 → 无行返回含 NULL 陷阱 | 非相关路径已按三值逻辑处理；相关路径缺（e2e 补测） | P1 | landed M3 |
| INTERSECT [DISTINCT/ALL] | 交集 | 显式拒绝：`parse/mod.rs` :402 断言报错 | P1 | landed M3 |
| EXCEPT [DISTINCT/ALL] | 差集 | 显式拒绝：`parse/mod.rs` :400 | P1 | landed M3 |
| UNION / UNION ALL | 已支持（去重 + 列宽化 + 左操作数列名） | 已有：`exec/set_ops.rs` | 已有 | — |
| WITH RECURSIVE | 递归 CTE | 显式拒绝：`parse/query.rs` :21-22 | P2 | deferred（理由：需要迭代执行器与行数上限语义，工程量大，v1 无消费场景） |

## D. DDL

现状基线：`exec/ddl.rs`(733 行) 已有 CREATE/DROP TABLE、索引与表缓存失效、列存段管理。

| feature | MySQL 行为 | rdb 现状（证据） | 优先级 | 里程碑 |
| --- | --- | --- | --- | --- |
| TRUNCATE TABLE | DDL 语义：隐式提交、事务内拒绝、清数据保结构、重置 auto-increment | 不支持（`parse/translate.rs` 无 TRUNCATE 分支） | P1 | landed M4 |
| RENAME TABLE | 原子改名，物理数据不动 | 不支持 | P1 | landed M4 |
| ALTER TABLE ADD/DROP INDEX | 在线加/删二级索引 | 不支持为独立语句（索引仅随 CREATE TABLE 声明：`exec/ddl.rs` / `src/sql/index/`） | P1 | landed M4 |
| ALTER TABLE ADD/MODIFY COLUMN | 加列/改列型 | 不支持 | P2 | deferred（理由：schema 版本迁移 + 存量行回填，影响 MVCC 编码，单独立项） |
| 复合索引 / 前缀索引 | 多列与前缀键 | 部分缺：复合 PK 已有（`tests/sql_composite_pk_e2e.rs`），二级复合/前缀索引未验 | P2 | deferred（理由：索引编码变更需迁移，先在 M4 记录） |

## E. 会话与协议面

现状基线：`front/vars.rs`(230 行) 有 sysvar 查表（`sysvar_value` :68）；`exec/show.rs`(316 行)
仅 SHOW TABLES / COLUMNS / INDEXES（`run` :13）。

| feature | MySQL 行为 | rdb 现状（证据） | 优先级 | 里程碑 |
| --- | --- | --- | --- | --- |
| SHOW CREATE TABLE | 由 schema 渲染 DDL | 不支持 | P1 | landed M4 |
| SHOW DATABASES | 列库名 | 不支持 | P1 | landed M4 |
| SHOW VARIABLES [LIKE p] / SHOW STATUS | 系统变量/状态（LIKE 过滤） | @@var 直接查已支持（`front/vars.rs`），SHOW 语句形态不支持 | P1 | landed M4 |
| DATABASE() / USER() / CONNECTION_ID() | 会话信息函数 | 不支持 | P1 | landed M4 |
| FROM DUAL | 无表 SELECT 的占位，等价无 FROM | 报表不存在：`parse/query.rs` :120 仅 `from=None` 映射 `TableRef::NoTable`，"dual" 被当普通表 | P0 | landed M0 |
| db.table 限定名 | 跨库限定 | 单库模型，限定名按表名处理 | P1 | landed M4（解析归一 + 明确语义） |
| KILL [QUERY] id | 终止连接/语句 | 不支持 | P2 | deferred（理由：需连接注册表与查询取消传播，v1 无强需求） |
| @@sysvar 直查 | 已支持（连接期与运行期） | 已有：`front/vars.rs` | 已有 | — |

## F. 查询语义修复（本期新发现，静默错误）

| feature | MySQL 行为 | rdb 现状（证据） | 优先级 | 里程碑 |
| --- | --- | --- | --- | --- |
| `ORDER BY 1` / `GROUP BY 1` 序数 | 指向输出列位置（1-based） | **静默错误**：`parse/translate.rs` :718 `translate_order` 把整型常量翻译为常量排序键（无操作），GROUP BY 同理由 `translate_select` 走 `translate_expr` | P0 | landed M0 |
| ORDER BY / HAVING 引用 SELECT 别名 | 先投影上下文、后 FROM 作用域 | **报未知列**：`exec/select.rs` :227 `validate_refs` 只对 FROM scope 做 `scan::check_expr`，别名列在投影外不可见 | P0 | landed M0 |
| `LIMIT ?`（占位符） | 二进制协议绑定后取非负整数 | 拒绝：`parse/translate.rs` :703 `translate_limit` 仅收数字字面量（SELECT 侧 `parse/query.rs` :205 `translate_limit_clause` 同一函数） | P0 | landed M0 |
| `SELECT 1 FROM DUAL` | 无表求值 | 见 E 组：报表不存在 | P0 | landed M0 |

## G. 明确不做（及理由）

| 项 | 理由 |
| --- | --- |
| binlog / 复制协议 | rdb 的复制由 openraft + SQL 2PC 层承担，binlog 无消费方；暴露半成品 binlog 反而误导 |
| 权限系统（GRANT/ROLE/列级权限） | 当前单账号模型（`front/auth.rs`，native-password）；多租户需求未出现，先保持大声拒绝 |
| caching_sha2_password | 保持 native-password-only，COMPAT.md 已记录；新增认证插件属协议工程而非 SQL 语义 |
| 窗口函数 | 需要分区排序执行器与帧语义，独立计划（本矩阵不留 P2 条目，避免半吊子承诺） |
| JSON / ENUM / SET 类型 | 类型系统 + 编码 + 索引联动，体量等同新数据面；v1 无场景 |
| ALTER ADD/MODIFY COLUMN | 见 D 组 P2：schema 迁移单独立项 |
| 锁等待/死锁检测（innodb_lock_wait 类） | 已有显式锁快速失败（MySQL 1205/1213 语义，COMPAT.md）；改为等待队列属事务层重构 |
| 多语句（multi-statement）批次 | 前端按单语句分发；客户端驱动普遍可拆分，收益低 |
| information_schema | 只读元数据面，可由 SHOW 系（M4）覆盖主要用法；完整模拟成本高 |
| collation / 大小写不敏感比较 | 决策点 3：存储无 collation 概念，保持 byte-wise（binary collation 等价），记录偏离 |

## 优先级统计（落地口径）

2026-10-06 落地后：上表 P0/P1 共 41 行全部 `landed`（M0-M5）；P2 五项维持
deferred（复合/前缀索引、ALTER COLUMN、WITH RECURSIVE、KILL、GROUP_CONCAT 内层
ORDER BY）。明细（计划期口径，供对照）：

- P0：F 组 4 项 + A 组 8 项（CASE/IF 族/CAST/CONCAT/SUBSTRING/ROUND 族/日期位移/`<=>`）+ B 组
  ODKU + C 组非相关 EXISTS = 15 项。
- P1：A 组 18 项 + B 组 3 项 + C 组 6 项 + D 组 3 项 + E 组 6 项 ≈ 36 项。
- P2（deferred，须在对应里程碑文档写明理由）：GROUP_CONCAT 内层排序、WITH RECURSIVE、ADD COLUMN、
  复合/前缀二级索引、KILL。
