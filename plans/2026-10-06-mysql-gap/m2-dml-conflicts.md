# M2 — DML 冲突路径（ODKU / REPLACE / INSERT…SELECT / INSERT…SET）

对应 `gap-matrix.md` B 组。前置：M1（`exec/func/` 拆分与 VALUES() 内表达式求值复用同一套
求值基建）。**第一动作是拆分 `src/sql/exec/write.rs`**——749 行已逼近 800 上限。

## 目标

1. `INSERT ... ON DUPLICATE KEY UPDATE`：冲突（PK 或唯一索引）时走更新分支；UPDATE SET 右值
   支持 `VALUES(col)` 引用（引用待插入行的值）；affected rows 语义 = 插入 1 / 冲突且值有变 2 /
   冲突且值未变 0（MySQL 语义，客户端默认 `useAffectedRows=0` 时不变——以服务端 1/2/0 为准）。
2. `REPLACE INTO`：冲突先删后插，affected rows = 删 1 + 插 1 = 2（多冲突键按 MySQL 取 2 的
   常见路径，v1 不展开多行删除计数）。
3. `INSERT ... SELECT`：SELECT 侧按只读快照物化（复用 `exec/relation.rs`），行构造走既有
   INSERT 路径；同表读写（`INSERT INTO t SELECT ... FROM t`）天然支持（读 ts < 写 ts）。
4. `INSERT ... SET col = expr`：解析为列清单 + 单行 VALUES，行为与显式列表等价。
5. 保留**PK 重复 INSERT 静默 upsert**（有意偏离，决策点 2）；ODKU 是显式冲突路径，两者关系
   写入 COMPAT.md。

## 现状与证据

- `src/sql/parse/translate.rs`（727 行）`translate_insert`（:584）：`on.is_some()` →
  unsupported "ON DUPLICATE KEY UPDATE / ON CONFLICT"（:585-587）；`assignments` 非空 →
  unsupported "INSERT ... SET"（:590）；`replace_into` → unsupported "REPLACE INTO"（:593）。
  这三个拒绝分支即本里程碑的 IR 化改造点。
- `src/sql/exec/write.rs`（749 行）：行构造、PK 编码、列存写入、AUTO_INCREMENT（配合
  `exec/sequence.rs`，468 行）都在此文件；PK 冲突静默覆盖（StarRocks PK 模型依赖）。
- 唯一索引冲突：已按 MySQL 1062 拒绝（`tests/sql_types_e2e.rs` :371 有 e2e 断言）——ODKU 必须
  把"唯一索引冲突"从报错路径迁到冲突分支；索引条目维护在 `src/sql/index/`（`keys.rs` 270 行、
  `maintain.rs` 183 行）。
- 集群写路径：SQL 2PC（`src/sql/dist/twopc.rs` 324 行、`gather.rs` 384 行）；决策点 1 的
  gather-read 成本评估在此做。

## 实现拆分（文件与职责，标注行数预算）

- `src/sql/exec/upsert.rs`（新增，预算 ≤400）：
  - `on_duplicate(shared, sess, plan) -> SqlResult<ExecOutcome>`：冲突读（按 PK/唯一键点查旧行，
    走写者 ts 的读路径）→ 命中则求值 UPDATE SET（`VALUES(col)` 以"待插入行"为环境）→ 更新走
    既有行更新原语（索引条目迁移、MVCC 版本）；未命中走插入。纯函数式：计划与旧行都是显式
    入参，无内部状态机。
  - `replace_into(...)`：冲突读 → 删（索引条目清理）→ 插；affected=2。
  - affected rows 计数规则集中为一个纯函数 `affected(inserted, updated, changed)`。
- `src/sql/exec/insert_select.rs`（新增，预算 ≤350）：SELECT 物化（`relation_of` + 快照 ts 与
  写侧隔离）→ 逐批走 `write.rs` 的行构造路径；源行类型 → 目标列 coerce 复用 `expr::coerce`。
- `src/sql/exec/write.rs`（749 → 预算 ≤550）：抽出冲突/选择逻辑后保留行构造、列存写入、
  sequence 交互；对 upsert 模块暴露纯入口。`write_tests.rs`（561 行）相应拆用例。
- `src/sql/parse/translate.rs`（727 行，预算：若 `translate_insert` 改造后超 760 则把 DML
  翻译整体拆出 `src/sql/parse/translate_dml.rs`，预算 ≤350）：
  - `on` → IR `OnDup { assignments: Vec<(String, Expr)> }`，`VALUES(col)` 翻译为
    `Expr::Func { name: "values", args:[col] }` 保留到求值层；
  - `assignments`（INSERT…SET）→ 列清单 + 单行；
  - `replace_into` → `Statement::Insert { replace: true, .. }`（AST 扩展在 `parse/ast.rs`，
  +~20）。
- `src/sql/index/`（不新增文件）：唯一索引冲突检测入口从"报 1062"参数化为"回调冲突分支"，
  保持纯函数（冲突键集合作为返回值）。
- `src/sql/dist/`：ODKU 冲突读在集群下的位置——决策点 1 评估后二选一：
  (a) leader 端 gather-read 点查（复用 `gather.rs` 单键读）→ 保留集群 ODKU；
  (b) 集群大声拒绝 ER 1235 "ON DUPLICATE KEY UPDATE in cluster mode"，单机先行。
  评估标准：单语句 RTT 增量与 raft 日志体积可接受（量化：点查 ≤2 次 RPC 时选 a）。

## 单测

- `exec/upsert_tests.rs` / `exec/insert_select_tests.rs`（新增，各 ≤400）：
  - affected rows 矩阵：新插入=1 / 冲突更新有变=2 / 冲突更新无变=0 / REPLACE=2；
  - `VALUES()` 引用与混合常量（`c = VALUES(c) + 1`）；
  - 多行 VALUES 中部分冲突（第 1 行冲突、第 2 行新插）；
  - 唯一索引（非 PK）冲突触发 ODKU；索引条目迁移正确性（旧键条目消失）；
  - AUTO_INCREMENT：ODKU 更新分支不烧号、REPLACE 烧号（与 MySQL 对齐：REPLACE 走新行）；
  - 列存表 ODKU/REPLACE/INSERT…SELECT → ER 1235 大声拒绝；
  - 事务内 ROLLBACK：ODKU 更新与 REPLACE 均可回滚（行存 MVCC 路径）。

## e2e

新文件 `tests/sql_upsert_e2e.rs`（预算 ≤400）：

- affected_rows 断言用真实 client（`conn.query_drop` 返回的 affected）；
- `VALUES()` 引用、多行混合冲突、唯一索引重叠场景；
- txn：BEGIN → ODKU → ROLLBACK → 旧值可读；
- restart：`tests/sql_restart_e2e.rs` 模式（进程重启后冲突路径仍生效）；
- cluster：三进程（`process_cluster_e2e.rs` 样板）——若走决策点 (a) 验证 2PC 提交与冲突读
  一致性；走 (b) 验证 ER 1235 报错可见；
- 更新旧断言：全仓库 grep "ON DUPLICATE"/"INSERT ... SET"/"REPLACE INTO" 的既有拒绝断言
  （`parse/` 单测与 `mysql_compat_e2e.rs` 如有）改为新语义。

## 文档同步

- `COMPAT.md`：ODKU/REPLACE/INSERT…SELECT 语义、affected rows、集群决策结果、PK 静默 upsert
  与 ODKU 的关系（决策点 2）；
- `agents/rust/sql.md`：`exec/upsert.rs`、`exec/insert_select.rs` 模块图；
- `features/sql-dataplane.md`、`features/e2e-coverage.md`、`features/changelog/<日期>/`。

## 验收

- B 组 P0/P1 标记"已有"；PK 静默 upsert 在矩阵保持"有意偏离"且 COMPAT.md 可查；
- `exec/write.rs` ≤550、新文件各 ≤400；`cargo test --workspace --no-fail-fast` 全绿；
- 决策点 1 有书面结论（量化数据 + 选型）落在本文档追加小节。

## 风险

- ODKU 冲突读放大写路径延迟（单机点查可控；集群 gather-read 是主要不确定性——决策点 1）；
- 唯一索引 + ODKU 的索引迁移在并发事务下可能撞 first-committer-wins（1213）：保持快速失败，
  与现有写写冲突语义一致，不做锁等待；
- REPLACE 的删插非原子可见窗口（MVCC 下对外原子，但索引层两步）：依赖既有事务提交边界，
  e2e 并发用例覆盖读侧只见前或后；
- affected rows "无变=0" 需要旧新行比较，列存无法参与（已拒绝），行存按 Value 逐列比较即可。
