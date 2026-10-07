# SQL M3：子查询与集合操作（EXISTS / 相关子查询 / INTERSECT / EXCEPT）

Commit: (working-tree, 随本提交入库)

## 背景
MySQL 兼容差距计划（`plans/2026-10-06-mysql-gap/`）C 组定位出两块缺口：
- `exec/subquery.rs`（M2 时 207 行）只会把**非相关** IN 子查询重写为字面量集合、非相关标量
  折叠为常量——`EXISTS` 无分支，外层列引用（相关性）无处绑定；
- `exec/set_ops.rs` 只有 `UNION [ALL]`，`parse/mod.rs` 的单测断言 `INTERSECT` / `EXCEPT` /
  `WITH RECURSIVE` 一律报错。
M3 在不动 `eval` 签名的前提下补齐两块（M1 刚拆完函数注册表，传染面要收敛）。

## 修复内容（src/sql，待提交工作区）

### 新增 `exec/correlated.rs`（328）+ `exec/correlated_map.rs`（200）
两趟设计，相关性对求值器不可见：
- **第一趟（defer，`subquery::rewrite_query`，`SubqCtx::outer == None`）**：非相关子查询照旧
  折叠；内层名解析失败（“unknown column”且该名能对上外层作用域）→ 该节点**原样保留**为
  deferred，错误被吞掉（typo 会在第二趟原样复现，不做静默猜测）。
- **第二趟（bind，`correlated::bind_query`，`select.rs:95` 在外层 FROM 物化后调用）**：对每个
  deferred 节点先用 `correlated_map::map_compound` 收集**自有表达式位置**（投影 / WHERE /
  GROUP BY / HAVING / ORDER BY / JOIN 条件；嵌套子查询体不进）上的自由变量，按外层行求值成
  key 元组，**按 distinct key 记忆化**（重复外层键只求值一次）；每个 distinct 键把全部外层
  引用**字面量替换**进子查询副本后经 `run_compound` 独立求值，产出 `(key, CorrelatedOut)`
  表，节点换成纯数据 `Expr::Correlated { kind, keys, cases }`——求值期零存储访问。
- **shadow 判定**（`shadow_sides`）：子查询自身 FROM 的每个基表侧（含别名与 catalog 列名）
  先遮蔽同名引用，避免把内层表自己的列误判成外层引用；派生表不透明（见窄拒绝）。
- 嵌套层级天然链式：每层用自己的外层行绑定，跨层（skip-level）引用不可链 → 响亮报错。
- `correlated_map.rs`（200）是共享的列引用重写器（检测与替换同一实现，`map_compound` /
  `map_expr`），供 `outer_refs` 与 `substitute_refs` 复用，避免两份遍历逻辑漂移。

### `exec/subquery.rs`（207 → 361）
- 新增 `Expr::Exists` 分支（非相关：任一行即 TRUE，折叠为布尔字面量）与**相关 defer 路径**
  （`is_correlation` 判定 + `OuterRows { scope, rows }` 把第二趟切换为 bind）；
- `rewrite_from`：JOIN 条件里的子查询照常重写，但**仍相关**的立即拒绝（JOIN 条件在 FROM
  物化期间求值，那时外层行还不存在）；
- `run_subquery` 把执行器的 “unknown column” 翻译成相关性判决（1235 + “outer reference；
  原消息”），已带 NotSupported 的嵌套判决与歧义名错误不再包裹。
- 随附 `subquery_tests.rs`（338）：非相关 IN/标量回归、相关标量 / IN / EXISTS（含空集 →
  NULL、多行 → 1242 风格错误）、两层嵌套、setop 臂内相关、JOIN 条件拒绝、typo 双向诚实报错。

### `exec/set_ops.rs`（398 → 386，净减：去重路径收敛）
- `merge_relations` 从 UNION 专用推广为四算子共用：列数不匹配 → 1235 且**消息点名算子**
  （“INTERSECT/EXCEPT/UNION operands yield different column counts (l vs r)”）；数值列宽化
  （INT|DOUBLE → DOUBLE、DATE → DATETIME）与“两侧行先按合并列型归一化再比较”（`1 = 1.0`、
  Date = 午夜 DATETIME）、左操作数列名、列 nullable 取或——全部沿用 UNION 既有规则；
- `combine_rows` 四象限：`UNION ALL` 直拼、`UNION` 排序去重；`INTERSECT`/`EXCEPT` DISTINCT
  = 左侧去重后按右侧有序集过滤（保留左首现顺序）；ALL = `multiset()` **多重集算术**——
  左侧 run-length 编码后每 distinct 行发射 `f(count_left, count_right)` 次（INTERSECT 取
  `min`、EXCEPT 取 `saturating_sub`），右侧出现次数用排序 + partition_point 计数，NULL 参与等值
  （与去重路径一致）；ALL 结果保持左侧首现顺序。
- 随附 `set_ops_tests.rs`（351）：四算子 × DISTINCT/ALL、多重集计数、arity / 宽化 /
  Date↔DATETIME 单元级断言。

### 翻译与 IR（`parse/`）
- `parse/expr.rs`：sqlparser 的 `Exists` / `InSubquery` → `Expr::Exists { query, negated }` /
  `Expr::InSubquery`；`parse/query.rs`：`SetOperator::Intersect|Except` 进 IR，`Minus` 与
  `BY NAME` 量化符仍 1235（消息点名 “Minus set operations” / “ByName set quantifier”），
  `WITH RECURSIVE` 仍 1235（P2 deferred）。
- `parse/ast.rs`（554）：`Expr::Exists`、`Expr::Correlated { kind, keys, cases }`、
  `CorrelatedKind { Scalar | Exists{negated} | In{lhs,negated} }`、`CorrelatedOut { Scalar |
  Rows }`（纯数据，注释写明 miss 时的 SQL 缺省：NULL / false / 空集）。
- `exec/expr.rs`（740）`eval` 只加一个 `Expr::Correlated` 分支：按 key 查表，`Scalar` miss →
  NULL、`Exists` → 布尔（支持 NOT EXISTS 翻转）、`In` 复用既有 `in_predicate`（三值逻辑：
  NULL 成员使 NOT IN 永不为真）——签名不变，M1 的函数注册表无传染。

### 语义（MySQL 对齐）
- **EXISTS / NOT EXISTS**：与内层投影无关，任一行即真；SELECT 列表里投影为 0/1 单元格。
- **相关标量子查询**：单列；空 → NULL（外层行照常输出 NULL）；>1 行 → “Subquery returns
  more than 1 row”（1242 风格，绑定期报错）；内层可带 GROUP BY / HAVING / 聚合、可进
  CASE / COALESCE / 函数实参。
- **相关 IN 子查询**：`lhs` 在**外层**按行求值，绑定贡献成员集；`NOT IN` 空集 → 全行保留、
  含 NULL 成员 → 永不为真（三值），相关版本按外层行逐行建成员集。
- **INTERSECT / EXCEPT [DISTINCT|ALL]**：DISTINCT 去重（NULL 相等），ALL 按出现次数取
  min / 做减法；**INTERSECT 结合性高于 UNION / EXCEPT，同级从左折叠**（标准 SQL；e2e 用
  `4 UNION 3 INTERSECT 3` = {3,4} 与加括号的左折叠读法做差分钉死）；列名取左操作数。
- **窄拒绝（全部响亮，绝不静默）**：相关 JOIN 条件（“correlated subqueries are not
  supported in JOIN conditions”，1235）；子查询派生表内的外层引用（“outer reference”1235）；
  skip-level 引用（诚实 1054，点名无法绑定的列）；`WITH RECURSIVE` / `MINUS` / `BY NAME`
  仍 1235。
- **v1 取舍**：相关求值按 distinct 外层键记忆化，但不做去关联重写（N+1 点查/扫描被接受，
  行存路径快）；无性能断言，e2e 只钉正确性。

## e2e（tests/，本工作区新增 / 增补）
- `tests/mysql_compat_e2e.rs`（M3 增补的既有断言，随实现一并落）：`INTERSECT` / `EXCEPT` / `EXCEPT ALL`
  的 DISTINCT/ALL 基本形态、混合链（`(t<3 UNION ALL u.ref_id) EXCEPT t<1`）左折叠结果、
  尾部 `ORDER BY 1 LIMIT 2 OFFSET 3`、EXPLAIN compound 计划、相关 IN / EXISTS / 标量子查询
  （空 → NULL）、`NOT IN` 空子查询全行保留、相关 JOIN 条件与 `WITH RECURSIVE` 拒绝、CTE 体内
  子查询。
- `tests/sql_subquery_e2e.rs`（**新增**，319 行，10 用例，`common::mysql` 脚手架 + 套件级
  fixture：usr/ord/pay/ghost/dup/sel/nul/tag/evt/city 十表）：
  1. `uncorrelated_exists_counts_and_projects`：命中 / 未命中、NOT EXISTS、空内表翻转、内层
     WHERE 裁决、SELECT 列表 0/1 投影；
  2. `correlated_scalar_projection_and_filter`：SELECT 列表按行 SUM（空 → NULL）、内层
     GROUP BY 聚合同值、COALESCE 兜底、WHERE 裸用与算术内嵌；
  3. `correlated_exists_semi_and_anti_join`：半连接、反连接找孤儿、带内层谓词的反连接、
     相关 IN（成员集随外层行移动）、外层别名引用；
  4. `two_level_nesting`：EXISTS 套 EXISTS、标量套标量（内层空 → NULL → UNKNOWN → 该行
     被剔）；
  5. `not_in_traps_empty_null_and_correlated`：空集全保留、含 NULL 成员零行（IN 仍答精确
     匹配）、相关 NOT IN 逐行成员集（u1 空集存活、u2..4 含 NULL 永不为真）；
  6. `intersect_and_except_multiset_semantics`：dup={1,2,2,3,3,3} vs sel={2,2,3} 的
     DISTINCT/ALL 计数精确钉死（INTERSECT ALL = {2,2,3}、EXCEPT ALL = {1,3,3}）、自交恒等、
     右空集 = 左操作数；
  7. `mixed_chain_precedence_and_arity_errors`：五算子混合链按“INTERSECT 更紧 + 同级左折叠”
     求值并注释推导、`1 EXCEPT 2 UNION 3` 同级左折叠、加括号对照、三种算子的列数不匹配错误
     各自点名；
  8. `setops_over_group_by_and_aggregate_arms`：整行比较（`(d,3) ≠ (d,1)`）、聚合臂 + 字面量
     臂的 EXCEPT ALL / INTERSECT ALL 多重集算术、裸聚合臂参与；
  9. `subqueries_interplay_with_m1_functions`：`EXISTS (... GROUP BY DATE(d) HAVING
     COUNT(*) > n)`、标量子查询作函数实参、`IN (SELECT UPPER(...))`、标量进 CASE；
  10. `narrow_shapes_reject_loudly`：WITH RECURSIVE / MINUS / BY NAME / 相关 JOIN 条件 /
      派生表外层引用（1235 各自点名）、skip-level（1054 点名列）、多行标量（非相关 + 相关
      两条路径）。
  断言经 brace 宏（`col!` / `rows_!` / `err!` / `unknown!`）一例一行，行预算内保持可读
  （rustfmt 会把长调用链纵向展开）。

## 验证
- `cargo test --test sql_subquery_e2e`：**10/10 绿**（两轮复跑）；`cargo test --test
  mysql_compat_e2e`：**4/4 绿**；`cargo fmt --check` 干净。
- 单测（`exec/subquery_tests.rs` 338、`exec/set_ops_tests.rs` 351、`correlated.rs` / `ast.rs`
  内联断言）为落地工作区自带；全量 `cargo test --workspace`（含 `sql_setop_cluster_e2e.rs`
  集群 UNION 回归）以落地提交的 CI 运行为准。

## 关联与残留
- 计划：`plans/2026-10-06-mysql-gap/m3-subquery-setops.md`（C 组 P0/P1 销账；`WITH RECURSIVE`
  维持 P2 + deferred 理由）。后续：M4 DDL/session → M5 e2e 盲区。
- 残留：相关子查询无去关联 / 无跨键共享子计划（接受 N+1）；INTERSECT / EXCEPT 无优先级
  文档化于 COMPAT.md 的同步（随 M3 收尾提交一并补齐 `COMPAT.md`、`agents/rust/sql.md`、
  `features/sql-dataplane.md`、`features/e2e-coverage.md`）。
- e2e 钉死的实现侧注意点：混合集合链的**优先级来自 sqlparser 的标准 SQL 文法**
  （INTERSECT 更紧），不是 `exec::set_ops` 自行决定——`SetExpr::SetOperation` 树形已是
  该结合性；改优先级要动翻译层，不是执行层。
