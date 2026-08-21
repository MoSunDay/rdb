Commit: c0cce389e75f34cf39c06ac4b56f22cde7efd1f3
# rust/sql（SQL 数据面：MySQL 接入 + MVCC 事务 + 分布式执行）

## 职责
- 经 MySQL 线协议（opensrv-mysql，native-password）提供 SQL 数据面：DDL/DML/SELECT/
  SHOW/EXPLAIN/预编译语句；目录（catalog）经 raft 复制（leader-only 写）。
- 行存为 MVCC 多版本链，读写共用 16384 slot 物理空间（`<slot>/` 前缀），但 SQL 路径
  不走 RESP 的 MOVED——跨节点分布由本模块族内部完成（scatter-gather 读 + 2PC 写）。
- 与 Go 归档实现无对应关系（Rust 独有）；行为契约与偏差清单见 `rust/COMPAT.md` 的
  "SQL data plane" 节。

## 模块地图
- `front/`：MySQL 接入——握手/auth（`auth.rs`，用户密码取自 `mysql_*` 配置）、
  shim（query/prepare/execute 到 `exec::execute` 的桥）、会话变量拦截（`vars.rs`，
  `SELECT @@var`）、连接收尾回滚未决事务（`serve.rs`）。
- `parse/`：sqlparser 驱动的 AST→内部 IR（`translate.rs`），错误码→MySQL 错误号
  （`error.rs`：1213 写写冲突、1062 唯一冲突、1027 节点不可达等）。
- `exec/`：执行器——`mod.rs`（`execute` 入口 + `SqlSession`）、`write.rs`（DML 与
  写集生成）、`select.rs`/`scan.rs`（FROM 物化 + 事务叠合）、`agg.rs`、`expr.rs`、
  `show.rs`、`render.rs`（EXPLAIN）、`ddl.rs`（目录写 + 索引回填）。
- `storage/`：`row.rs`（版本键 `<slot>/ 0x20 table_id pk !ts`、header 0x01/0x00/0x02）、
  `codec.rs`（typed 编解码 + kind 常量 0x20/0x21/0x22）、`schema.rs`、`catalog.rs`
  （raft 目录）、`gc.rs`（水位清扫：仅保留 ≤ 水位的最新 live 锚点，墓碑锚点整组清除）。
- `tx/`：`ts.rs`（Oracle：本地原子 / 集群模式切换）、`global.rs`（raft 块授权：
  `sql_ts_cursor` 先持久后发放、4096 块、HTTP `/sql/ts`、降级单调回退）、`nodes.rs`
  （`sql_nodes` 注册表：raft addr → 各 bind）、`session.rs`（快照事务：写集暂存、
  own-write 叠合、首提交者胜冲突检测、索引维护入提交批）。
- `index/`：二级/唯一索引键（`keys.rs`，索引 slot=`crc16(table_id++col_pos)`）、
  行变迁→索引操作推导与维护（`maintain.rs`、`mod.rs` 查找/范围/唯一属主）。
- `plan/`：单表访问路径（IndexLookup vs SeqScan，sargable =/IN/BETWEEN，>1000 pk 回退）。
- `dist/`：节点间 SQL RPC（`sql_rpc_bind`，u32 长度前缀 JSON）——`twopc.rs` 协调者、
  `participant.rs` 参与者（PREPARE/DECIDE 各为单原子批 + 参与者标记）、`plan.rs`
  （写计划按 slot 归属分组）、`gather.rs`（按 band scatter-gather 读）、`recover.rs`
  （在疑标记经 `/sql2pc/status` 决议，60s 租期 presumed-abort）。

## 关键不变量
- 版本键 ts 后缀取反（`!ts`）：同 pk 新版本在前；`visible_value` 取 `ts ≤ read_ts`
  的首个非 0x02 版本。
- 时间戳：集群未就绪=本地原子；就绪后所有 alloc 经 leader 块授权，游标先 raft 持久；
  任一节点 `now()` 为本地已知 global_hi（允许读旧快照，禁止回退）。
- 2PC：提交决议先落本地库再广播；参与者 PREPARE 批含 0x02 行 + 唯一索引项 + 标记；
  COMMIT 翻转 0x02→0x01 并补 0x21 项；读路径永不显露 0x02。
- 集群模式（>1 稳定实例）下：读走 Gather（band 并发拉取，任一 owner 不可达即整查
  报错）；索引路径与 JOIN 物化保持本地（v1 限制）。

## 测试地图
- 单元：各模块旁 `*_tests.rs`（tx/global、index、plan、exec/*、storage/gc）。
- 进程级 e2e（`rust/tests/`）：`sql_e2e.rs`（握手/DDL/DML/SELECT 全链）、
  `sql_txn_e2e.rs`（快照隔离/冲突）、`sql_index_e2e.rs`（索引/唯一/计划）、
  `sql_oracle_cluster_e2e.rs`（进程内 3 节点全局 ts）、`sql_2pc_e2e.rs` 与
  `sql_dist_read_e2e.rs`（3 进程 2PC 写与 scatter-gather 读）。
