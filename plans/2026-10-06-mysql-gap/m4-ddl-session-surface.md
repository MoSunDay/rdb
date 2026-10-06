# M4 — DDL 与 session 表面积（TRUNCATE / RENAME / ALTER INDEX / SHOW 面）

对应 `gap-matrix.md` D、E 组。前置：M0–M2 合入（`parse/translate.rs` 已完成 DML 拆分评估）。
**第一动作是拆分 `src/sql/exec/ddl.rs`**——733 行已逼近 800 上限。可与 M5 并行。

## 目标

1. `TRUNCATE TABLE t`：DDL 语义（决策点 5）——隐式提交当前事务（或事务内直接拒绝，取与
   现有 DDL-in-txn 一致的路径）、不可回滚、清空数据与索引并清理列存段、重置
   AUTO_INCREMENT 计数。
2. `RENAME TABLE a TO b`：catalog 改名（走 raft 复制的 catalog 路径），`table_id` 与物理
   key 编码不变（数据零拷贝）；表缓存失效复用既有机制。
3. `ALTER TABLE t ADD INDEX / DROP INDEX`：复用 `src/sql/index/` 既有建/删索引路径（在线
   重建条目）。
4. SHOW 面：`SHOW CREATE TABLE`（由 schema 渲染 MySQL 风格 DDL）、`SHOW DATABASES`、
   `SHOW [GLOBAL|SESSION] VARIABLES [LIKE p]`、`SHOW STATUS`（统一 sysvar 表）。
5. 会话函数：`DATABASE()`、`USER()`、`CONNECTION_ID()`。
6. `db.table` 限定名：解析归一（库名必须匹配当前库，否则大声报错——单库模型语义明确化）。

## 现状与证据

- `src/sql/exec/ddl.rs`（733 行）：CREATE/DROP TABLE、索引创建（随表）、缓存失效
  （:376 附近 race 窗口注释）、列存段管理均在其中；无 TRUNCATE/RENAME/ALTER INDEX 分支。
- `src/sql/exec/show.rs`（316 行）：`run`（:13）仅 ShowTables/ShowColumns/ShowIndexes；
  `type_name`（:143）已有类型名渲染（SHOW CREATE TABLE 的列类型列直接复用）。
- `src/sql/front/vars.rs`（230 行）：`sysvar_value`（:68）静态查表（version/autocommit/
  transaction_isolation 等）；`parse_sysvar_query`（:24）识别连接期 @@-query。无 SHOW
  VARIABLES 语句形态、无 DATABASE()/USER()/CONNECTION_ID()。
- 事务边界：COMPAT.md 记录"DDL inside a txn is rejected"——TRUNCATE 沿用该拒绝（事务内
  拒绝，自动提交下隐式提交语义由语句外层天然满足），与决策点 5 相容。
- AUTO_INCREMENT：`exec/sequence.rs`（468 行），计数是 raft 复制的 catalog 状态
  （`sql_sequence/<table>`）——TRUNCATE 重置即删除/重置该状态。
- catalog：`storage/catalog.rs`（359 行），表名 → schema 映射经 raft 复制；RENAME 是该映射
  的原子更新。

## 实现拆分（文件与职责，标注行数预算）

- `src/sql/exec/ddl_alter.rs`（新增，预算 ≤400）：
  - `truncate(shared, sess, table)`：schema 校验 → 行存数据/索引条目按表前缀清空（走 raft
    提交的 DDL 边界，与 DROP 的清理路径同类）→ 列存段 purge（`src/sql/columnar/gc.rs`，
    239 行既有 GC 路径）→ sequence 重置 → 缓存失效；事务内拒绝（与现有 DDL 一致）；
  - `rename(shared, sess, from, to)`：目标名冲突检查 → catalog 更新（raft guard 内原子）→
    `table_id`/物理 key 不动 → 缓存失效；
  - `alter_index(shared, sess, table, add|drop, index_def)`：复用 `src/sql/index/` 的
    建/删路径（`maintain.rs`）；ADD 走全表回填重建，DROP 走条目清理。
- `src/sql/exec/ddl.rs`（733 → 预算 ≤550）：保留 CREATE/DROP 与缓存失效核心；TRUNCATE/RENAME/
  ALTER 分发到 `ddl_alter.rs`；`ddl_tests.rs`（528 行）相应拆用例。
- `src/sql/exec/show.rs`（316 → 预算 ≤400）：
  - `show_create_table`：由 schema 渲染 `CREATE TABLE` 列清单（列名/类型/NULL/DEFAULT/PK/
    二级索引），类型列复用 `type_name`；引擎差异（DUPLICATE/KEY 子句）按 COMPAT.md 口径渲染；
  - `show_databases`：单库模型返回当前库（+ 系统口径文档化）；
- `src/sql/front/vars.rs`（230 → 预算 ≤400）：
  - 统一 sysvar 表：`sysvar_value` 扩展 STATUS 类变量（threads_connected 类可从连接注册表
    取的真实值优先，无源则静态值）；
  - `SHOW VARIABLES [LIKE p]` / `SHOW STATUS`：整表 + LIKE 过滤（byte-wise，决策点 3 一致）；
  - `DATABASE()` / `USER()` / `CONNECTION_ID()`：作为零参函数挂入 M1 的 `exec/func/mod.rs`
    注册表（`func/datetime.rs` 同级的会话信息放 `func/mod.rs` 内小节或 `func/session.rs`，
    ≤150 行；`CONNECTION_ID` 需要 front 层连接 id 注入会话——经 `SqlSession` 显式字段传递，
    保持纯函数式传参）。
- `src/sql/parse/translate.rs`（M2 后 ≤760）：TRUNCATE/RENAME/ALTER INDEX 翻译（+~40；若触发
  阈值则随 M2 的 `translate_dml.rs` 拆分一并外迁 DDL 翻译到 `translate_ddl.rs` ≤300）；
  `db.table` 限定名归一在 `object_name`（:~450）处理。
- AST：`parse/ast.rs` 增加 `Statement::Truncate/AlterTable` 变体（+~20）。

## 单测

- `exec/ddl_alter_tests.rs`（新增，≤400）：TRUNCATE 后空表 + auto-inc 从 1 重新开始；事务内
  TRUNCATE 拒绝；RENAME 后新旧名查询（旧名 no-such-table、新名数据完整、`table_id` 断言）；
  ADD/DROP INDEX 后查询计划使用/停止使用索引；
- `front/vars.rs` 测试扩展：LIKE 过滤、DATABASE()/USER() 值、CONNECTION_ID 非零单调；
- `exec/show.rs` 测试：SHOW CREATE TABLE 文本快照（列型/索引行）。
- `parse/mod.rs`：三条新语句翻译 + `db.table` 限定名（当前库名通过、异库名报错）。

## e2e

新文件 `tests/sql_ddl_surface_e2e.rs`（预算 ≤400；复用 M5 `tests/common/mysql.rs`）：

- TRUNCATE：插数 → TRUNCATE → 计数 0 → AUTO_INCREMENT 新行从 1；事务内拒绝路径；
  列存表 TRUNCATE 段清理（`columnar_e2e.rs` 模式）；
- RENAME：改名前后读写、SHOW TABLES 反映、集群三进程下 catalog 收敛；
- ALTER INDEX：加索引后点查走索引（EXPLAIN 断言）、删索引后回退扫描；
- SHOW CREATE TABLE / SHOW DATABASES / SHOW VARIABLES LIKE 'wait%' / SHOW STATUS；
- `SELECT DATABASE(), USER(), CONNECTION_ID()` 三函数值断言；
- `SELECT * FROM current_db.t` 通过、`other_db.t` 报错。

## 文档同步

- `COMPAT.md`：TRUNCATE 的 DDL 语义（隐式提交/不可回滚/auto-inc 重置）、RENAME 原子性、
  db.table 限定名单库语义、SHOW 面清单；
- `agents/rust/sql.md`：`exec/ddl_alter.rs`、`exec/show.rs`、`front/vars.rs` 职责更新；
- `features/sql-dataplane.md`、`features/e2e-coverage.md`、`features/changelog/<日期>/`。

## 验收

- D、E 组 P0/P1 标记"已有"（E 组 FROM DUAL 已在 M0 落地）；ADD COLUMN、复合/前缀索引、KILL
  保持 P2 + deferred 理由；
- `exec/ddl.rs` ≤550、`ddl_alter.rs` ≤400、`show.rs` ≤400、`vars.rs` ≤400；
- `cargo test --workspace --no-fail-fast` 全绿。

## 风险

- TRUNCATE 大表清理耗时（全前缀扫描删除）：与 DROP 同路径、同量级，接受 DDL 级耗时；
  集群下走 raft 提交后的异步清理窗口需 e2e 断言"清理期间新写入不复活旧数据"（ts 大于
  truncate 点即可，依赖既有 MVCC 语义）；
- RENAME 与并发 DML 的竞态：catalog 改名 raft 原子，物理 key 不变使得进行中的写不失效——
  e2e 并发改名+写用例覆盖；
- SHOW CREATE TABLE 与真实 `CREATE TABLE` 的**往返一致性**（渲染出的 DDL 重新执行可成功）：
  用 e2e 固化（round-trip 测试）；
- CONNECTION_ID 注入涉及 front 层 → 会话对象的字段穿透，注意不引入可变全局（经
  `SqlSession` 显式携带）。
