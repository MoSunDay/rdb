# SQL M2：DML 冲突路径（ODKU / REPLACE / INSERT…SELECT / INSERT…SET）

Commit: (working-tree, 随本提交入库)

## 背景
MySQL 兼容差距计划（`plans/2026-10-06-mysql-gap/`）B 组定位出 INSERT 家族的整块缺口：
`parse/translate.rs`（727 行）对 `ON DUPLICATE KEY UPDATE`、`INSERT ... SET`、`REPLACE INTO`
三形态一律 unsupported，`INSERT ... SELECT` 亦不可用；affected rows 无 1/2/0 语义，
`VALUES(col)` 引用不存在。M2 的**第一动作是拆文件**（translate.rs 与 749 行的 `exec/write.rs`
都逼近 800 上限），再补齐冲突路径。

## 修复内容（src/sql，待提交工作区）

### 拆分：翻译期 `parse/translate_dml.rs` + 执行期 `exec/insert_common.rs` / `exec/upsert.rs` / `exec/insert_select.rs`
- `parse/translate_dml.rs`（294）：INSERT 家族专用翻译（`translate.rs` 只 dispatch 进来）。
  持有 `VALUES(col)` 的唯一合法点（ODKU 赋值 → IR 标记 `Expr::InsertValues`，其余位置
  1064 parse error）、`INSERT ... SET` 归一化（命名单行 VALUES）、`REPLACE`+`ODKU` 组合
  1064、VALUES/SELECT 源形态与列清单 arity 校验。`translate_dml_tests.rs`（194）随附。
- `exec/insert_common.rs`（302）：行构造共享层——列清单展开（命名/位置）、求值、列类型
  coerce、NOT NULL（AUTO_INCREMENT 槽位例外，延迟到 allocator）；VALUES 元组、SELECT 源、
  SET 归一形态**全部收敛到同一 `build_row_values`**。
- `exec/upsert.rs`（279）：ODKU 与 REPLACE 的纯决策层——写前先取冲突快照（autocommit：
  frontier 同步后的 `now()`；txn：pinned `read_ts` 合并本事务 staged 写），pk→row 与每条
  unique 索引的 value→owner 映射；探测顺序确定：**先 pk、再按列序逐个 unique 索引**。
  决策产物流入 `write::apply_writes`（与 UPDATE 共用索引迁移 / 2PC / 批写机制，含改 pk
  本身的 ODKU）。`upsert_tests.rs`（395）随附。
- `exec/insert_select.rs`（69）：SELECT 源在**任何写落地前物化完毕**（复用 `set_ops`
  一次快照读 + 位置 arity 校验），同表读写天然读语句前快照。`insert_select_tests.rs`（257）。
- `exec/write.rs`（675）：`insert` 收窄为 dispatch（列存/集群 veto → 行构造 → 冲突分支），
  plain 路径原样保留。

### IR 形状（`parse/ast.rs`）
`Statement::Insert { table, columns, source, conflict }`：
- `source: InsertSource::Values(Vec<Vec<Expr>>)`（SET 归一于此）| `Select(Box<CompoundQuery>)`；
- `conflict: ConflictAction::Error`（plain，pre-M2 行为不变）| `OnDuplicate(Vec<(String, Expr)>)`
  | `Replace`；
- `Expr::InsertValues(String)`：ODKU 赋值内 `VALUES(col)` 的唯一产物，求值时由
  `subst_values` 换成**待插入行**的列值；逃逸到通用求值器 → 1235 大声错误。

### 语义（MySQL 对齐）
- **affected rows 1/2/0**：新插入 1 / 冲突且值有变 2 / 冲突且逐列相同 0（写前比较整行）；
  多行语句逐行累计（[new, changed, identical] → 3）。
- **`VALUES(col)` 与未赋值列**：赋值右值里裸列引用读**现存行**、`VALUES(col)` 读**待插入行**
  （`c = c + VALUES(c)` 可用）；未出现在赋值清单的列保持现存行值（不取待插入行）。
- **pk 变更 ODKU**：`ON DUPLICATE KEY UPDATE id = 2, ...` 同一写内迁移行 + 全部索引条目，
  老 pk 可复用、被拒插入行的 unique 值不留残 entry。
- **REPLACE**：删除 pk + **每一条** unique 命中（去重）后插入，affected = 删除数 + 1
  （单 pk 冲突 2；一行同时撞两条 unique 索引 + pk → 3）；腾出的 unique 值可复用。
- **INSERT…SELECT 快照**：源物化先于写（`INSERT INTO t SELECT ... FROM t` 读语句前快照，
  精确翻倍不发散）；txn 内合并本事务 staged 行；与 ODKU/REPLACE 组合可用（聚合 upsert：
  `SELECT k, SUM(v) ... GROUP BY k ON DUPLICATE KEY UPDATE total = total + VALUES(total)`）。
- **INSERT…SET**：`INSERT INTO t SET a = e, b = e` ≡ 命名列清单 + 单行 VALUES（含表达式与
  ODKU 组合），带源（VALUES/SELECT）时 1064。
- **AUTO_INCREMENT**：id 对**每一条**待插入行先于冲突决策分配，故 ODKU 更新分支照样烧号
  （与 MySQL 的 gap 语义一致；见 `exec/sequence` 的 reserve-batch 分配）。
- **plain INSERT 不变（有意偏离，COMPAT.md 在案）**：pk 重复仍走静默 last-writer-wins
  upsert（StarRocks PRIMARY KEY 模型依赖），unique 命中仍 1062；ODKU 是显式冲突路径，两者并存。

### 决策点 1 选型 (b)：集群模式响亮拒绝（ER 1235）
ODKU/REPLACE 在决策时需要**冲突现存行**，而冲突读跑在协调者本地快照上——多节点集群里该行
（或其 unique 索引条目）通常落在别的 slot owner 上，决策会静默漏判。正确的集群冲突读是 2PC
前按 key 的 per-key gather（每个被探测 pk / unique 值 ≥1 次 RPC），超出计划对选型 (a) 设定的
“点读 ≤2 RPC”门槛，故 M2 选 (b)：凡 `cluster_spans_remote`（即写会变 2PC 的同一条件）即以
**ER 1235、消息含 "not supported in cluster mode"** 快速失败（每个节点都是转发边界，任何节点
发起点均可见），单机 / 单 band 集群不受影响。**plain `INSERT ... SELECT` 在集群下可用**：
SELECT 侧本就 scatter-gather 一次（`dist::gather`），行写与多行 INSERT 一样走 2PC；列存表
对 ODKU/REPLACE/INSERT…SELECT 一律 ER 1235（append-only，与 UPDATE/DELETE 同门）。

## e2e（tests/，本工作区新增）
- `tests/sql_upsert_common/mod.rs`（122）：三套件共享脚手架（errno 常量、leader-retry DDL、
  **立即回读**的 affected 探针 `aff`——错误语句会清掉计数器、服务端错误探针、文本协议
  cell 解码、per-test 独立 bootstrap 节点）。
- `tests/sql_upsert_e2e.rs`（5 用例）：affected 1/2/0 全矩阵（pk 冲突 changed/identical、
  unique 冲突 changed/identical、多行混合 [new, changed, identical]→3 + 落库行校验）；
  `VALUES()` 引用与 `c = c + VALUES(c)` 算术、未赋值列保持、改 pk ODKU + unique 索引迁移
  （按索引查新 pk、老 pk 复用、无残 entry）；REPLACE（pk 单冲突 2、双 unique+pk 重叠 3、
  无冲突 1、多行 REPLACE 3、索引可查可拒 1062）；txn 内 ODKU/REPLACE/INSERT…SELECT staged
  可见 + ROLLBACK 恢复行数与旧值；kill -9 重启后冲突路径写入、unique 索引（1062 拒绝）与
  冲突判定（identical→0 / REPLACE→2）全部存活。
- `tests/sql_insert_select_e2e.rs`（3 用例）：同表 `INSERT INTO sd SELECT id+100, v FROM sd`
  快照精确翻倍（两轮，9 行）；跨表 + WHERE；聚合 upsert（首轮 6 = 1+2+1+2，复跑 8 = 4×2，
  total 累加）；INSERT…SET（普通/表达式/ODKU 组合）；AUTO_INCREMENT ODKU 更新分支烧号
  （下一 id 严格大于被烧 id）；负矩阵（列存 ODKU/REPLACE/INSERT…SELECT 1235+“columnar”、
  `VALUES()` 出现在 INSERT 元组 1064 / UPDATE·DELETE·SELECT 位置 1235 unknown function、
  未知赋值列 1054、REPLACE+ODKU 组合 1064、plain INSERT pk 静默 upsert + unique 1062）。
- `tests/sql_upsert_cluster_e2e.rs`（1 用例，三进程集群）：决策 1b 落地——ODKU / REPLACE /
  ODKU-on-SELECT 从**每个**节点发起均 1235 且消息含 "not supported in cluster mode"、
  不落地任何行；plain 路径完好——跨 band 多行 INSERT + `INSERT INTO ci SELECT ... FROM cs`
  走 2PC 提交（affected 30、三节点 gathered 读一致）、unique 索引跨 band 1062 拒绝且原行
  完好、plain INSERT pk 静默 upsert 过 2PC 且全局可见。

## 验证
- `cargo test --test sql_upsert_e2e --test sql_insert_select_e2e --test sql_upsert_cluster_e2e`：
  **9/9 绿**（5 + 3 + 1，各两轮复跑）；`cargo fmt --check` 干净。
- 单测（`exec/upsert_tests.rs`、`exec/insert_select_tests.rs`、`parse/translate_dml_tests.rs`）
  为落地工作区自带；全量 `cargo test --workspace` 以落地提交的 CI 运行为准。

## 关联与残留
- 计划：`plans/2026-10-06-mysql-gap/m2-dml-conflicts.md`（B 组 P0/P1 销账；集群冲突读 gather
  方案留待 gap-matrix P2 重估）。后续：M3 子查询/集合 → M4 DDL/session → M5 e2e 盲区。
- 文档同步（`COMPAT.md` 冲突路径语义与集群偏离、`agents/rust/sql.md` 模块地图、
  `features/sql-dataplane.md`、`features/e2e-coverage.md`）随 M2 收尾提交一并补齐。
- e2e 钉死的实现侧注意点：affected 计数器是连接级状态，错误语句会将其清零（客户端语义，
  非服务端问题）；`INSERT ... SET` + 源（VALUES/SELECT）为 1064 parse error。
