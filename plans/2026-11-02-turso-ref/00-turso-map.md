# turso/libsql 架构地图（00-map）

- 参考仓库：`link_repos/turso/`（turso/libsql，pin `d8bc862d20bedc72945a1da27c8b7045414c0261`，只读）
- 本文目的：为向量功能（01/02）与性能优化（03）研究提供可定位的架构地图；只做结构与职责级描述，不深入实现
- 行内代码引用/术语保留英文；rdb 侧路径均为 `/root/rdb` 仓库根相对路径

## 1. 顶层 Cargo workspace

根 `Cargo.toml`：workspace `version = 0.10.0`，`resolver = "3"`，成员见下表；
`exclude` 了 `libsql-sqlite3/ext/crr`、`libsql-sqlite3/ext/libsql-wasi`、`libsql-shell`、`tools/rebuild-log`、`tools/fuzz`（独立构建/独立版本）。
注意 `libsql-sqlite3/` 本身**不是** workspace 成员（C 代码树，由 `libsql-ffi` 的 build.rs 编译）。

| Crate（package 名） | 目录 | 职责（据 Cargo.toml description + 模块布局） |
|---|---|---|
| `libsql` | `libsql/` | 用户侧数据库库：`Database`/`Connection`/`Rows` 抽象与 builder（`database/builder.rs`）。子模块：`local/`（本地 embedded 连接）、`sync/`（embedded replica 离线写同步）、`replication/`（客户端侧复制）、`hrana/`（Hrana 远程协议客户端）、`wasm/`（WASM 运行时桥）、`parser.rs`、`value.rs`、`transaction.rs` |
| `libsql-sys` | `libsql-sys/` | "Native bindings to libSQL"：rusqlite 风格低层 API（`connection.rs`/`statement.rs`/`value.rs`）；关键子目录 `wal/`（`wrapper.rs`/`sqlite3_wal.rs`/`ffi.rs`：把 SQLite 的 WAL 写路径接到 Rust，供 sqld 复制钩住） |
| `libsql-ffi` | `libsql-ffi/` | bindgen 生成/维护的 C FFI；`build.rs` 编译 `bundled/sqlite3.c`（amalgamation，由 `libsql-sqlite3` 生成），`bundled/` 还带 `sqlean/`（扩展集合）与 `SQLite3MultipleCiphers/`（加密页） |
| —（C 树） | `libsql-sqlite3/` | SQLite C fork，见 §2 |
| `libsql-server`（bin `sqld`） | `libsql-server/` | 服务端：Hrana(HTTP/WS)/gRPC 接入、namespace、primary/replica 复制、bottomless 集成，见 §4 |
| `libsql_replication` | `libsql-replication/` | "Replication protocol for libSQL"：`frame.rs`（`FrameNo`、`FrameHeader{frame_no,checksum,page_no,size_after}`）、`replicator.rs`（回放）、`injector/`（注入 WAL）、`rpc.rs`、`snapshot.rs`、`meta.rs` |
| `bottomless` | `bottomless/` | "Bottomless replication for libSQL"：S3 归档/恢复（PITR）。`bottomless_wal.rs`（作为自定义 WAL 逐帧上传）、`replicator.rs`、`backup.rs`、`read.rs`、`transaction_cache.rs`；`bottomless-cli` 为配套 CLI |
| `libsql-hrana` | `libsql-hrana/` | Hrana 远程协议（客户端消息类型层，供 `libsql` 使用） |
| `libsql-rusqlite` | `vendored/rusqlite/` | rusqlite fork：`libsql-experimental` feature 打开 libSQL 扩展（如 `sqlite3_create_bucket`、vector API） |
| `libsql-sqlite3-parser` | `vendored/sqlite3-parser/` | SQLite 方言语法 lexer/parser 的 Rust fork（sqld 用它做语句预分析） |
| `xtask` | `xtask/` | 构建任务：从 `libsql-sqlite3` 重新生成 amalgamation 等 |
| `sql-experimental` | `bindings/c/` | C 实验性 API 绑定 |
| `libsql-wasm` | `bindings/wasm/` | WASM 构建（编译到 wasm32-wasi，跑在浏览器/WASM runtime） |

## 2. libsql-sqlite3/：SQLite C fork

### 2.1 目录组织（相对上游）

fork 基于上游 SQLite **3.47.0**（`VERSION`）的 autoconf 发布树：保留 `configure.ac`、`Makefile.in`、
`main.mk`、`tool/mksqlite3c.tcl`（amalgamation 生成器）、`src/`、`ext/`、`test/`、`doc/` 布局；
fork 版本号单独记在 `LIBSQL_VERSION`（当前 `0.2.3`）。fork 的组织方式是"**原地扩展**"：

- libSQL 新增/改动的 `.c`/`.h` 直接放进上游 `src/`（vector 系列即是），并改 `Makefile.in`、
  `main.mk`（如 `Makefile.in:203` 起 `vector.lo vectorfloat32.lo ... vectordiskann.lo vectorvtab.lo`）
  与 `tool/mksqlite3c.tcl`（`mksqlite3c.tcl:488` 把 `vector.c` 纳入 amalgamation），使新文件与上游文件同构地进入 `sqlite3.c`。
- 新特性用编译期宏裁剪（如 `SQLITE_OMIT_VECTOR`，见 `vector.c` 尾部 `#endif`），Rust 侧通过 build.rs feature 选择。
- `ext/` 在上游扩展（fts3/fts5/rtree/icu/rtree/session/rbu/recover/misc/expert…）之外新增 fork 自有扩展：
  `crr/`（CRDT、Cargo 独立构建）、`udf/`（用户定义函数机制）、`wasi/`+`libsql-wasi/`（WASI VFS）、
  `vwal/`（versioned WAL，内存版复制用）、`async/`。
- `crates/wasmtime-bindings`：WASM UDF 的 Rust 侧绑定（C 与 wasmtime 桥接）。

### 2.2 src/ 下与本项目相关的关键文件

vector 系列（全部 2024 年 libSQL 新增，版权头 "the libSQL authors"）：

| 文件 | 行数 | 职责 |
|---|---|---|
| `src/vector.c` | 1275 | `vector` blob 值类型 + SQL 函数注册中心：`vector`/`vector32`/`vector64`/`vector8`/`vector1bit`/`vector16`/`vectorb16`/`vector_extract`/`vector_distance_cos`/`vector_distance_l2`/`libsql_vector_idx`（marker 函数，`sqlite3RegisterVectorFunctions()`） |
| `src/vectorInt.h` | 192 | `Vector` 结构、element 子类型枚举与通用声明 |
| `src/vectorfloat32.c` 等 6 个 | 128–195 | 各 element 类型的构造/距离/规范化实现：`vectorfloat32.c`（f32）、`vectorfloat64.c`（f64）、`vectorfloat8.c`（i8 量化）、`vectorfloat16.c`（f16）、`vectorfloatb16.c`（bfloat16）、`vectorfloat1bit.c`（1-bit/二值），每文件提供 `vectorF<type>DistanceCos/DistanceL2` 等 |
| `src/vectorIndex.c` | 1186 | vector index 的 DDL 钩子：拦截 `CREATE INDEX ... ON t(libsql_vector_idx(col,'type=diskann','metric=cosine'))`，解析参数（`VECTOR_INDEX_TYPE_PARAM_ID`/`VECTOR_INDEX_TYPE_DISKANN`），维护全局元数据表 `VECTOR_INDEX_GLOBAL_META_TABLE`（WITHOUT ROWID），并废弃旧的 `USING` 语法 |
| `src/vectorIndexInt.h` | 259 | index 对象/参数 ID/公共接口声明 |
| `src/vectordiskann.c` | 1821 | DiskANN 实现（论文注释：DiskANN 2019 / FreshDiskANN 2021 / LM-DiskANN 2023）：建图、压缩向量节点、插入与搜索 |
| `src/vectorvtab.c` | 226 | 兼容旧的 `vector_idx` virtual table 包装（`vectorVtab`/`vectorVtabCursor`） |

核心执行/存储文件（均存在，供 03 性能研究定位；行数即规模感）：

| 文件 | 行数 | 角色 |
|---|---|---|
| `src/vdbe.c`（+`vdbeInt.h`/`vdbeapi.c`/`vdbeaux.c`/`vdbemem.c`/`vdbeblob.c`/`vdbevtab.c`/`vdbetrace.c`） | 9452 | VDBE 字节码虚拟机：SQL 全部编译为 OP_ 指令流执行 |
| `src/vdbesort.c` | 2767 | 外部排序器（ORDER BY / index build 的 merge sort） |
| `src/btree.c`（`btree.h`/`btreeInt.h`） | 11495 | B-tree 页层：游标、平衡、拆分合并、overflow 页 |
| `src/pager.c`（`pager.h`）+ `src/wal.c` + `src/pcache.c`/`pcache1.c` | 8036 | pager：页缓存、事务/日志状态机；WAL 模式；page cache |
| `src/where.c`（7490）+ `whereInt.h`/`wherecode.c`/`whereexpr.c` | — | 查询计划器：代价模型、索引选择、查询计划生成 |
| `src/select.c` | 8780 | SELECT 语义展开（子查询、聚合、ORDER BY/LIMIT 代码生成） |
| 其他相关 | — | `parse.y`（语法）、`build.c`（DDL）、`expr.c`、`func.c`、`os_kv.c`（KV-store VFS，libSQL 后加）、`memdb.c`、`status.c` |

## 3. 数据面：VDBE → btree → pager（简述）

**编译期**：`sqlite3_prepare` 走 tokenize → `parse.y` 语法 → 名称解析（`resolve.c`）→ `select.c`/`where*.c`
规划 → `vdbeaux.c` codegen，产物是一段 VDBE 程序（`Op` 数组，每条带 P1–P4 操作数）；语句对象持有它反复执行。

**执行期**：`vdbe.c` 是一个大解释循环（`sqlite3VdbeExec`），逐条执行 OP_ 指令：`OP_OpenRead` 打开 btree cursor，
`OP_Column` 经 cursor 解码行（配合 `vdbemem.c` 的 Mem 值表示），表达式求值、聚合、跳转都在指令流内完成；
排序/去重走 `vdbesort.c` 的外部 sorter（可选用 worker 线程做后台归并），虚表走 `vdbevtab.c`，blob 直读走 `vdbeblob.c`。
语句执行本身是单连接单线程的解释循环，并行只存在于 sorter 与 pager 层之下。

**存储期**：btree 以页为单位组织表/索引 B-tree（`btree.c`）；pager（`pager.c`）管理页状态机（journal/WAL 两模式）、
脏页缓存（`pcache1.c`）与事务提交；WAL 模式下提交追加 `wal.c` 的 frame（页号+commit 标记），checkpoint 才写回主文件。
libSQL 的复制正是替换/钩住这一层（`libsql-sys/src/wal/`），把 WAL frame 流导出给 sqld（见 §4）。
**对照 rdb**：rdb 没有页层， RocksDB 直接承担 pager+btree 的持久化职责；03 性能文档可在此对齐"一个 SQL 操作最终变成多少次物理读写"的口径。

## 4. sqld（libsql-server）一句话级概述

sqld 是单 primary 多 replica 的 SQLite 服务化封装：primary 把客户端事务经自定义 WAL
（`libsql-server/src/replication/primary/replication_logger_wal.rs` + `frame_stream.rs`）导出为带 `FrameNo` 的
WAL frame 流，通过 gRPC（`libsql_replication::rpc`）推给 replica，replica 用 `libsql-replication` 的 injector
按帧回放进自己的 WAL；replica 可服务只读，断点续传靠 `FrameNo` + snapshot（`snapshot.rs`/`snapshot_store.rs`）；
`bottomless` 在旁边把 frame 异步归档到 S3 做 PITR。接入面是 Hrana over HTTP/WebSocket（`hrana/`、`http/`）+ gRPC（`rpc/`），多库由 `namespace/` 隔离。

## 5. 与 rdb 对应物映射表

| turso/libsql 组件 | rdb 对应物 | 备注 |
|---|---|---|
| 协议前端：sqld Hrana/HTTP/WS、`bindings/c` | `src/sql/front/`（MySQL wire：`serve.rs`/`auth.rs`/`conv.rs`/`shim.rs`） | 协议完全不同，接入层职责同构 |
| SQL 词法/语法：`src/parse.y`、`vendored/sqlite3-parser` | `src/sql/parse/`（sqlparser crate + `translate*.rs` 到内部 IR） | turso 在引擎内做语法；rdb 在 Rust 侧前置 |
| 计划器：`src/where*.c`、`select.c`（codegen 前） | `src/sql/plan/` + `src/sql/index/`（secondary index 选择/维护） | rdb 计划器薄，索引面是 RocksDB prefix scan |
| 执行器：VDBE（`src/vdbe*.c`） | `src/sql/exec/`（`select.rs`/`scan.rs`/`expr.rs`/`write.rs`/`agg.rs`…） | 解释器 vs 直接 Rust 求值树 |
| 存储：`btree.c`/`pager.c`/`pcache*`/`wal.c` | RocksDB（`src/store/`）+ `src/sql/storage/`（`catalog.rs` raft 目录、`codec.rs`/`row.rs` 行编解码、slot 前缀键） | rdb 无页/WAL 层；列存另见 `src/sql/columnar/` |
| vector 扩展（§2.2 全部文件） | rdb 计划新增 `src/sql/` 向量面（类型/函数/DDL）+ 复用 `src/search/{vecmath.rs, quant.rs, ann/}` | rdb 已有 SQ8 量化 + SPANN 式 disk ANN + BM25 全套（`src/search/`，FT.* on RESP 面）；SQL 面尚未暴露 |
| 复制：`libsql-replication`、sqld primary/replica、frame 流 | `src/rcache/`（openraft 控制面）+ `src/sql/storage/replicate.rs`（DDL 复制屏障）+ `src/sql/dist/`（跨 slot 2PC） | turso 是物理页复制（WAL frame）；rdb 是 raft 逻辑复制，粒度/模型不同，只做行为参考 |
| `bottomless`（S3 归档客户端） | 无直接对应（`src/s3/` 是 rdb 自带的 S3 兼容对象存储**服务端**，方向相反） | PITR 思路可参考，实现不可照搬 |
| embedded replica / `libsql/src/sync` | 无对应 | rdb 客户端不持有数据副本 |

## 6. 后续深读文件清单（01–03 作者按图索骥）

### 6.1 向量 SQL 面（语法/类型/函数 → 01）

- `link_repos/turso/libsql-sqlite3/src/vector.c` — 函数集、`Vector` blob 布局、NULL/维度校验规则
- `link_repos/turso/libsql-sqlite3/src/vectorInt.h` — 子类型枚举与序列化格式
- `link_repos/turso/libsql-sqlite3/src/vectorfloat32.c` / `vectorfloat8.c` / `vectorfloat1bit.c`（其余 3 个同构，抽样即可）— 各类型距离/规范化实现
- `link_repos/turso/libsql-sqlite3/src/vectorIndex.c` — `libsql_vector_idx` DDL 语义、参数表、错误消息措辞
- `link_repos/turso/libsql-sqlite3/src/vectorvtab.c` — 兼容虚拟表（判断 rdb 是否需要等价物）
- API 面确认（已初查）：`libsql-ffi` 绑定（`bundled/bindings/bindgen.rs`）、`libsql/src/`、`vendored/rusqlite/src/` 均**无** vector 专用 API——能力只经 SQL 内置函数 + `CREATE INDEX ... libsql_vector_idx(...)` 暴露；01 设计 rdb SQL 面时可沿用"纯 SQL 面"结论

### 6.2 ANN/DiskANN 索引（→ 02）

- `link_repos/turso/libsql-sqlite3/src/vectordiskann.c` — 全文精读：建图参数、搜索路径、与 rdb `src/search/ann/` 的 SPANN 对照
- `link_repos/turso/libsql-sqlite3/src/vectorIndexInt.h` — 索引元数据结构
- `link_repos/turso/libsql-sqlite3/src/vectorIndex.c` — 元数据表布局（`VECTOR_INDEX_GLOBAL_META_TABLE`）与增量维护入口
- rdb 侧对照：`src/search/ann/mod.rs`、`src/search/ann/kmeans.rs`、`src/search/ann/partition.rs`、`src/search/quant.rs`、`src/search/vecmath.rs`

### 6.3 性能（执行/存储路径 → 03）

- `link_repos/turso/libsql-sqlite3/src/vdbe.c`（挑 OP_OpenRead/OP_Column/OP_Filter 等热点指令）+ `vdbeInt.h`
- `link_repos/turso/libsql-sqlite3/src/vdbesort.c` — 外部排序器（对照 rdb `src/sql/exec/` 的 sort 实现）
- `link_repos/turso/libsql-sqlite3/src/btree.c`（游标/页拆分章节）+ `btreeInt.h`
- `link_repos/turso/libsql-sqlite3/src/pager.c`（事务状态机）+ `wal.c`（frame 格式，与 `libsql-replication/src/frame.rs` 互证）
- `link_repos/turso/libsql-sqlite3/src/where.c`/`wherecode.c` — 代价模型与索引选择（对照 `src/sql/plan/`）
- `link_repos/turso/libsql-server/src/replication/primary/frame_stream.rs` + `libsql-replication/src/frame.rs` — frame 流导出

### 6.4 服务端/复制（背景，不单独立项）

- `link_repos/turso/libsql-server/src/`（`main.rs`、`replication/`、`hrana/`、`namespace/`）
- `link_repos/turso/bottomless/src/bottomless_wal.rs`（frame→S3 归档）
- `link_repos/turso/docs/`（`DESIGN.md`、`CONSISTENCY_MODEL.md`、`HRANA_2_SPEC.md`，行为契约备查）
