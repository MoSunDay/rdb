# 查询语言面：UNION / CTE / 派生表 / 子查询（MySQL 兼容 Phase 1）

## 背景
Phase 1 计划 1.1/1.2/1.3：此前 sqlparser 能解析 UNION/WITH/IN (SELECT)/FROM 子查询，
但翻译层把整类语句静默拒成 1235 NotSupported，或更糟——FROM-less SELECT（`SELECT 1`）
直接报错。MySQL 客户端最常见的复合查询形态全部不可用。本次补齐执行语义，保持
"不支持就大声拒绝"的既有原则。

## 实现
- **IR**（`parse/ast.rs`）：新 `Statement::SelectCompound(Box<CompoundQuery>)`；
  `CompoundQuery { ctes, body, order_by, limit, offset }`；`QueryBody::{Select(Box<Query>),
  Nested(Box<CompoundQuery>), Union{left,right,all}}`；`Cte { name, column_aliases,
  query }`；`TableRef::Derived{query,alias}` 与 `TableRef::NoTable`（FROM-less SELECT
  从此合法：单行空行 × 投影）。表达式层新增 `Expr::Subquery`（标量子查询）与
  `Expr::InSubquery{expr,query,negated}`。
- **翻译**（新 `parse/query.rs`，255 行）：`translate_compound`（WITH：仅非递归，别名
  列表改名）、`translate_body`（仅 UNION——INTERSECT/EXCEPT/MINUS 与 BY NAME 量词
  均按 1235 大声拒绝；锁子句只允许出现在孤立 SELECT 上）、`translate_select`（SELECT
  核心，无尾部）；`parse/translate.rs` 按"无 WITH 的普通 SELECT"走原快路径，其余分发
  到复合路径；占位符计数/绑定遍历覆盖 compound/CTE/派生表/子查询。
- **关系代数**（新 `exec/relation.rs`）：`Relation{columns, rows: Arc<Vec<_>>}` 与
  `CteScope`（大小写不敏感、后定义遮蔽前定义）。
- **子查询**（新 `exec/subquery.rs`）：语句开始时一次性改写——标量子查询物化后提升为
  字面量、IN 子查询提升为 `InList`；未知列失败统一映射为 "correlated subqueries are
  not supported"（关联子查询不支持，1235）。改写点收敛在 `select::run_at` 单一咽喉，
  普通 SELECT 路径同样受益。
- **执行**（新 `exec/set_ops.rs`）：`run_compound` 逐个物化 CTE 入 scope；每个 UNION
  操作数经 `select::run_at` 走完整管线（含集群 scatter-gather），协调者侧再拼装——
  集群模式下复合查询的每个操作数都是全量 gather，不存在"只读本节点切片"的静默降级。
  `union_relations`：列数校验、INT|DOUBLE→DOUBLE 数值拓宽、plain UNION 去重（NULL
  相等，同 MySQL）、列名取左操作数。
- **扫描/EXPLAIN**：`scan.rs::materialize` 改 async 并带 `&CteScope`（CTE 名先于
  catalog 查找）；`dist/gather.rs` 同签名（CTE 永不 gather，Derived/NoTable 不扇出）；
  `render.rs` 输出 CTE/Union-Distinct/Union-All/Sort/Limit 的复合 EXPLAIN 行。

## 明确不支持（1235 大声拒绝）
- INTERSECT / EXCEPT / MINUS；`UNION [DISTINCT] BY NAME` 等量词变体；
- `WITH RECURSIVE`；
- 关联子查询（引用外层列）；
- LATERAL 派生表；派生表必须有别名。
- 复合查询尾部 ORDER BY 支持序数（`ORDER BY 2`），与 MySQL 一致。

## 验证
- 单元：`set_ops.rs` 内嵌 concat/dedup/拓宽/列数校验；`scan_tests`/`gather_tests`
  适配 async materialize 新签名。
- `tests/mysql_compat_e2e.rs` 新增 UNION [ALL]/去重含 NULL/CTE 改名遮蔽/派生表/
  标量与 IN 子查询/FROM-less SELECT/EXPLAIN 输出端到端用例。
- 新增 `tests/sql_setop_cluster_e2e.rs`（3 进程集群）：列存 3 节点各 5 段 + 行存单次
  2PC 散布 + oracle 视图推进（沿 `sql_join_cluster_e2e.rs` 既有约定），任一节点发起
  `SELECT ... UNION SELECT ...`（列存∪行存）、UNION ALL 重复计数、CTE 聚合再 UNION
  均返回全量一致结果。
- 全量门禁：fmt / clippy -D warnings / `cargo test --workspace`（924 通过）/ release
  构建全绿。
