# M3 — 子查询与集合操作（EXISTS / 相关子查询 / INTERSECT / EXCEPT）

对应 `gap-matrix.md` C 组。前置：M1（表达式求值基建统一到 `exec/func/` + `exec/expr.rs` 的
`eval`，相关子查询的外层行绑定复用同一求值入口）。执行器侧文件余量充足，无拆分动作。

## 目标

1. `EXISTS` / `NOT EXISTS`（非相关 + 相关）：翻译为独立 IR 节点，求值返回布尔（相关时按外层
   行逐次求值，命中首行即短路）。
2. 相关标量子查询 / 相关 IN 子查询：内层可引用外层 FROM 作用域列（`outer.col` 绑定）；相关
   标量子查询多行结果仍按现有"subquery returns more than 1 row"错误路径。
3. `NOT IN` 空集与 NULL 陷阱语义用例固化（`x NOT IN (empty)` → 无行返回被剔除；
   `x NOT IN (1, NULL)` → 永不为真）。
4. `INTERSECT [DISTINCT|ALL]` / `EXCEPT [DISTINCT|ALL]`：复用 `set_ops.rs` 既有 UNION 去重与
   列宽化基建；列数/类型检查与 UNION 同规则；结果列名取左操作数。
5. `WITH RECURSIVE` 维持拒绝，但把"deferred + 理由"写进矩阵与 COMPAT.md（P2）。

## 现状与证据

- `src/sql/exec/subquery.rs`（207 行，余量充足）：`rewrite_expr`（:58）把**非相关** IN 子查询
  重写为字面量集合（`rows_literals` :205），`rewrite_query`（:151）作用于整条查询；
  `scalar_of`（:185）处理非相关标量（恰好一行一列）。无 EXISTS 分支、无外层绑定。
- `src/sql/exec/set_ops.rs`（398 行）：`run_compound`（:27）支持 CTE（非递归）+ UNION [ALL]，
  含去重、数值列宽化、左操作数列名、compound 尾部 ORDER BY/LIMIT（`apply_tail`）。
- 拒绝证据：`src/sql/parse/mod.rs` :400-403 断言 EXCEPT/INTERSECT/WITH RECURSIVE 报错——
  本里程碑改写这些断言；`src/sql/parse/query.rs` :21-22 拒绝 recursive。
- 相关性判定：需要"子查询自由变量 ⊆ 内层作用域"的检测；`exec/scan.rs` 的 `check_expr`
  （FROM scope 校验）可复用为失败即"含外层自由变量"的探测。

## 实现拆分（文件与职责，标注行数预算）

- `src/sql/parse/ast.rs`（317 行，+~20）：`Expr::Exists { query, negated }`；`Expr::Subquery`
  增加相关标记（或由执行期自由变量检测决定，AST 不存——取后者，AST 只加 Exists）；
  `QueryBody` 增加 `Intersect { all }` / `Except { all }` 变体（或复用既有集合操作枚举形态，
  以 `translate_body` 现状为准）。
- `src/sql/parse/query.rs`（255 行，+~25）：`translate_body` 放开 INTERSECT/EXCEPT 形态；
  `parse/mod.rs` 旧拒绝断言改写为接受断言。
- `src/sql/parse/expr.rs`（M1 后 ≤450，+~15）：EXISTS/NOT EXISTS 翻译。
- `src/sql/exec/subquery.rs`（207 → 预算 ≤400）：
  - `rewrite_expr` 增加 `Expr::Exists` 分支：先按非相关尝试（一次物化、字面量化）；检测到
    外层自由变量则**保留 IR 节点**，由求值期处理；
  - 相关求值：`eval` 增加外层行环境（外层 `FromScope` + 当前行作为嵌套求值上下文传入，
    纯函数式——环境是显式参数）；相关 IN 复用同一绑定；EXISTS 内层加 `LIMIT 1` 等价短路。
- `src/sql/exec/set_ops.rs`（398 → 预算 ≤480）：
  - `intersect` / `except`：基于 `Value` 行键的哈希/有序集合运算，DISTINCT 走既有去重路径，
    ALL 保留重复（`multiset` 计数语义）；列宽化沿用 UNION 规则（取更宽类型）；
  - 集合操作结合性按 sqlparser 产出的嵌套形态逐层求值（MySQL 8.0 语义：左结合，INTERSECT
    优先于 EXCEPT/UNION——以 sqlparser AST 结构为准，文档记录选择）。
- `src/sql/exec/select.rs`（550 行，+~15）：WHERE/HAVING 求值路径把外层行环境传入
  `eval`（签名扩展或经 `FromScope` 携带，二选一以改动面小者为准）。

## 单测

- `exec/subquery.rs` 内嵌测试扩展（或拆 `subquery_tests.rs` ≤400）：
  - 非相关 EXISTS（空/非空集合）、NOT EXISTS；
  - 相关 EXISTS（`WHERE EXISTS (SELECT 1 FROM o WHERE o.id = i.id)`）；
  - 相关标量（`SELECT (SELECT MAX(x) FROM t2 WHERE t2.k = t1.k) FROM t1`）；
  - NOT IN 空集（结果空）与含 NULL（结果空）陷阱；
- `parse/mod.rs`：INTERSECT/EXCEPT/ALL/DISTINCT 形态翻译、递归 CTE 仍拒绝（保留 P2 断言）；
- `exec/set_ops.rs` 测试：INTERSECT DISTINCT/ALL、EXCEPT DISTINCT/ALL、与 UNION 混合嵌套、
  列宽化（INT ∩ DOUBLE）。

## e2e

新文件 `tests/sql_subquery_e2e.rs`（预算 ≤400）：

- 非相关 EXISTS/NOT EXISTS 计数场景；
- 相关 EXISTS 半连接（订单/用户类双表）；
- 相关标量子查询（含 NULL 结果传播）；
- NOT IN 空集与 NULL 陷阱两条负路径；
- 集合组合：`UNION ALL + INTERSECT + EXCEPT` 嵌套、DISTINCT/ALL 对照、列类型宽化、
  尾部 ORDER BY/LIMIT 作用域；
- 复用 M5 `tests/common/mysql.rs` 样板。

## 文档同步

- `COMPAT.md`：子查询支持范围（相关/非相关矩阵）、INTERSECT/EXCEPT 语义与结合性选择、
  WITH RECURSIVE deferred 理由；
- `agents/rust/sql.md`：`subquery.rs` 相关求值环境说明；
- `features/sql-dataplane.md`、`features/e2e-coverage.md`、`features/changelog/<日期>/`。

## 验收

- C 组 P0/P1 标记"已有"；WITH RECURSIVE 保持 P2 + deferred 理由；
- `subquery.rs` ≤400、`set_ops.rs` ≤480；
- `cargo test --workspace --no-fail-fast` 全绿（含 `sql_setop_cluster_e2e.rs` 既有 UNION
  集群用例回归）。

## 风险

- 相关子查询逐行重求值的性能（无缓存/去关联）：v1 接受 N+1 求值（行存点查/扫描快），不尝试
  子查询去关联重写——复杂度不成比例，记入文档；
- 外层行环境扩展 `eval` 签名的传染面（M1 刚拆完函数注册表）：若传染过广，改用
  `FromScope` 携带外层行（zero-copy 引用），避免全局签名震荡；
- 集合操作在集群 scatter-gather 下的语义：INTERSECT/EXCEPT 需要**全量两侧**数据在单点合并
  （compound 已按"每操作数物化"实现，天然满足），确认不引入跨节点流式假设。
