Commit: b0d81c9
# rdb/sql（SQL 数据面：MySQL 接入 + MVCC 事务 + 分布式执行）

## 职责
- 经 MySQL 线协议（opensrv-mysql，native-password）提供 SQL 数据面：DDL/DML/SELECT/
  SHOW/EXPLAIN/预编译语句；目录（catalog）经 raft 复制（leader-only 写）。
- 行存为 MVCC 多版本链，读写共用 16384 slot 物理空间（`<slot>/` 前缀），但 SQL 路径
  不走 RESP 的 MOVED——跨节点分布由本模块族内部完成（scatter-gather 读 + 2PC 写）。
- 与 Go 归档实现无对应关系（Rust 独有）；行为契约与偏差清单见 `COMPAT.sql.md`
  （原 `COMPAT.md` "SQL data plane" 节，2026-10-06 MySQL-gap M0-M5 后按行数预算拆出）。

## 模块地图
- `front/`：MySQL 接入——握手/auth（`auth.rs`，用户密码取自 `mysql_*` 配置）、
  shim（query/prepare/execute 到 `exec::execute` 的桥，按语句是否 INSERT 决定 OK 包
  是否携带会话 last_insert_id）、线协议值转换（`conv.rs`：预编译参数解码（含
  DATE/DATETIME 二进制参数）、Date/DateTime/Decimal cell 编码器与列定义
  （按 ColMeta 置 `NOT_NULL_FLAG`/`PRI_KEY_FLAG`）；`conv_bin.rs`：结果集 cell 按公告列型编码——
  文本协议逐字节委托 opensrv 原实现（NULL 为 0xFB 单字节），二进制路径对静态/
  运行期类型错配做兼容编码（`?` 定型 VAR_STRING、数值/DATE 绑定出规范文本，
  与文本协议一致；数值进 DOUBLE/FLOAT、Date↔DateTime 互转、整型列宽收窄），
  无忠实拼法的组合（字符串对数值列/NULL 对 NOT NULL 列/行长不齐）由
  `preflight_binary` 在结果集开始前响亮回 1292，shim 的 `write_outcome` 仅
  prepared 路径启用预检——2026-10-08 收口 M5 开放跟进项，二进制协议不再断连）、
  会话变量拦截（`vars.rs`：`SELECT @@var` 形状识别 + 25 项 sysvar 表，M4 起
  `SHOW [GLOBAL|SESSION] VARIABLES [LIKE]` 与 `SHOW STATUS` 复用同一张表——LIKE
  为 SHOW 专用大小写不敏感匹配，数据 LIKE 保持 bytewise）、连接收尾回滚未决
  事务 + 握手期由认证用户/peer host/连接 id 构造 `SessionInfo`（`serve.rs`）。
- `parse/`：sqlparser 驱动的 AST→内部 IR（`translate.rs`：dispatch 枢纽（M2 起
  INSERT/DDL 转出）+ `translate_type.rs`：列类型翻译，DECIMAL(p,s)/NUMERIC 的
  p 1..=38 校验；`translate_dml.rs`：INSERT 家族专用翻译——`VALUES(col)` 的唯一
  合法点（ODKU 赋值 → `Expr::InsertValues` 标记，其余位置 1064）、`INSERT … SET`
  归一为命名单行 VALUES、REPLACE+ODKU 组合 1064；`translate_ddl.rs`：TRUNCATE/
  RENAME/ALTER-INDEX（含其余 ALTER 操作点名 P2 拒绝）与 SHOW `[LIKE|WHERE]`
  形态），错误码→MySQL 错误号
  （`error.rs`：1213 写写冲突、1062 唯一冲突、1027 节点不可达等）；`query.rs`
  （复合查询翻译：UNION [ALL]/INTERSECT/EXCEPT [DISTINCT|ALL]/WITH CTE/派生表
  ——MINUS/BY NAME/WITH RECURSIVE 1235 大声拒绝）；`order_limit.rs` + `order_keys.rs`（M0：
  ORDER BY/GROUP BY 裸整型序数→1-based 投影位置（越界 1054）、ORDER BY/HAVING
  别名大小写不敏感代入（别名优先同名 FROM 列，不做二次替换）、`LIMIT ?`/
  `OFFSET ?` 占位符随语句带到执行期绑定）；`table.rs`（`FROM DUAL` 归一为
  `TableRef::NoTable`）；`expr.rs` + `func_forms.rs`（CASE/CAST/CONVERT（USING
  仅 utf8/utf8mb4）/RLIKE/SUBSTRING FROM-FOR/TRIM remstr/POSITION IN/
  `d ± INTERVAL n unit` 等特殊形态直翻或去糖；INTERVAL 单位白名单；
  GROUP_CONCAT SEPARATOR/DISTINCT（内层 ORDER BY 翻译期 1235）；CEIL/FLOOR
  关键字形态专用节点翻译）；`func_sig.rs`（纯 name→(min,max) arity 签名表，
  prepare 期即报 1582，与家族求值器、`func::meta` 三方同步）；`starrocks.rs`
  （StarRocks 表模型预解析：
  PRIMARY KEY/DUPLICATE KEY/DISTRIBUTED BY 在 MySQL 解析前从原文 token 扫描摘出
  并改写注入，未识别子句与 `AGGREGATE KEY` 1235 大声拒绝——见 COMPAT.md
  "StarRocks table-model DDL"）。
- `exec/`：执行器——`mod.rs`（`execute` 入口 + `SqlSession`；M4 起握手构造的
  `SessionInfo { user, connection_id }` 随会话穿透，`session_funcs::substitute`
  在每次执行入口把零参会话函数改写为字面值——求值层保持 session-free）、
  `write.rs`（DML 写集决策 + `apply_writes` 行批 sink：RowWrite（版本行 + 索引
  op + 写前沿探测）统一入 2PC 或本地提交，INSERT/UPDATE/DELETE 三族共用；
  StarRocks PK 模型的自动提交 INSERT 先按可见快照回收旧行喂给索引维护，
  即 upsert replace）、`insert_common.rs`（行构造共享层：列清单展开/求值/列类型
  coerce/NOT NULL——VALUES 元组、SELECT 源、INSERT…SET 归一形态全部收敛同一
  `build_row_values`）、`upsert.rs`（ODKU/REPLACE 纯决策层：写前取冲突快照
  （autocommit 前沿同步 now / txn pinned read_ts 合并 staged 写）建 pk→row 与
  unique value→owner 映射——后者是**每条已决策写维护的 BTreeMap 活 overlay**
  （O(log n) 直接查找，非按行重扫），先 pk 后列序 unique 探测，决策流入
  `apply_writes`；ODKU update 分支在决策期即拒两类抢占：pk 挪移命中活行与
  唯一值改挂他行（镜像检查，均 1062，事务错误在语句处浮出而非 COMMIT）——
  含改 pk 本身的 ODKU 与 REPLACE 的删多插一）、`insert_select.rs`（SELECT 源在
  任何写落地前一次快照物化 + 位置 arity 校验）、`select.rs`（`run_at` 单一咽喉：
  子查询改写 + 锁 veto + 相关子查询第二趟 bind 钩子）/`scan.rs`
  （FROM 物化 + 事务叠合，带 `CteScope`；`ColMeta` 携带 nullable/primary，
  DUP 模型 schema pk 不打 key 标志）、`set_ops.rs`（复合查询执行：CTE 物化、
  UNION 拼装/去重/拓宽、INTERSECT/EXCEPT 的 DISTINCT 去重与 ALL 多重集交/差；
  优先级来自 sqlparser 标准文法——INTERSECT 更紧，同级左折叠）、
  `relation.rs`（`Relation` + CTE 作用域）、`subquery.rs`（非相关标量/IN 折叠
  为字面量 + EXISTS 分支；相关名解析失败时 defer 保留节点，第二趟切换 bind）、
  `correlated.rs` + `correlated_map.rs`（defer→bind 第二趟：按 distinct 外层键
  记忆化，外层引用字面量替换进子查询副本独立求值，产出纯数据
  `Expr::Correlated` 查表——求值期零存储访问；shadow 判定防内层表自身列误判）、
  `agg.rs`（COUNT/SUM/AVG/MIN/MAX + M1 的 GROUP_CONCAT）、`expr.rs`（通用算子
  求值 + `Expr::Correlated` 查表 + 对 `func::eval_func` 的转发；三值 NOT/IN；
  `length()` 字节 / `char_length()` 字符）与
  `expr_decimal.rs`（i128 精确十进制算术：`/` 长除 scale+4 half-away-from-zero、
  列 scale 舍入 fit_column/1292）、
  `show.rs`（SHOW TABLES/COLUMNS/INDEX + M4 的 CREATE TABLE 确定性渲染（可重建
  往返）/DATABASES/VARIABLES[LIKE]/STATUS）、`render.rs`（EXPLAIN，含复合计划）、
  `session_funcs.rs`（DATABASE/USER/CONNECTION_ID 族 per-exec 字面量替换）、
  `ddl.rs`（`catalog_txn`/
  `catalog_apply`：进程级 `CATALOG_MUX`（pub(crate)）串行整个 DDL（decide→queue→commit-await），
  并同时串行 `sequence::allocate` 的 floor RMW 全程（读 floor→queue→commit-await），
  raft 写锁只在 decide+queue 瞬间持有，commit 一律锁外 await——锁跨 await 会饿死
  leader 的 raft/HTTP 服务与 ts refill（4 核 CI 实证挂死）；
  `DdlPlan{mutations, schema, changed}`：table-id 分配与目录变更在同一 raft 写守卫
  窗口内单决策生效，索引回填 mutations 随决策落盘；M4 拆分后收缩为 CREATE/DROP
  TABLE + 临界区 + 表缓存失效）、`ddl_alter.rs`（M4 拆出：TRUNCATE 同名换新
  table_id（物理等价 DROP+同定义重建，复制效果即全集群一次 truncate）、RENAME
  纯目录改名（table_id/物理 key 不动，自增计数器按原值搬迁）、CREATE/DROP INDEX
  与 ALTER ADD/DROP INDEX/内联 KEY——复用同一 raft 守护临界区）、`sequence.rs`
  （AUTO_INCREMENT 分配：串行由 `CATALOG_MUX` 提供——floor 读→queue→commit-await
  全程持 mux，FSM 只见已 apply 的 bump，queued-but-unapplied 期间重读旧 floor 即重号；
  raft 写锁仍只在 queue 前瞬间持有、批量预留 64、`LAST_INSERT_ID()`）。
- `exec/func/`（M1 拆分）：标量函数纯 dispatch（无注册器 struct、无状态）——
  `mod.rs`（`eval_func` 家族链式 `control -> string -> numeric -> datetime`，每家族
  只答 `Option<SqlResult>`；`wrong_param_count` 统一 1582 措辞）、`string.rs` +
  `string_more.rs`（CONCAT 族/SUBSTRING/LEFT/RIGHT/LPAD/RPAD/REPEAT/LOCATE/INSTR/
  POSITION/REPLACE/TRIM/REVERSE/HEX/UNHEX + 迁入的 UPPER/LOWER/LENGTH/CHAR_LENGTH；
  导出 byte-wise `regexp_match`）、`numeric.rs` + `numeric_more.rs`（ROUND/CEIL/
  FLOOR/TRUNCATE/MOD/POW/SQRT/SIGN/GREATEST/LEAST + 迁入 ABS；`eval_bitop` 的
  `& | ^ << >>` u64 语义）、`datetime.rs` + `datetime_more.rs`（字段抽取族/
  DATE_ADD 族全 INTERVAL 单位/DATEDIFF/DATE_FORMAT/UNIX_TIMESTAMP 族 + 迁入时钟
  族）、`control.rs`（VERSION/LAST_INSERT_ID + 三个结构化入口：`eval_case`/
  `eval_cast`/`eval_lazy`——IF/IFNULL/NULLIF/COALESCE 实参求值前拦截）、
  `meta.rs`（纯 name→SqlType 结果列类型表，投影 typer 查表）。`expr.rs` 收缩为
  通用算子 + 转发，不再持有函数体。
- `storage/`：`row.rs`（版本键 `<slot>/ 0x20 table_id pk !ts`、header 0x01/0x00/0x02）、
  `codec.rs`（typed 编解码 + kind 常量 0x20/0x21/0x22；payload/key 标签 0x06=Date
  天数、0x07=DateTime 微秒）、`schema.rs`、`catalog.rs`
  （raft 目录 + `sql_sequence/<table>` 自增计数器；`CatalogTxn` 方法取 `&mut self`
  并经 `state()`/`*_state` 读取窗口内状态）、`gc.rs`（水位清扫：仅保留 ≤ 水位的最新 live 锚点，墓碑锚点整组清除）。
- `temporal.rs` + `temporal_tests.rs`：时间域纯函数——儒略日 civil 数学
  （`days_from_civil`/`civil_from_days`，checked 运算）、canonical/紧凑字面量解析与
  格式化（微秒 6 位、为 0 不渲染）、`now_micros`/`today_days`（UTC 墙钟）。
- `tx/`：`ts.rs`（Oracle：本地原子 / 集群模式切换，预约失败映射 1213）、`floor.rs`
  （持久时钟下限：保留键 `\x00sql_ts_floor` 随每个打戳批原子捎带最大 ts，boot
  `advance_to` 恢复（normal+backup 监听）；键缺失=pre-floor 库原地升级，boot 一次
  性全键空间扫描取 max 并立即落键）、`global.rs`
  （raft 块授权：`sql_ts_cursor` 先持久后发放、4096 块、HTTP `/sql/ts`；
  `reserve_write_frontier(floor, want, strict)`——写集含远端属主（2PC）时 strict：
  leader 不可达即拒绝盖章，绝不本地 GAP；纯本地写宽松：降级单调回退，同节点 refill
  重锚）、`nodes.rs`（`sql_nodes` 注册表：raft addr → 各 bind；`cluster_ready` 边沿
  250ms 快轮询注册 + 3s 循环兜底）、`session.rs`（快照事务：写集暂存、
  own-write 叠合、首提交者胜冲突检测、索引维护入提交批、SAVEPOINT 栈——marker 快照
  整张写集 + append 长度 + 当时 latch）、`latch.rs`（锁读注册表：`(table_id,pk)`
  进程级、all-or-nothing、同 owner 重入、冲突 1205 快败、多节点 veto）。
- `index/`：二级/唯一索引键（`keys.rs`，索引 slot=`crc16(table_id++col_pos)`）、
  行变迁→索引操作推导与维护（`maintain.rs`、`mod.rs` 查找/范围/唯一属主）。
- `plan/`：单表访问路径（IndexLookup vs SeqScan，sargable =/IN/BETWEEN，>1000 pk 回退）。
- `dist/`：节点间 SQL RPC（`sql_rpc_bind`，u32 长度前缀 JSON）——`mod.rs` 公共件含
  `row_probe`/`any_remote_owner`（行平面 slot 探测写集是否跨远端属主，strict 预约判定）、
  `twopc.rs` 协调者、
  `participant.rs` 参与者（PREPARE/DECIDE 各为单原子批 + 参与者标记）、`plan.rs`
  （写计划按 slot 归属分组）、`gather.rs`（按 band scatter-gather 读）、`recover.rs`
  （在疑标记经 `/sql2pc/status` 决议，60s 租期 presumed-abort）。
- `columnar/`：列存引擎——段文件（`format.rs` 信封/自写 CRC-32、`encode.rs`
  PLAIN+DICT 页、`decode.rs`）、元数据 kind `0x23`（`meta.rs`，无 slot 前缀）、
  段注册表（`mod.rs`，按 `(store_path, bind)` 缓存）、冲刷/放置（`writer.rs`、
  `commit.rs`）、读（`reader.rs`）、DROP 清理与孤儿清扫（`commit.rs`、`gc.rs`）。

## 关键不变量
- 版本键 ts 后缀取反（`!ts`）：同 pk 新版本在前；`visible_value` 取 `ts ≤ read_ts`
  的首个非 0x02 版本。
- 时间戳：集群未就绪=本地原子；就绪后所有 alloc 经 leader 块授权，游标先 raft 持久；
  两种形态都有跨重启下限——`\x00sql_ts_floor` 保留键（本地批捎带持久化，boot
  `advance_to`；见 `tx/floor.rs`），时钟绝不回退（回退会让旧版本行遮蔽重写）；
  `now()` 骑集群游标前沿（`sync_cursor_frontier`，允许读旧快照，禁止回退）；写路径
  先按读点预留写前沿（`reserve_write_frontier`/`alloc_n_above`，过期或过短的本地
  块尾作废重租），保证本节点写入的 ts 高于它已读到的版本；ts authority 不可达时，
  跨远端属主的提交 fail-fast（1213，GAP 无 cursor 可覆盖、newest-wins 会静默掩埋），
  纯本地写保留 GAP 降级。
- 2PC：提交决议先落本地库再广播（outcome 落库失败 = 提交失败：不发出任何 Decide，
  尽力广播 abort，客户端收到可重试 WriteConflict）；status 应答（HTTP 与 TxnStatus）
  只含请求节点名下的索引切片（outcome 记录按节点映射：协调者含全部参与者、参与者
  在自身 bind 下；`own_ops` 仅本地重放，永不过线）；参与者 PREPARE 批含 0x02 行 +
  唯一索引项 + 标记；COMMIT 翻转 0x02→0x01 并补 0x21 项；读路径永不显露 0x02。
- 集群模式（>1 稳定实例）下：读与 UPDATE/DELETE 行匹配走 Gather（band 并发拉取，
  任一 owner 不可达即整查报错，写匹配绝不只看本地切片）；DDL ack 前等各可达 peer
  的 FSM 服务该目录写（`storage/replicate.rs` 复制屏障，best-effort 有界延迟）；
  索引路径与 JOIN 物化保持本地（v1 限制）。
- 列存：段只落提交节点，读向所有节点扇出（`ScanColumnar`）；可见性=段级
  `commit_ts ≤ read_ts`，prepared 段不入注册表即不可见；段元数据与行写同批原子发布。

## 测试地图
- 单元：各模块旁 `*_tests.rs`（tx/global、tx/savepoint、index、plan、exec/*（含
  `ddl_tests.rs`/`expr_tests.rs`）、storage/gc）。
- 公共件：`tests/common/mod.rs` 的 `start_sql_cluster(dir, n)` 统一多节点 SQL 集群
  拉起（列存/2PC/分布式读/JOIN/set-op/自增/StarRocks 等 e2e 复用）；M5 起另有
  `tests/common/mysql.rs`——单一 MySQL e2e 脚手架（connect/leader 重试 ddl+run/
  rows 族/errno 常量/affected 与 server_error 探针/wait_table/world 夹具），
  收敛原 7 套件本地样板并吸收 `sql_funcs_common`、`sql_upsert_common`（目录已删）；
  9 个集群/HA 编排套件（2pc/join_cluster/setop_cluster/dist_read/restart/
  composite_pk/ddl_visibility/starrocks/upsert_cluster 等）有意暂持本地副本。
- 进程级 e2e（`tests/`）：`sql_e2e.rs`（握手/DDL/DML/SELECT 全链、并发建表互不撞号——败者 1050）、
  `sql_txn_e2e.rs`（快照隔离/冲突/断连回滚）、`sql_index_e2e.rs`（索引/唯一/计划）、
  `sql_oracle_cluster_e2e.rs`（进程内 3 节点全局 ts）、`sql_2pc_e2e.rs` 与
  `sql_dist_read_e2e.rs`（3 进程 2PC 写与 scatter-gather 读、FOR UPDATE veto 与
  索引表 EXPLAIN 退化）、`auto_increment_e2e.rs`（自增分配/重启续号/3 节点唯一 id/
  OK 包 last_insert_id）、
  `sql_types_e2e.rs`（DATE/DATETIME 与 DECIMAL：字面量往返、谓词、类型化元数据、
  预编译二进制 cell 与参数、唯一索引、UNION 宽化、不支持/零值时间字面量大声拒绝）、
  `sql_composite_pk_e2e.rs`（复合 pk：全元组去重、唯一索引 1062、部分键 WHERE、
  NUL 转义保序、重启持久化）、`sql_restart_e2e.rs`（单机 kill -9 重启可见性：
  ts floor 键 + 缺键时 boot 一次性扫描恢复）、
  `mysql_compat_e2e.rs`（MySQL 语义回归：NOT/IN 三值等；M3 增 INTERSECT/EXCEPT
  混合链左折叠、尾部 `ORDER BY 1 LIMIT .. OFFSET ..`、相关 IN/EXISTS/标量子查询
  与拒绝矩阵；M5 随 common::mysql 收敛样板）、
  `columnar_e2e.rs`（列存单机 + 3 节点集群扇出读）、
  `starrocks_model_e2e.rs`（表模型：单机 PK upsert/DUP 追加/拒绝矩阵含
  `AGGREGATE KEY` 1235，3 节点跨 owner replace 的行级收敛）、
  `txn_semantics_e2e.rs`（SAVEPOINT 可见性 / @@transaction_isolation / 锁读冲突）、
  MySQL-gap M0-M5 新增（2026-10-06，均走 `common/mysql`）：
  `sql_query_semantics_e2e`（序数/别名解析/LIMIT ?/FROM DUAL，M0）、
  `sql_funcs_{string,numeric,datetime,control}_e2e`（函数族 29 用例 + 元数据 +
  prepared + 负矩阵，M1）、`sql_upsert_e2e`/`sql_insert_select_e2e`（ODKU/REPLACE/
  INSERT…SELECT/INSERT…SET 语义与负矩阵）与 `sql_upsert_cluster_e2e`（集群 ODKU/
  REPLACE 1235 + plain 路径 2PC，M2）、`sql_subquery_e2e`（相关子查询三形态/
  集合操作/窄拒绝，M3）、`sql_ddl_surface_e2e` + `sql_ddl_surface_cluster_e2e`
  （TRUNCATE/RENAME/索引 DDL/SHOW 面/会话函数，单机 + 三进程 ack 即刻可见，M4）与
  `sql_rename_gc_e2e`（RENAME/TRUNCATE × 后台 GC：`RDB_SQL_GC_PERIOD_MS` 缩短
  周期，~7 轮清扫后改名表数据完好回归，M4）、
  `sql_outer_join_e2e`（LEFT/RIGHT OUTER JOIN 首次 e2e 覆盖）与 `sql_failover_e2e`
  （三进程 SIGKILL SQL leader：新 leader 接管 DDL/写/ts，尸体 respawn 追平，M5）、
  `sql_decimal_e2e`（自 `sql_types_e2e` 拆出的 DECIMAL 精确语义自包含用例，M5）。
