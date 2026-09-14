Commit: 98e17a5
# rdb/sql（SQL 数据面：MySQL 接入 + MVCC 事务 + 分布式执行）

## 职责
- 经 MySQL 线协议（opensrv-mysql，native-password）提供 SQL 数据面：DDL/DML/SELECT/
  SHOW/EXPLAIN/预编译语句；目录（catalog）经 raft 复制（leader-only 写）。
- 行存为 MVCC 多版本链，读写共用 16384 slot 物理空间（`<slot>/` 前缀），但 SQL 路径
  不走 RESP 的 MOVED——跨节点分布由本模块族内部完成（scatter-gather 读 + 2PC 写）。
- 与 Go 归档实现无对应关系（Rust 独有）；行为契约与偏差清单见 `COMPAT.md` 的
  "SQL data plane" 节。

## 模块地图
- `front/`：MySQL 接入——握手/auth（`auth.rs`，用户密码取自 `mysql_*` 配置）、
  shim（query/prepare/execute 到 `exec::execute` 的桥，按语句是否 INSERT 决定 OK 包
  是否携带会话 last_insert_id）、线协议值转换（`conv.rs`：文本/二进制 cell 双向，
  含 DATE/DATETIME 二进制 cell 与预编译参数解码；列定义按 ColMeta 置
  `NOT_NULL_FLAG`/`PRI_KEY_FLAG`）、
  会话变量拦截（`vars.rs`，`SELECT @@var`）、连接收尾回滚未决事务（`serve.rs`）。
- `parse/`：sqlparser 驱动的 AST→内部 IR（`translate.rs`），错误码→MySQL 错误号
  （`error.rs`：1213 写写冲突、1062 唯一冲突、1027 节点不可达等）；`query.rs`
  （复合查询翻译：UNION [ALL]/WITH CTE/派生表——INTERSECT/EXCEPT/BY NAME/
  WITH RECURSIVE 1235 大声拒绝）；`starrocks.rs`（StarRocks 表模型预解析：
  PRIMARY KEY/DUPLICATE KEY/DISTRIBUTED BY 在 MySQL 解析前从原文 token 扫描摘出
  并改写注入，未识别子句与 `AGGREGATE KEY` 1235 大声拒绝——见 COMPAT.md
  "StarRocks table-model DDL"）。
- `exec/`：执行器——`mod.rs`（`execute` 入口 + `SqlSession`）、`write.rs`（DML 与
  写集生成；StarRocks PK 模型的自动提交 INSERT 先按可见快照回收旧行喂给索引维护，
  即 upsert replace）、`select.rs`（`run_at` 单一咽喉：子查询改写 + 锁 veto）/`scan.rs`
  （FROM 物化 + 事务叠合，带 `CteScope`；`ColMeta` 携带 nullable/primary，
  DUP 模型 schema pk 不打 key 标志）、`set_ops.rs`（复合查询执行：CTE 物化、
  UNION 拼装/去重/拓宽）、`relation.rs`（`Relation` + CTE 作用域）、`subquery.rs`
  （标量/IN 子查询提升改写，相关改写按 BadField + "unknown column" 前缀匹配）、
  `agg.rs`、`expr.rs`（三值 NOT/IN；`length()` 字节 / `char_length()` 字符）、
  `show.rs`、`render.rs`（EXPLAIN，含复合计划）、`ddl.rs`（`catalog_txn`/
  `DdlPlan{mutations, schema, changed}`：table-id 分配与目录变更在同一 raft 写守卫
  窗口内单决策生效，索引回填 mutations 随决策落盘）、`sequence.rs`
  （AUTO_INCREMENT 分配：leader 串行 RMW + 批量预留 64、`LAST_INSERT_ID()`）。
- `storage/`：`row.rs`（版本键 `<slot>/ 0x20 table_id pk !ts`、header 0x01/0x00/0x02）、
  `codec.rs`（typed 编解码 + kind 常量 0x20/0x21/0x22；payload/key 标签 0x06=Date
  天数、0x07=DateTime 微秒）、`schema.rs`、`catalog.rs`
  （raft 目录 + `sql_sequence/<table>` 自增计数器；`CatalogTxn` 方法取 `&mut self`
  并经 `state()`/`*_state` 读取窗口内状态）、`gc.rs`（水位清扫：仅保留 ≤ 水位的最新 live 锚点，墓碑锚点整组清除）。
- `temporal.rs` + `temporal_tests.rs`：时间域纯函数——儒略日 civil 数学
  （`days_from_civil`/`civil_from_days`，checked 运算）、canonical/紧凑字面量解析与
  格式化（微秒 6 位、为 0 不渲染）、`now_micros`/`today_days`（UTC 墙钟）。
- `tx/`：`ts.rs`（Oracle：本地原子 / 集群模式切换，预约失败映射 1213）、`global.rs`
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
  拉起（列存/2PC/分布式读/JOIN/set-op/自增/StarRocks 等 e2e 复用）。
- 进程级 e2e（`tests/`）：`sql_e2e.rs`（握手/DDL/DML/SELECT 全链、并发建表互不撞号——败者 1050）、
  `sql_txn_e2e.rs`（快照隔离/冲突/断连回滚）、`sql_index_e2e.rs`（索引/唯一/计划）、
  `sql_oracle_cluster_e2e.rs`（进程内 3 节点全局 ts）、`sql_2pc_e2e.rs` 与
  `sql_dist_read_e2e.rs`（3 进程 2PC 写与 scatter-gather 读、FOR UPDATE veto 与
  索引表 EXPLAIN 退化）、`auto_increment_e2e.rs`（自增分配/重启续号/3 节点唯一 id/
  OK 包 last_insert_id）、
  `sql_types_e2e.rs`（DATE/DATETIME：字面量往返、谓词、类型化元数据、预编译
  二进制 cell 与参数、唯一索引、UNION 宽化、不支持/零值时间字面量大声拒绝）、
  `mysql_compat_e2e.rs`（MySQL 语义回归：NOT/IN 三值等）、
  `columnar_e2e.rs`（列存单机 + 3 节点集群扇出读）、
  `starrocks_model_e2e.rs`（表模型：单机 PK upsert/DUP 追加/拒绝矩阵含
  `AGGREGATE KEY` 1235，3 节点跨 owner replace 的行级收敛）、
  `txn_semantics_e2e.rs`（SAVEPOINT 可见性 / @@transaction_isolation / 锁读冲突）。
