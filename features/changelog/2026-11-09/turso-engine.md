# 2026-11-09 · turso 引擎全量落地（第二嵌入式 SQL 引擎，T0–T6）

Commit: 待回填（T6 文档收尾批，落提交时补 sha；代码于 T0–T5 各批合入）

来源计划：`plans/2026-11-09-turso-engine/`（`00-research.md` turso crate 侧调查 +
`00-rdb-context.md` rdb 侧落点地图）；契约与两引擎对照矩阵、偏差台账
（(k)–(z)）：`COMPAT.sql.md` "turso engine" 节；模块地图：`agents/rust/sql.md`。

## 动机

在既有 MySQL 线协议前端后接入第二个嵌入式 SQL 引擎（tursodatabase/turso，
SQLite 文件格式兼容），以**库级路由**（db 名 `turso_` 前缀）与 rocks 引擎共存：
SQLite 生态文件可直接挂载使用，向量检索复用引擎内置函数，rocks 主路径零行为
变化。落地六里程碑 T0–T5 + 文档收尾 T6。

## T0 研究（00-research / 00-rdb-context）

- 只读源码调查 + 独立冒烟二进制（同版本 crate）验证嵌入 API：Builder/Database/
  Connection 生命周期、多连接、executor 无关 Future、错误分类、向量内置函数、
  UDF 注册不可达（⑥）等；rdb 侧给出逐文件落点地图（exec 入口/USE 校验/配置管道/
  raft 目录/测试基建）与最小切口清单。
- 拍板 D1–D8：pin `=0.9.0-pre.2` 且 `default-features = false`（mimalloc 会换
  进程级分配器）、VECTOR(N) DDL=BLOB+应用侧校验、事务字符串直通、预编译=重放
  query+params 等。

## T1 路由骨架（smoke）

- `src/sql/turso/{mod,engine,route}.rs`：conf 双闸（`turso_engine` + `turso_db_dir`）
  + `turso_` 前缀库级路由；`sql/exec/mod.rs::dispatch` 插 cfg 门闩的
  `turso::route`（`Ok(None)` 落回宿主执行器，COM_QUERY 与 prepared 双路径生效）。
- 引擎绑定但未实现的语句一律 1235（"... on turso engine (see COMPAT.sql.md)"），
  绝不静默落到错误引擎的表。

## T2 目录与库文件（catalog）

- `src/sql/turso/catalog.rs`：CREATE/DROP DATABASE = raft 存在性条目
  （`sql_db/<name>` = `"turso"`，leader-only）+ 节点本地库文件；SHOW DATABASES
  合并（rdb ∪ `*.db` 文件名）；USE 校验文件存在（未建 → 1049）。
- `meta.rs`：SHOW TABLES / DESCRIBE / SHOW CREATE TABLE 直读库自身
  sqlite_master / PRAGMA table_info（rdb 不留影子 schema）。

## T3 CRUD 翻译 + parity（crud / parity）

- `translate/`（expr/funcs/select/dml/ddl）：MySQL AST→SQLite SQL 渲染器即
  allowlist 门（单表 SELECT 干线、多行字面 INSERT、UPDATE/DELETE、TRUNCATE=DELETE、
  类型映射含 `vector(n)`、AUTO_INCREMENT=INTEGER PRIMARY KEY AUTOINCREMENT）。
- `conv.rs`：turso 值/列元数据 → rdb Value/ColMeta（向量列 BLOB→`'[..]'` 文本）。
- 双引擎同语句对照套件钉死一致面与五条已知偏差（DECIMAL→REAL 渲染、AVG 格式、
  no-op UPDATE affected、多行 INSERT last-insert id、TRUNCATE 水位）。

## T4 向量面（vector）

- `translate/vector.rs`：函数族映射到引擎内置（`vector32`/`vector_extract`/
  `vector_distance_{cos,l2}` + dot 符号翻转回真内积）；写路径应用侧维度/格式校验
  （D6，错误面对齐 rocks 的 1292）；精确 KNN（ORDER BY DISTANCE … LIMIT）。
- CREATE VECTOR INDEX → 1235（仅精确 KNN；ANN 是 rocks 引擎专属）；与 rocks 同
  数据同查询的 id 序 parity（数值断言一律 epsilon=1e-6，f32 vs f64）。

## T5 事务与预编译（txn）

- 显式事务 = 会话钉住连接上的字符串直通 BEGIN/COMMIT/ROLLBACK（连接即事务账本，
  `sess.txn` 状态机不参与）；MySQL 的 BEGIN 隐式提交语义应用侧模拟；txn 外
  COMMIT/ROLLBACK no-op OK。
- 预编译语句参数绑定进 rdb IR 后重放（路由对 prepared 同样生效）；事务内 USE 换库
  → 1235；双连接脏读检查（WAL 式快照，无脏读）；隔离级别回显=会话记录（引擎忽略）。

## 关键决策

- pin `=0.9.0-pre.2`、`default-features = false`：pre-1.0 API churn 冻结；mimalloc
  会声明进程级 `#[global_allocator]`，fts 无用——分配器隔离。
- 前缀路由：sqlparser `CreateDatabase` 无 ENGINE 子句，`turso_` 名前缀即引擎标记。
- 目录仅存存在性条目 + 节点本地豁免：无 MVCC/2PC/slot 路由/MOVED/raft schema 复制；
  跨引擎/跨库语句结构性不可能（限定名单段 + 单表 allowlist → 1235）。
- 仅精确 KNN；事务字符串直通；向量校验放应用侧（引擎无列约束可挂）。

## e2e（tests/，六套件）

- `turso_smoke_e2e`（T1）：常量算术路由、1235 拒绝形状、默认库零变化、
  未开引擎节点 USE turso_x 仍 1049。
- `turso_catalog_e2e`（T2）：CREATE/DROP DATABASE（1007/1008/IF 形状）、SHOW
  DATABASES 合并、sqlite_master 三件 SHOW、最小数据往返、权限位决定的只读重开
  （写 → 1290）。
- `turso_crud_e2e`（T3）：类型化建表（pk AUTO_INCREMENT/NOT NULL/内联索引）、多行
  INSERT 与 last-insert id、SELECT 干线（含 `LIMIT o,n` 交换/GROUP BY 聚合）、
  UPDATE/DELETE affected、TRUNCATE/DROP、allowlist 外 1235 矩阵。
- `turso_parity_e2e`（T3）：同 schema 同语句双引擎对照——一致面 + 五条台账偏差
  逐条引擎侧断言。
- `turso_vector_e2e`（T4）：VECTOR(N) DDL/DESCRIBE、写路径校验、函数族、精确 KNN、
  1292/1582/1235 矩阵、与 rocks 的 id 序 parity（epsilon 纪律）。
- `turso_txn_e2e`（T5）：直通事务三件 + 隐式提交语义、txn 外 no-op、prepared
  INSERT/SELECT `?` 绑定、事务内 USE 1235、双连接脏读检查、隔离级别回显。

## T6 文档收尾（本批，docs-only）

- `COMPAT.sql.md` 新增 "turso engine" 节（两引擎语句对照矩阵 + 偏差台账
  (k)–(z)）；`agents/rust/sql.md` 模块地图/测试地图补 `sql/turso/` 子树与六套件；
  `plans/index.md` 推进 landed；`features/e2e-coverage.md` 入台账。

## 后续（非本批）

- ANN 检索跟随 turso 上游路线图（当前仅精确 KNN，CREATE VECTOR INDEX 1235）。
- pre-1.0 升级（含 1.0 正式版）需 mini-plan：重跑 T0 冒烟集 + 编译墙时复测。
- JOIN/UNION/子查询 allowlist 按需渐进扩容（渲染器即门，扩容须同步对照矩阵）。

## 验证

- T0–T5 各批合入时 `cargo test --workspace` 全绿（含六个 `--test turso_*_e2e`，
  见各批合入记录与 `features/e2e-coverage.md`）；T6 为 docs-only 批，无代码变更
  （撰写时工作树含并行车道 wukong-ft Batch 2 的未完成修改，全量测试以其批为准）。
