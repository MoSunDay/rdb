# 2026-11-09 turso-engine：turso crate 0.9.0-pre.2 嵌入 API 研究（00-research）

> 状态：proposed
> 日期：2026-11-09
> 对象：`turso = "=0.9.0-pre.2"`（源码 `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/turso-0.9.0-pre.2/`，下称 `turso/`；核心 `turso_core-0.9.0-pre.2/`，下称 `core/`；SDK 层 `turso_sdk_kit-0.9.0-pre.2/src/rsapi.rs`，下称 `rsapi`）
> 方法：只读源码调查 + 独立冒烟二进制（`/tmp/tsmoke`，同版本 crate）实测，凡标 **[冒烟]** 均为运行验证
> 关联：`00-rdb-context.md`（rdb 侧落点）；本文件只覆盖 turso crate 侧

## ① 生命周期：Builder / Database / Connection

```rust
// turso/src/lib.rs
pub fn new_local(path: &str) -> Self                                  // :186
pub fn read_only(mut self, read_only: bool) -> Self                   // :282
pub fn with_io(mut self, vfs: impl Into<IoBackend>) -> Self           // :270  ("memory"|"syscall"|"io_uring")
pub async fn build(self) -> Result<Database>                          // :324
// turso/src/lib.rs:371
pub fn connect(&self) -> Result<Connection>                           // Database::connect
```

- **多连接**：允许。`Database` 是 `Arc` 内核的 `Clone`（lib.rs:359-364）；每次 `connect()`
  都新建一条核心连接（rsapi:1070 `TursoDatabase::connect` → `db.connect(ConnectOpts)`），
  无内建池化（turso/src/connection.rs:52-56 注释提到的 ConnectionPool 仅为设想）。
- **Send/Sync**：`assert_send_sync!(Connection)`（connection.rs:71），`Statement`/`Rows`
  同样断言；跨任务共享安全。
- **tokio**：**不依赖**。turso crate 仅 `sync` 特性才引入 tokio/hyper（turso/Cargo.toml features）；
  所有 API 是 executor 无关的 `Future`（IO 由 `TursoStatusCode::Io` + waker 驱动，
  `Statement::step` lib.rs:398-412 内联 `run_io()`）。**[冒烟]** 用
  `futures::executor::block_on` 跑通全部用例（含文件 IO），rdb 现有 tokio runtime 无缝兼容。
- 并发门闩：同连接上普通 query 为 shared 操作、`execute_batch`/`batch` 为 exclusive
  （connection.rs:527 `ConnectionOperationGate`）；跨连接并发读写见 ④。
- 其他连接级 API：`busy_timeout(Duration)` connection.rs:514（1ms→2ms→…≤100ms 分段退避）、
  `last_insert_rowid()` :469、`is_autocommit()` :495。

## ② query / execute / 参数 / Row / 预编译

```rust
// turso/src/connection.rs（Connection，全部 &self）
pub async fn query(&self, sql: impl AsRef<str>, params: impl IntoParams) -> Result<Rows>   // :124
pub async fn execute(&self, sql: impl AsRef<str>, params: impl IntoParams) -> Result<u64>  // :131（返回受影响行数）
pub async fn execute_batch(&self, sql: impl AsRef<str>) -> Result<()>                       // :146（多语句脚本）
pub async fn prepare(&self, sql: impl AsRef<str>) -> Result<Statement>                      // :354
pub async fn prepare_cached(&self, sql: impl AsRef<str>) -> Result<Statement>               // :367
// turso/src/lib.rs（Statement，Clone；query/execute 需 &mut self）
pub async fn query(&mut self, params: impl IntoParams) -> Result<Rows>     // :434
pub async fn execute(&mut self, params: impl IntoParams) -> Result<u64>    // :475
pub async fn query_row(&mut self, params: impl IntoParams) -> Result<Row>  // :606
pub fn column_count/column_name/column_names/column_index/columns(&self)   // :523-571
pub fn reset(&self) -> Result<()> / pub fn n_change(&self) -> u64          // :590 / :597
```

**prepare-then-bind 存在**：`prepare` 后可对同一 `Statement` 反复 `query/execute(新参数)`
——内部先 `reset()` 再 `bind_positional`（lib.rs:455-471 `query_with_operation_guard`），
无需手动解绑。**[冒烟]** 同一 stmt 依次绑 `(10,32)`→42、`(1,1)`→2 验证通过。

- 参数类型 `IntoParams`（params.rs:136-216）：`()`、`Vec<T>`、`Vec<(String,T)>`、
  `[T; N]`（位置）、`[(&str,T); N]`（命名）、≤16 元组、`params_from_iter`；
  宏 `params![]` / `named_params!{}`（params.rs:285-310）。命名键须带 SQL 前缀 `:k`/`@k`/`$k`/`?1`。
- **Value 枚举**（value.rs:4-10，即 `turso::Value`，pub use lib.rs:53）：
  ```rust
  pub enum Value { Null, Integer(i64), Real(f64), Text(String), Blob(Vec<u8>) }
  ```
  `IntoValue` 对一切 `TryInto<Value>` blanket 实现（params.rs:265-272）；`From` 覆盖
  i8..u32/i64/f32/f64/bool/&str/String/&[u8]/Vec<u8>/Option<T>（value.rs:139-247）。
- Row API：`Rows::next() -> Result<Option<Row>>`（rows.rs:47）、
  `Row::get_value(idx) -> Result<Value>`（rows.rs:86）、`Row::get::<T: FromValue>(idx)`（:108）、
  `Rows::column_names()`（:32）。列名来自语句元数据，`Column{name, decl_type}`（lib.rs:619）。

## ③ 只读打开

- **机制是 Builder 标志，不是 URI**：`Builder::read_only(true)` → `OpenFlags::ReadOnly`
  （lib.rs:330-334；core io/mod.rs:556 bitflags）。本嵌入 API **不解析** `?mode=ro`
  （仅 `:memory:`/`file::memory:` 前缀被识别为内存库，core database.rs:313-314）。
- **[冒烟]** 文件库 ro 打开后：读 OK；写 → `Error::Readonly("database is readonly")`；
  ro 打开不存在文件 → `Error::IoError(NotFound, "open")`（不会创建）。

## ④ 事务

- **字符串直通可用**：`conn.execute("BEGIN"/"COMMIT"/"ROLLBACK", ())` **[冒烟]** 验证；
  默认 autocommit（`is_autocommit()==true`，显式 BEGIN 后变 false）。
- Rust 侧托管 API 也有：`conn.transaction(&mut self) -> Transaction`（transaction.rs:272，
  需 `&mut`）、`unchecked_transaction(&self)`（:324）、
  `transaction_with_behavior(TransactionBehavior)`（:285）；`Transaction::commit/rollback/finish`
  （:165/:178/:195），`DropBehavior`（:39，默认 Rollback）。
- `TransactionBehavior::{Deferred,Immediate,Exclusive,Concurrent}` →
  `"BEGIN DEFERRED|IMMEDIATE|EXCLUSIVE|CONCURRENT"`（:17-27）。
  **BEGIN CONCURRENT 需 `PRAGMA journal_mode=mvcc`**（默认 journal_mode 为 `wal`；
  **[冒烟]** 非 mvcc 下报 `"Concurrent transaction mode is only supported when MVCC is enabled"`，
  切 mvcc 后成功）。另：事务内嵌套 BEGIN → `"cannot start a transaction within a transaction"`。
- **并发写模型**：单写者。**[冒烟]** 连接 A `BEGIN IMMEDIATE` 持写锁后，连接 B
  `BEGIN IMMEDIATE`/写语句 → `Error::Busy("database is locked")`；读连接不受影响。
- ⚠️ **快照粘滞（关键运维规则）**：**[冒烟]** 未把 `Rows` 抽干（`next()` 到 `None`）就丢弃，
  该连接停留在打开的读事务里，**后续查询看不到其他连接已提交的数据**（count 一直返回旧值）；
  抽干后恢复正常（新 commit 可见）。`query_row` 文档（lib.rs:606-610）明言要"执行到完成"。
  封装层必须保证每条查询的 Rows 被 drain 或显式 reset。

## ⑤ 向量函数（全内置，无需开关）

注册表 core/dialect/sqlite/mod.rs:514-526 + core/function.rs:387-456（`VectorFunc`，
arities :430-456）：

| SQL 函数 | 元数 | 语义/返回 |
|---|---|---|
| `vector(x)` / `vector32(x)` | 1 | 同一实现（vdbe/execute.rs:11770-11775），= Float32Dense 序列化 BLOB |
| `vector32_sparse` / `vector64` / `vector8` / `vector1bit` | 1 | 对应类型转换 + 序列化 BLOB（vector/mod.rs:37-95） |
| `vector_extract(v)` | 1 | BLOB → **TEXT** `"[1,2,3]"`（mod.rs:97-118）**[冒烟]** |
| `vector_distance_cos(a,b)` | 2 | `1 − dot/(‖a‖·‖b‖)`（distance_cos.rs:80）；正交=1.0；**同向量≈1.79e-7 而非 0**（simsimd 浮点）**[冒烟]** |
| `vector_distance_l2(a,b)` | 2 | 欧氏距离（distance_l2.rs:55 sqrt）**[冒烟]** [0,0]×[3,4]=5.0 |
| `vector_distance_dot(a,b)` | 2 | **取负的点积**（distance_dot.rs:1bit 路径注释"Return negated" :46-54，f32/f64 路径同）**[冒烟]** [1,2]·[3,4]=**−11**、自身=−5 |
| `vector_distance_jaccard(a,b)` | 2 | 第三种距离：稀疏/1bit 向量集合距离（jaccard.rs；同向量=0）**[冒烟]** |
| `vector_concat(a,b)` / `vector_slice(v,s,e)` | −1/3 | 拼接/切片（mod.rs:182-210） |

- 入参：TEXT JSON 数组 `'[1,2,3]'` 或 BLOB（`parse_vector` mod.rs:13-34）。
- **存储/返回形态 = `Value::Blob(Vec<u8>)`**（serialize.rs:9-55；vector_types.rs:43-60 奇偶判型）：
  - Float32Dense：**裸 LE f32×N，无类型尾字节**（偶长度即可判型）**[冒烟]** `[1,2,3]` → 12 字节
  - Float64Dense：数据 + 尾字节 `0x02`；1bit：`[数据][trailing_bits][0x03]`；
    f8：`[量化][alpha f32][shift f32][trailing][0x04]`；sparse：数据 + `dims u32 LE` + `0x09`
  - 与 libSQL vectorInt.h 布局兼容（vector_types.rs:49-58）。
- 维度不匹配 → `"Vectors must have the same dimensions"`；类型不匹配 →
  `"...same type"`（ConversionError → `Error::Error`）。距离函数返回 `Value::Real(f64)`。

## ⑥ 嵌入 API 的 UDF 注册：**不可用**

- `turso::Connection` **没有** `create_function`/register 类方法（connection.rs 公开方法清单
  :124-514 全查无）；其内部 `TursoConnection` 为私有字段，无法下钻。
- 底层存在但够不着：`turso_sdk_kit::rsapi::TursoConnection::register_external_scalar_function`
  （rsapi:1212，**C ABI 裸函数指针** `ScalarFunction` 回调 + argc/deterministic/析构钩子），
  仅面向 C API/扩展，嵌入路径不可达。`turso_ext::functions::ScalarFunc` trait 同理（扩展框架）。
- 结论：VECTOR_DIM 校验走应用侧（见 D6）。

## ⑦ 错误类型与分类

`turso::Error`（lib.rs:88-137，thiserror）：`ToSqlConversionFailure` / `QueryReturnedNoRows` /
`ConversionFailure` / `Busy(String)` / `BusySnapshot` / `Interrupt` / `Error(String)` /
`Misuse` / `Constraint(String)` / `Readonly` / `DatabaseFull` / `NotAdb` / `Corrupt` /
`IoError(io::ErrorKind, &'static str)` / `Batch*`。上游映射 `From<LimboError>`
（rsapi:733-763）。**[冒烟]** 实测各场景：

| 场景 | 变体 + 消息 | 判定方式 |
|---|---|---|
| 表已存在 | `Error("Parse error: table t already exists")`（schema.rs:3835） | 字符串 contains `"already exists"` |
| 表不存在 | `Error("Parse error: no such table: missing")` | contains `"no such table"` |
| 唯一约束 | **`Constraint("UNIQUE constraint failed: u.a")`**（error.rs:455→rsapi:736） | 变体匹配 |
| 语法错 | `Error("near \"SELEC\": syntax error")`（ParseError） | contains `"syntax error"` |
| 写锁/忙 | **`Busy("database is locked")`**（LimboError::Busy） | 变体匹配 |
| 只读写 | `Readonly("database is readonly")` | 变体匹配 |
| 文件不存在(ro) | `IoError(NotFound, "open")` | 变体匹配 |

外键/约束族统一进 `Constraint`；未特判的 LimboError 全部落 `Error(String)` → 需字符串兜底。

## ⑧ 内存库 / 文件库 / 建库行为

- 内存库：路径 `":memory:"`（或 `":memory:名字"` 前缀）、`"file::memory:"`、空串
  （core database.rs:313-314 `is_memory_like`）；同 Database 多连接共享同一内存库 **[冒烟]**。
- 文件库：`OpenFlags::default() == Create`（io/mod.rs:561-565）→ **打开不存在文件即创建**
  （**[冒烟]** 新路径直接建库建表成功）；`read_only(true)` 时不创建（③）。
- VFS 可换：`with_io("memory"|"syscall"|"io_uring")`（lib.rs:270；rsapi TursoDatabaseConfig.vfs 注释）。

## ⑨ 编译墙时与重量级传递依赖

- 冷缓存 `cargo check --features turso-engine`（含 rdb 全栈）**5m47s**（dev profile，本次机器；
  后台全量跑完 `Finished dev [unoptimized + debuginfo] target(s) in 5m 47s`）。
- turso 子树 normal 依赖 **240 crate**。重量级传递依赖（rdb Cargo.lock 实录）：
  `simsimd 6.5.16`、`zstd/zstd-sys`、`lz4-sys`、`bzip2-sys`（压缩/WAL）、`rayon`+`crossbeam-*`、
  `roaring`、`io-uring`、`libmimalloc-sys`。
- ⚠️ **默认特性 = `["mimalloc","fts"]`，且 turso crate 自身在 mimalloc 特性下声明
  `#[global_allocator]`（turso/src/lib.rs:43-46）**：按当前依赖行引入会把**整个 rdb 进程**
  的分配器换成 mimalloc（rdb 自身未声明 global_allocator，故可链接通过、无报错）。
  **[冒烟]** `default-features = false`（去掉 mimalloc+fts）编译并运行全绿，见 D1。

## 拍板 D1–D8

- **D1 版本 pin 与升级策略**：维持 `=0.9.0-pre.2` 精确 pin（pre-1.0 API 高频变动，升级须专项
  mini-plan + 重跑本文件冒烟集）。**建议 T1 起改用已验证的
  `turso = { version = "=0.9.0-pre.2", default-features = false, optional = true }**
  （消除进程级 mimalloc 分配器接管与 C 依赖 libmimalloc-sys；FTS 若后续要再用
  `features = ["fts"]` 显式加回）。本任务未改 Cargo.toml，两条依赖行均验证可编译。
- **D2 按 db 文件路由可行**：一个 turso db = 一个文件路径 = 一个 `Database`（Arc+Clone），
  多 `connect()` 并发读写（单写多读）**[冒烟]**；按 `sess.db` 名映射 `<turso_db_dir>/<name>.db`
  建/开库即可。两个硬约束：每查询 Rows 必须 drain（④快照粘滞）；写冲突按 `Error::Busy`
  分类返回。连接建议每会话一连接（无内建池），`Database` 句柄进程级缓存。
- **D3 raft-catalog 仅登记元数据**：turso 引擎内部没有 CREATE DATABASE/实例概念（一个
  Database=一个路径），库名只存在于 rdb catalog（name→路径/引擎标记），元数据走 raft 复制、
  文件本体本地——与 rdb 现有 catalog 模式一致，无引擎侧阻碍。
- **D4 跨引擎拒绝 1235**：引擎无跨库查询能力（ATTACH 为 `experimental_attach(false)` 默认关，
  lib.rs:220），rdb 原生库与 turso 库、turso 库之间同语句混查一律前置拒绝
  （MySQL 1235 ER_NOT_SUPPORTED_YET），不做桥接。
- **D5 DISTANCE 'dot' 映射**：引擎**有** `vector_distance_dot`，但语义是**取负点积**（⑤，
  同向量=−‖v‖²）——包装为 `-vector_distance_dot(a,b)` 恢复 MySQL VECTOR_DISTANCE('dot')
  的真点积语义；'cos'→`vector_distance_cos`（注意同向量≈1.8e-7≠0，测试留 epsilon）；
  'l2'→`vector_distance_l2`。不引入 jaccard（MySQL 无对应）。
- **D6 VECTOR(N) DDL=BLOB+应用侧校验**：UDF 注册不可达（⑥），列声明映射为引擎 BLOB，
  N 与格式校验在 rdb 包装层做：INSERT/UPDATE 值侧直接以 `Value::Blob(f32 LE ×N)` 绑定
  （偶长无尾字节，⑤），或 SQL 里包 `vector32('[...]')`；校验失败在包装层报 MySQL 错误，
  不依赖引擎约束。检索比较用 ⑤ 的距离函数族。
- **D7 事务字符串直通**：`BEGIN/COMMIT/ROLLBACK` 走 `conn.execute` **[冒烟]** 验证可行，
  与 `SqlSession.txn` 状态机一一对应；不用 Rust `Transaction` API（`&mut self` 借用约束
  与会话共享冲突）。BEGIN CONCURRENT 需 mvcc，默认 journal_mode=wal → **不透传
  CONCURRENT**，统一 BEGIN(DEFERRED)；`"cannot start a transaction within a transaction"`
  与 `Busy` 需按 ⑦ 分类回包。
- **D8 预编译语句=重放 query+params**：MySQL COM_STMT_EXECUTE 侧不复用跨网络语句句柄，
  每次以参数重放：`conn.query(sql, params)`（内部 prepare→bind→step）即可；高频语句可用
  `prepare_cached`。Statement 可 Clone 反复 bind（②，**[冒烟]**），如 T1 引入会话级 stmt
  缓存，务必 drain Rows（④）后再 reset/复用。

## 验证记录

- 最终依赖行（未改动，Cargo.toml:75）：
  `turso = { version = "=0.9.0-pre.2", optional = true }`
  —— **已验证可编译**：`cargo check --features turso-engine` 于 22:26:48 全绿收尾
  （5m47s，含 warning `proc-macro-error2 future-incompat` 一条，与本依赖无关）。
- 备选行（冒烟二进制采用，`cargo run` 全绿）：`default-features = false` 变体，见 D1。
- 注意：22:27 后 `cargo check`（default 与 turso-engine 均）因**并行工作流**对
  `src/search/index_codec/posting.rs` 的未完成修改（E0061，ft_index.rs:237 实参数不符）
  暂时失败，与 turso 依赖无关；turso 侧检查在彼时之前已全量通过。
