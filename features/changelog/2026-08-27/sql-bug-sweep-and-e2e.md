Commit: 98e17a5
# SQL 数据面缺陷集中修复与 e2e 契约补齐

## Context
对 `src/sql/` 做一轮正确性排查，确认 5 个缺陷与若干精度问题：并发 DDL 的 table-id
竞态、`NOT`/`IN` 的三值逻辑错误、OK 包不携带 `last_insert_id`、`length()` 语义与
StarRocks 未拒绝的 `AGGREGATE KEY`。集中修复，并以 e2e 锁定对外契约
（MySQL 与 StarRocks 模型）。

## Change Summary
- **DDL 目录写原子化**（`exec/ddl.rs`、`storage/catalog.rs`）：table-id 分配与目录
  变更（建表/建索引 mutations + schema）合并为同一 raft 写守卫窗口内的单一决策
  （`catalog_txn` / `DdlPlan{mutations, schema, changed}`），并发建表不再可能撞号；
  `changed` 标志区分 no-op 与已排空 mutations 的计划，防止 create_index 回填被跳过。
- **三值逻辑**（`exec/expr.rs`）：`NOT NULL -> NULL`；`IN` 先短路相等命中，
  否则见过 NULL 即结果 NULL（`NOT IN` 同理），对齐 MySQL/SQL 标准。
- **OK 包 last_insert_id**（`front/shim.rs`）：仅在语句为 INSERT 时携带会话
  last_insert_id，其余语句写 0。
- **表达式/元数据质量**（`exec/expr.rs`、`exec/{scan,select,set_ops,relation,mod}.rs`、
  `front/conv.rs`）：`length()` 按字节、`char_length()` 按字符；`ColMeta` 携带
  nullable/primary，SHOW COLUMNS 与线协议列定义置 `NOT_NULL_FLAG`/`PRI_KEY_FLAG`
  （DUP 模型的 schema pk 不打 key 标志）；子查询相关改写收紧为
  `BadField` + "unknown column" 前缀匹配。
- **StarRocks**（`parse/starrocks.rs`）：`AGGREGATE KEY` 以 MySQL 1235 大声拒绝。
- **e2e**（仅 `tests/`）：`tests/common/mod.rs` 提升共享 `start_sql_cluster(dir, n)`，
  7 个测试文件去掉重复起集群代码；新增回归——断连回滚开放事务、FOR UPDATE veto 与
  索引表 EXPLAIN 退化、不支持/零值时间字面量大声拒绝、AGGREGATE 拒绝文案、
  DUP 模型 3 节点收敛、并发建表隔离、OK 包 last_insert_id、NOT/IN 三值。

## Impact Surface
- SQL 数据面：DDL 并发语义、表达式求值、线协议列元数据与 OK 包、错误号。
- 对 RESP 数据面、控制面、存储布局无影响（目录 JSON 形状未变）。

## Notes / Compatibility
- 并发 `CREATE TABLE` 竞态败者的错误号为 **1050**（表已存在），非 1060。
- 自增 id 按 64 宽批量预留（既有行为）：首条自动分配语句从 65 起号，e2e 现已锁定。
- `2 IN (NULL, 1)` 返回 NULL 而非 FALSE——MySQL/标准三值语义，非实现偏差。
- mysql_async 客户端：`Conn::last_insert_id()` 取自最近 OK 包，SELECT 之后不可依赖
  （结果集终结符会覆盖）。

## Related Docs
- [SQL 数据面](../../sql-dataplane.md)
- [rust/sql 模块地图](../../../agents/rust/sql.md)
- 契约与偏差：`COMPAT.md` "SQL data plane" / "StarRocks table-model DDL" 节
