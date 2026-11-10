# 2026-11-09 turso-engine：rdb 现有 SQL 面集成上下文（00-rdb-context）

> 状态：proposed
> 日期：2026-11-09
> 输入：只读调查，仓库快照 commit `b7378c0`（行号以该快照为准）
> 关联：本目录总览；参考研究 `plans/2026-11-02-turso-ref/`（turso/libsql pin d8bc862d）；
> 契约台账 `COMPAT.sql.md`；模块地图 `agents/rust/sql.md`

为"在现有 MySQL 协议前端后面接入第二个嵌入式 SQL 引擎（turso）"提供精确的落点地图：
逐项给出文件路径 + 行号 + 短摘录，最后给出最小切口清单（§13）。调查不改任何代码。

## 1. 执行入口：`exec::execute` / `exec::dispatch`

文件 `src/sql/exec/mod.rs`（293 行）。

- `execute()` L150-178：每语句一个入口。先做**逐次执行的会话函数替换**（L160-170，
  `session_funcs::substitute`，把 `DATABASE()`/`USER()` 绑成字面量），再 L171
  `let out = dispatch(shared, sess, stmt).await;`，最后统一打点
  `monitor::observe_sql_latency`（L172-176，kind 来自 `Statement::metric_kind`
  `src/sql/parse/ast.rs` L549）。
- `dispatch()` L180（私有 `async fn dispatch(shared: &Shared, sess: &mut SqlSession, stmt: Statement)`）：
  - L188-204 事务内 DDL 拒绝（按**语句种类清单** matches!，`ErrorCode::TxnDdl` → 1235）：
    ```rust
    if sess.txn.is_some() && matches!(stmt, Statement::CreateTable {..} | Statement::DropTable {..} ...)
    ```
  - L207 `Statement::Begin`；L218 Commit；L225 Rollback；
  - L231-237 DDL 臂 → `ddl::run(shared, stmt).await`（注意不传 sess）；
  - L238-240 DML 臂 → `write::insert/update/delete(shared, sess, stmt)`；
  - L241 `Statement::Select(q)` → `select::run`；L245 SelectCompound；L249 Explain；
  - L250-256 SHOW 族 → `show::run(shared, sess, &stmt)`；
  - **L257-260 `Statement::Use`**：
    ```rust
    Statement::Use(db) => { sess.db = db; Ok(ExecOutcome::Ok) }
    ```
    **不做任何校验**（见 §4 的"两处校验"讨论）；
  - L261 SetIgnored；L262-267 SetIsolation（存 `sess.isolation`）；L270-291 SAVEPOINT 族。
- 关键类型（同文件）：
  - `ColMeta` L60-77：字段 `table: String` / `name: String` / `sql_type: SqlType` /
    `nullable: bool` / `primary: bool`；构造器 `ColMeta::computed(table,name,sql_type)` L78。
  - `ExecOutcome` L90-98：`Rows { columns: Vec<ColMeta>, rows: Vec<Vec<Value>> } |
    Affected(u64) | Ok`。
  - `SqlSession` L100-121：`db: String`、`txn: Option<crate::sql::tx::Txn>`、
    `isolation: Option<String>`、`last_insert_id: i64`、`info: SessionInfo`。
  - `SessionInfo` L123-146：`user: String`（`user@host`）、`connection_id: i64`，
    Default = `root@localhost`/0。握手时由 `front::new_shim` 一次性写入（shim.rs L62-88）。
  - `DEFAULT_DB` L147：`pub const DEFAULT_DB: &str = "rdb";`
- 调用方：前端两处 —— `front/shim.rs` `on_query` 间接经 `run_statement`（L350）与
  `on_execute` 直呼 `exec::execute`（L288）。**每会话引擎路由钩子的最小插入点**：
  `dispatch()` 内、事务内 DDL 守卫（L204 收口）与 `match stmt`（L206 起）之间，
  一句 `if let Some(out) = turso::route(shared, sess, &stmt).await? { return Ok(out); }`
  即可同时覆盖 COM_QUERY 与 prepared 两条路径，且打点/替换语义不变。

## 2. 值类型：`Value` / `SqlType`

文件 `src/sql/storage/schema.rs`（目录编解码在 `row.rs`，目录装载在 `catalog.rs`）。

- `SqlType` L19-41：`Bool | Int | Double | Decimal{precision,scale} | Date | DateTime |
  VarChar | Blob | Vector{dim: u32}`；`MAX_DECIMAL_SCALE = 38`（L14）。
- `Value` L50-67（`ExecOutcome::Rows` 的 cell 类型，serde 可序列化）：
  ```rust
  pub enum Value { Null, Bool(bool), Int(i64), Double(f64),
      Decimal(i128, u8), Date(i64), DateTime(i64),
      Str(String), Bytes(Vec<u8>), Vector(Vec<f32>) }
  ```
  `PartialEq` 手写（L69-86，Vector 按位比较）；`Value::sql_type()` L123-141 推断列型。
- `Engine` L227-231：`Row`（默认）| `Columnar`，`ENGINE=columnar` 表选项在
  `parse/translate.rs::table_engine` L699-716 解析。turso 若复用 `Engine` 或新增
  变体，这是表级"引擎"的既有先例。
- 结论：**`Value` 已有 `Bytes` 与 `Vector` 变体**，turso 侧 BLOB/向量结果可无损映射。

## 3. 错误通道：`SqlError` → MySQL errno

文件 `src/sql/parse/error.rs`（130 行）。

- `SqlError { code: ErrorCode, msg: String }` L8-11；`SqlResult<T>` L130。
  **没有携带显式 errno 的变体**——errno 由 `SqlError::kind()` L78-113 集中映射到
  opensrv 的 `ErrorKind`，前端统一 `results.error(e.kind(), e.msg.as_bytes())`
  （shim.rs `write_outcome` L417、`on_prepare` L222、`on_execute` L282）。
- `ErrorCode` 变体（L14-48）→ 前端 errno（`kind()` L78-113，MySQL 值注于注释）：
  `Parse`→1064、`NoSuchTable`→1146、`TableExists`→1050、`BadField`→1054、
  `DupEntry`→1062、`BadNull`→1048、`WrongValueCount`→1136、`NotSupported`→**1235**
  （ER_NOT_SUPPORTED_YET）、`AccessDenied`→1045、`NodeUnreachable`→1027、
  `UnknownSavepoint`→1305、`LockWaitTimeout`→1205、`WrongAutoKey`→1075、
  `WrongValue`→1292、`WrongParamCount`→1582、`WriteConflict`→1213、
  `TxnDdl`→**1235**、`Unknown`→HY000。
- 惯用构造器 `SqlError::unsupported(msg)` L64-69：
  ```rust
  ErrorCode::NotSupported, format!("not supported in this version: {}", msg.into())
  ```
- **1235 先例（引擎互斥拒绝）**：`src/sql/tx/session.rs::check_engine` L209-222——
  每事务只碰一个引擎，第二个引擎的写被拒：
  ```rust
  Some(m) => Err(SqlError::new(ErrorCode::NotSupported, format!(
      "a transaction cannot mix row-store and columnar writes \
       (already staged a {} write)", ...)))
  ```
  另一处：`exec/write.rs` L84-90 集群模式拒 ODKU/REPLACE（同为 1235，
  消息带决策号 "m2-dml-conflicts decision 1b"）；`parse/starrocks.rs` L51 有
  `StarRocks {what}` 辅助函数。turso/rocks 跨引擎拒绝沿用此风格即可。

## 4. USE 与数据库模型（单库模型）

- 常量：`exec::DEFAULT_DB = "rdb"`（exec/mod.rs L147）。
- 前端校验：`front/shim.rs::is_known_db` L151-152：
  ```rust
  fn is_known_db(db: &str) -> bool { db.eq_ignore_ascii_case(exec::DEFAULT_DB) }
  ```
- **COM_INIT_DB 与文本 `USE x` 同走 `on_init`**（shim.rs L299-314）：opensrv 侧由
  `intermediary_options()` L157-166 的 `process_use_statement_on_query: false` 触发
  （opensrv-mysql 0.7.0 `src/lib.rs` L602-613：COM_QUERY 以 `USE `/`use ` 开头即截出
 库名、去 `;` 与反引号后调 `on_init`）。未知库 → `ErrorKind::ER_BAD_DB_ERROR`（1049）
  `"Unknown database '{db}'"`；已知 → `sess.db = db`。
- prepared 的 USE 在 `on_prepare` L226-237 直接拒 1235
  （"USE is not supported in the prepared statement protocol"），防止绕过 1049 校验。
- **执行器臂（exec/mod.rs L257-260）无条件赋值、不复核**——因前端已把 USE 全量
  引流到 on_init，该臂实际是防御/进程内调用路径。turso 引入后"USE 即引擎路由"需要
  同时扩 `is_known_db`（或其替代的已知库集合）与 on_init 的赋值语义（§13）。
- 事务中 USE：**任何一层都不拒绝**（on_init 与 exec 臂均不看 `sess.txn`）。
- SHOW DATABASES 输出：`exec/show.rs::show_databases` L70-81——`[DEFAULT_DB]` 加上
  与其不同的 `sess.db`；`show_tables` L172-175 用 `sess.db` 拼列名 `Tables_in_{db}`。
- **无 CREATE/DROP DATABASE 支持**：`parse/ast.rs` 的 `Statement` 枚举（L9 起）
  无对应变体；`parse/translate.rs::translate` L423-515 无 CreateDatabase 臂
  （落进兜底 L514 `other => Err(SqlError::unsupported(format!("{other}")))`）；
  DROP DATABASE 走 `parse/translate_ddl.rs::translate_drop` L27-55 的
  `other => Err(SqlError::unsupported(format!("DROP {other}")))`（L53）。
- 限定名整体被拒：`translate.rs::object_name` L406-421 只接受单段名，
  `"qualified name '{name}' (single-part names only)"`——`turso.t` 形式的跨库表引用
  现在就在解析层被拒（turso 计划需明确是否放开）。

## 5. 解析器：sqlparser 0.62 → 自有 `Statement`

- 版本：`Cargo.toml` L71 `sqlparser = { version = "=0.62.0", optional = true }`。
- 入口 `parse/translate.rs::parse_statement` L24-40：starrocks 预解析 →
  `vector_ddl::rewrite` → `trim_default::expand` → `Parser::parse_sql(&MySqlDialect{})`
  → 恰一条语句 → `translate(stmts.next(), model)`。**白名单就是 `translate()` 的
  match 臂**，未覆盖的 sqlparser 语句统一 1235（L514）。
- sqlparser 0.62 的 CREATE DATABASE 节点（`~/.cargo/.../sqlparser-0.62.0/src/ast/mod.rs`
  L4378-4401）：`Statement::CreateDatabase { db_name: ObjectName, if_not_exists: bool,
  location, managed_location, or_replace, transient, clone,
  data_retention_time_in_days, max_data_extension_time_in_days, external_volume,
  catalog }`——**Snowflake 形状，没有 engine 字段**；MySQL 的
  `[DEFAULT] CHARACTER SET/CHARSET [=] x` 与 `COLLATE` 会被解析吃掉
  （`parser/mod.rs::parse_create_database` L5486-5530，无方言门控，MySQL 方言直接可用）。
  DROP DATABASE 到达形式为 `Statement::Drop { object_type: ObjectType::Database, .. }`
  （ObjectType Display "DATABASE"，ast/mod.rs L8499；解析分派 parser/mod.rs L7374）。
- 新增 `CreateDatabase`/`DropDatabase` 需要动的文件：
  `parse/ast.rs`（Statement 枚举加变体）、`parse/translate.rs`（translate 加臂）、
  `parse/translate_ddl.rs`（translate_drop 的 match 加 `ObjectType::Database` 臂）、
  `exec/mod.rs::dispatch`（执行臂 + L188 事务内 DDL 清单）、测试
  `parse/mod.rs` L30 起（`fn stmt` L34）与 `translate_ddl_tests.rs`。
  （`ENGINE=` 选项无处安放——如需 `CREATE DATABASE ... ENGINE=turso` 语义，要么用
  `vector_ddl.rs` 式预解析改写（MARKER 先例，L57），要么纯靠库名约定/配置。）

## 6. 配置管道：Config → Shared → SQL 层

- `src/conf.rs::Config` L18 起，全字段 `#[serde(default, rename = "...")]`：
  `mysql_bind` L43-44、`mysql_user`/`mysql_password` L46-49。**尚无 turso 键**。
- 消费点：`src/main.rs` L643-649 `if !conf.mysql_bind.is_empty()` →
  `sql::front::bind(&conf.mysql_bind)` + `tokio::spawn(sql::front::serve(listener, Arc<Shared>))`；
  `front/serve.rs::serve` L44-59 从 `shared.conf.mysql_user/mysql_password` 取凭据。
- `Shared`（`src/state.rs` L320-346）**第一个字段即 `pub conf: conf::Config`**——SQL 层
  全部经 `shared.conf.*` 读配置；组装点 `src/main.rs` L570（normal）/L502（backup）、
  测试 `state.rs::testutil::shared_with` L356。
- **已就绪的一半**：`Cargo.toml` 已有 `turso-engine = ["dep:turso"]` feature（L42）与
  `turso = { version = "=0.9.0-pre.2", optional = true }`（L75），注释明言
  "Runtime-gated by conf `turso_engine` + `turso_db_dir` … R1 in plans/2026-11-09-turso-engine"
  （Cargo.lock 已锁定 0.9.0-pre.2）。缺的只是 conf.rs 两个键 + 初始化接线：
  新键 `turso_engine: bool`、`turso_db_dir: String`（serde default）→ main.rs 启动时
  初始化（与 mysql_bind 块并排）或 SQL 层惰性读 `shared.conf`。
- e2e 写 yaml：`tests/common/mod.rs::write_config` L132-152——模板字符串拼出
  `mysql_bind: "{addr}"\nmysql_user: "root"\nmysql_password: "e2e-sql-pass"\n` +
  `extra` 原样追加行（lite 段先例）；`spawn_node_mysql` L257-303 分配五个自由端口
  并写 `node_dir/conf.yaml`。turso 配置经 `extra` 注入零改动可用。

## 7. 前端流转：shim / conv / conv_bin

`src/sql/front/shim.rs`（488 行）：

- `on_query` L196-212 → `run_statement` L330-354：先 `vars::parse_sysvar_query` 拦
  `SELECT @@...`（vars.rs L63 `SessionVars{isolation}`、L68 `sysvar_value` 25 项表，
  `transaction_isolation|tx_isolation` L89-94 回显 `sess.isolation` 或
  "REPEATABLE-READ"），再 `parse_statement` → `exec::execute`。
- prepared：`on_prepare` L214-251 解析后存 `self.prepared: HashMap<u32, Statement>`
  （L47），参数列由 `conv::placeholder_columns(placeholder_count(&stmt))` 生成
  （全 VAR_STRING，conv.rs L204-210）；`on_execute` L253-293 逐参
  `conv::param_to_value`（conv.rs L47-55，NEWDECIMAL 文本→Str、二进制→Bytes）→
  `parse::bind_placeholders(&mut stmt, &values)` → `exec::execute` →
  `write_outcome(..., binary=true)`；`on_close` L295-297 移除。
- `write_outcome` L375-417（文本/二进制共用）：`Ok`→OkResponse（带 last_insert_id）、
  `Affected(n)`→affected_rows、`Rows`→`conv::colmetas_to_columns(&columns)`
  （conv.rs L188-201：`!nullable`→NOT_NULL_FLAG、`primary`→PRI_KEY_FLAG；
  `sql_type_to_mysql` conv.rs L142-158，**`Vector{..} → MYSQL_TYPE_BLOB`**）→
  二进制路径先 `conv_bin::preflight_binary`（错配响亮回 1292）→ 逐 cell
  `conv_bin::write_value`（conv_bin.rs L214-220，`w.write_col(Cell(v))`）。
- cell 编码：`conv_bin.rs::Cell::to_mysql_text` L38-57——`Str` 走 opensrv 文本、
  `Bytes` 走字节 slice、**`Vector(v) => storage::vector::format_vector(v).to_mysql_text(w)`
  （'[e,e,...]' 文本）**；二进制路径对 string-ish 列统一规范文本（`text_of` L185
  同样把 Vector 转 '[..]'）。即：向量在两种协议都是文本拼写（偏差台账 (b)）。

## 8. 既有向量面（rocks 引擎）——turso 对齐测试的镜像

- 类型解析：`parse/translate_type.rs` L93-120（custom_type L96）——sqlparser 把未知
  类型 `VECTOR(N)` 交给 custom 类型名，维度校验 `1..=MAX_VECTOR_DIM(4096)`
  （`storage/vector.rs` L17）。
- 函数实现：`exec/func/vector.rs`（eval 分发在 `exec/func/mod.rs` L45
  `.or_else(|| vector::eval(name, args))`）——`Metric` L26（Cosine/Euclidean/Dot）、
  `parse_metric` L34-46、`STRING_TO_VECTOR`/`TO_VECTOR` L102、
  `VECTOR_TO_STRING`/`VEC_TO_TEXT` L115、`VECTOR_DIM` L127、值级 `distance` L150。
  **注意有两个 Metric 枚举**：`sql/index/vector.rs::Metric` L31（ANN 索引用，
  `ast::Statement::CreateVectorIndex` 的字段，ast.rs L45）与 func 侧这份，
  后者经 `func/mod.rs` L35 re-export。
- KNN 模式：`exec/select.rs` L195-203——普通投影展开后在
  `if let Some(out) = vector_knn::try_execute(q, src, &items)? { return Ok(out); }`
  抢跑精确 top-k（形状识别 `vector_knn::match_knn` L59、执行 L124）；
  ANN 路径 `exec/vector_ann.rs`。
- ANN DDL：`parse/vector_ddl.rs` 预解析改写（MARKER `"vec_ann"` L57，
  `CREATE VECTOR INDEX ... WITH (METRIC '..', NPROBE n)` → 标记函数形式的
  CREATE INDEX，translate_ddl 再提升回 `Statement::CreateVectorIndex`）。
- 既有 e2e（对齐模板）：`tests/sql_vector_e2e.rs`
  （`vector_surface_end_to_end` L17：DDL/DESCRIBE 元数据/插入强制/函数往返/KNN/错误矩阵；
  `vector_ann_index_end_to_end` L145）；单元：`exec/ddl_vector_tests.rs`、
  `storage/row_vector_tests.rs`、`index/vector_tests.rs`。

## 9. 事务：`Txn` 与会话

- `src/sql/tx/session.rs::Txn` L66-82：`read_ts: u64`、`id: u64`、
  `writes: BTreeMap<TxnKey, TxnWrite>`、`appends: BTreeMap<String, Vec<Vec<Value>>>`
  （列存暂存）、**`mode: Option<Engine>`**（首写引擎，跨引擎即拒，见 §3）、
  `savepoints: Vec<Savepoint>`。`begin` L86（钉快照+注册）。
- staging 咽喉：`stage_append` L103 / `stage_upsert` L116 / `stage_delete` L127
  （L117-118 等处 `check_engine(txn, Engine::Row)?; txn.mode = Some(Engine::Row)`）。
- `sess.txn` 的使用：`exec::dispatch` Begin/Commit/Rollback/SAVEPOINT 族直接驱动；
  写路径合并自身写（`tx::merge_rows` session.rs L225）；断连回滚由连接 runner 经
  `session_handle()`（shim.rs L143）清理。
- USE 期间事务：不拒绝（§4）。隔离级回显：`vars.rs` `transaction_isolation`
  （L89-94）+ exec `SetIsolation` L262-267。

## 10. 测试基建：`tests/common/mysql.rs`

- API（mysql_async 客户端）：`connect` L38（带重试）/`connect_root` L66（root +
  `PASS="e2e-sql-pass"` L19）、`ddl` L89 与 `run` L95（leader 重试）、
  `rows` L101 / `one` L121 / `col` L130 / `col_types` L146 / `wait_table` L154、
  `server_error` L178 / `aff` L185、值构造 `s/int/i` L193-203、`assert_exprs` L259、
  单节点夹具 `world(tag)` L268 / `funcs_node(tag)` L283。errno 常量 L27-44
  （含 `ER_NOT_SUPPORTED_YET = 1235` L29）。`ProcNode.mysql` 字段
  （`tests/common/mod.rs` L59）承载前端地址。
- 代表性 e2e 模板：
  - `tests/sql_e2e.rs`——握手/DDL/DML/SELECT 全链、并发建表互不撞号；
  - `tests/sql_vector_e2e.rs`——向量面全链（turso 对齐测试的最佳模板，见 §8）；
  - `tests/mysql_compat_e2e.rs`——MySQL 语义回归（三值逻辑/集合操作/拒绝矩阵，
    M5 起走 common::mysql）；另有 `tests/txn_semantics_e2e.rs`
    （SAVEPOINT 可见性 / @@transaction_isolation / 锁读冲突）。
- 多节点：`tests/common/mod.rs::start_sql_cluster` L768。

## 11. Raft 目录：表 schema 如何复制（库元数据放哪）

`src/sql/storage/catalog.rs`：

- key 空间：`CATALOG_PREFIX = "sql_catalog/"` L25；`catalog_key(table)` L40 →
  `sql_catalog/<table>`（value 为 TableSchema JSON）；`sequence_key` L44 →
  `sql_sequence/`（AUTO_INCREMENT 计数，**任意 KV 复制条目的现成先例**）；
  `dropped_id_key` L48 → `sql_dropped/<id>`。
- 写事务：`CatalogTxn` L77-137 —— `queue_put` L103（schema upsert）、
  `queue_drop` L116（墓碑+id 标记）、**`queue_put_kv(key, value)` L124-127**
  （任意 FSM 条目：`RaftLogEntryData{key,value}` + apply ticket）；入口
  `catalog::begin(raft, what)` L149-161 **leader-only**（非 leader 报
  "requires the raft leader"）。读取：`lookup` L162 / `list_tables` L197 /
  `lookup_state` L177（FSM 直读，follower 可见）。
- DDL 全流程：`exec/ddl.rs::run` L43 起，`spawn_blocking` + `shared.raft.write()`
  守卫内做全部决策，守卫释放后才 await 提交（文件头注释 L1-25 详述约束）。
- **库存在性元数据的两个候选**：
  1. raft 目录元数据条目（如 `sql_db/<name>`）：复用 `begin + queue_put_kv`，
     follower 经 `lookup_state` 同步可见，与 `sql_sequence/` 同构；代价 =
     CREATE/DROP DATABASE 变成 leader-only DDL（本就与表 DDL 一致，需加进
     exec/mod.rs L188 的事务内 DDL 拒绝清单）。
  2. 纯本地注册表（启动扫 `turso_db_dir` + 配置）：更便宜，但 follower 上
     CREATE DATABASE 会与 SHOW DATABASES/USE 视图漂移。
  两者都符合代码库习惯（列存 segment 注册表本就是每节点本地的，见 ddl.rs 头注释）；
  推荐 1（增量小、集群一致），单机部署下二者等价。

## 12. 文档回写约定（只读摘要，不改原文）

- `agents/rust/sql.md`：首行 `Commit: <hash>`，`# rdb/sql（…）` 标题，
  小节固定为 `## 职责` / `## 模块地图`（按目录逐条、高密度中文长句 +
  反引号路径）/ `## 关键不变量` / `## 测试地图`（按 e2e 文件点名）。
  turso 落地应新增模块地图条目（如 `sql/turso/`）与测试地图行。
- `COMPAT.sql.md`：**英文**。头部指向 module map / feature summary / plans；
  正文 `## SQL data plane (MySQL wire, Rust-only)` 以 `- **主题** — 事实` 的
  bullet 组织（USE/1049/1235 的现行契约就在该节）；专项能力用独立
  `## …capability & deviations` 节 + `### deviation ledger (a)–(j)` 编号台账
  （向量面即此形态，`plans/2026-11-02-turso-ref/` 为决策出处）。turso 的契约与
  偏差应新增同风格节/条目，编号台账延续字母序。
- `plans/index.md` 规范：文档 `<400` 行、头部 `状态：`/`日期：`（总览另含 `关联：`）、
  `NN-topic.md` 编号、路径反引号、不编造行号。

## 13. 建议切口（最小 hook 清单）

1. **引擎分发**：`src/sql/exec/mod.rs::dispatch` L204-206 之间插一句路由
   （`turso::route(shared, sess, &stmt)` 返回 `Option<ExecOutcome>`，None 落回原
   match）；`#[cfg(feature = "turso-engine")]` + `shared.conf.turso_engine` 双闸。
   保留 execute() 的替换与打点，COM_QUERY/prepared 双路径同时生效。
2. **USE 路由**：`front/shim.rs::is_known_db` L151 改为"已知库集合"（DEFAULT_DB ∪
   turso 注册表/目录扫描）；`on_init` L307-312 依目标库设置 `sess.db`（引擎选择
   派生自 db 名或注册表）；`exec` Use 臂 L257 保持兜底；SHOW DATABASES 在
   `exec/show.rs::show_databases` L70-81 并入 turso 库集。
3. **CREATE/DROP DATABASE**：sqlparser 已能解析（§5），新增
   ast.rs 变体 + translate.rs 臂 + translate_ddl.rs `ObjectType::Database` 臂 +
   exec 执行臂（走 `catalog::begin + queue_put_kv("sql_db/<name>", …)`，leader-only；
   同步加进 exec/mod.rs L188 事务内 DDL 拒绝清单）。若需 `ENGINE=turso` 语法，
   仿 `parse/vector_ddl.rs` 预解析改写（sqlparser 的 CreateDatabase 无 engine 字段）。
4. **配置键**：`src/conf.rs::Config` 加 `turso_engine: bool` / `turso_db_dir: String`
   （serde default + rename）；消费走 `shared.conf`（state.rs L321）；初始化接线在
   `src/main.rs` L643 mysql_bind 块旁。Cargo.toml 的 feature/依赖已在（L42/L75）。
5. **错误映射**：不支持项一律 `SqlError::unsupported(...)`（1235 +
   "not supported in this version: …"）；跨引擎事务拒绝镜像
   `tx/session.rs::check_engine` L209 的措辞（"a transaction cannot mix …"）；
   新错误语义优先扩 `ErrorCode` + `kind()`，不另开 errno 通道。
6. **e2e 模板**：新套件仿 `tests/sql_vector_e2e.rs`（`spawn_node_mysql` +
   `wait_resp_ready` + `wait_mysql_ready` + `mysql_root_conn(&node, PASS)`）；
   turso 配置经 `tests/common/mod.rs::write_config` 的 `extra` 参数注入 yaml 行；
   errno 断言用 `common::mysql::ER_NOT_SUPPORTED_YET` 等常量。

### 调查中值得注意的"意外"（可能影响计划）

- `Value` 已有 `Bytes` 与 `Vector` 变体；Vector 在两种协议下都以 '[..]' 文本上线路
  （列型 BLOB），turso 向量结果可直接复用该拼写。
- USE 校验只在前端 `on_init`（文本 USE 也被 opensrv 引流过去）；exec 的
  `Statement::Use` 臂无条件赋值，事务中 USE 任何一层都不拒。
- `object_name` 只收单段名：`db.table` 限定名现即在解析层 1235。
- Cargo.toml 已引用本计划（R1）声明 feature/依赖，conf 键尚未存在。
- 存在两个 `Metric` 枚举（index 侧与 func 侧），命名时勿混淆。
