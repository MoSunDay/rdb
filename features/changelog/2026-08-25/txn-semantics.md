Commit: (working-tree, 随本提交入库)

# 事务语义三件套：SAVEPOINT / @@transaction_isolation / 锁读（P1）

## 背景
Phase-1 脚手架（`d713c8e`）里三块语义只有 IR 没有实现：SAVEPOINT 家族
dispatch 到 1235 桩；`@@transaction_isolation` 返回与连接无关的常量；
`SELECT ... FOR UPDATE / FOR SHARE` 在 translate 阶段直接拒绝（`b8491d4`
当时显式报错是诚实的选择）。本提交补齐 1.5 / 1.6 / 1.7 三条 lane 的真实
语义，全部落在既有快照隔离 + 暂存写集模型之上。

## 实现
### SAVEPOINT / ROLLBACK TO / RELEASE（真语义，非桩）
- **marker 快照整张 `writes` BTreeMap**，而非仅记长度——marker 之后对
  marker 之前 pk 的 re-stage 会原地覆盖，只回退长度会留下脏值；同时快照
  列存 append 缓冲长度与当时持有的 latch keys。
- 同名 savepoint 后写遮蔽前写（`rposition` 找最新，大小写不敏感）；
  `ROLLBACK TO` 保留目标 marker（可重复回滚）、截断其后的 marker、恢复
  writes/appends；`RELEASE` 删除 marker 及其后所有 marker。
- latch 交互对齐 MySQL 行锁：**保留 marker 之前拿的锁，释放之后拿的**
  （连同被撤销的写）。COMMIT/ROLLBACK 清空一切（每次 BEGIN 分配新 Txn
  与 latch owner id）。
- 裸 `SAVEPOINT` 无事务时隐式开事务（MySQL 行为）；无事务时
  `ROLLBACK TO` / `RELEASE` 报 1305（ER_SP_DOES_NOT_EXIST），未知名字同。

### 会话级 @@transaction_isolation
- `vars.rs` 新增 `SessionVars { isolation }`，`shim.rs` 从会话状态透传；
  `sysvar_value` / `sysvar_outcome` 增加 session 参数。
- `@@transaction_isolation` / `@@tx_isolation` 默认 **REPEATABLE-READ**
  （MySQL 默认值，也恰是引擎唯一隔离级——快照读）；`SET SESSION
  TRANSACTION ISOLATION LEVEL ...` 接受并按会话原样回显（引擎行为不变，
  报告值不再误导客户端）。

### 锁读（FOR UPDATE / FOR SHARE）
- AST 增 `Query.lock: Option<LockRead>`；`translate_lock` 拒绝多锁并列、
  `NOWAIT` / `SKIP LOCKED`（1235）、`OF` 非 FROM 表；接受 `OF <FROM 表>`。
- 新增 `src/sql/tx/latch.rs`：进程级注册表
  `OnceLock<Mutex<BTreeMap<LatchKey, LatchEntry>>>`，key 为
  `(table_id, pk)`，纯函数 `*_in` + 薄包装。
  - **all-or-nothing**：一批 key 要么全拿到要么不拿；
  - 同 owner 可重入（自己 FOR SHARE → FOR UPDATE 升级是 no-op，同 MySQL
    自有锁升级）；shared 可多持有者共享，exclusive 互斥；
  - 冲突**立即失败**，1205 报文形状（"Lock wait timeout exceeded; try
    restarting transaction"）——绝不阻塞等待，无超时旋钮、无死锁检测；
  - 显式事务 latch 挂 txn id，COMMIT/ROLLBACK 释放；autocommit 锁读用
    语句级临时 owner，语句结束即释放；
  - **多节点拓扑 veto**（1235）：latch 是节点本地的，gather 路径无法
    锁远端 band，宁可报错不可静默降级。
- `select.rs run` 顶部隔离小块：veto → 物化 → latch（txn id 或临时
  owner）→ 释放；`execute_query` 以下零改动（避免与 select/query lane
  冲突）。

## 测试覆盖
| 功能 | 测试 | 文件 |
|------|------|------|
| savepoint 回滚/遮蔽/截断/释放/存续/可见性/latch 交互（8 例） | `savepoint_*` / `rollback_to_*` / `release_*` / `staged_write_*` | `src/sql/tx/savepoint_tests.rs` |
| latch 全拿或全不拿+重入 / 1205 / 共享互斥 / 释放 / 集群 veto（7 例） | `owner_ids_*` / `acquire_*` / `conflicting_*` / `shared_*` / `release_*` / `cluster_*` | `src/sql/tx/latch.rs` |
| 隔离级默认值与逐级回显（2 例） | `isolation_*` | `src/sql/front/vars.rs` |
| 锁语法翻译（含 NOWAIT/SKIP LOCKED 拒绝） | `locking_reads_translate_into_query_lock` 等 | `src/sql/parse/mod.rs` |
| e2e：savepoint 可见性 / 隔离级往返 / 双会话 FOR UPDATE 冲突 / ROLLBACK TO 释放锁 / FOR SHARE 共享 + autocommit / NOWAIT 仍拒绝（6 例） | `tests/txn_semantics_e2e.rs` | 单进程真实节点 |

## 验证
- 全量 `cargo test --workspace`：40 个套件全绿（单测 720 + 全部 e2e）。
- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings`
  / `cargo build --release --workspace` 通过。
