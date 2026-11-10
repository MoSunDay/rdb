# 2026-11-10 · turso 引擎 allowlist batch 1（JOIN / 子查询 / 集合运算 / CTE 开闸）

来源计划：`plans/2026-11-09-turso-engine`（landed 后续批次）；契约与矩阵、
新增偏差 (z1)/(z2)：`COMPAT.sql.md` "turso engine" 节；e2e 台账：
`features/e2e-coverage.md`。

## 开了什么

rdb AST 早已全量解析这些形状，turso 翻译层此前一律 1235；本批把渲染器
扩成：

- **JOIN**（新 `src/sql/turso/translate/join.rs`）：`TableRef` join 树 →
  SQLite —— `INNER/LEFT/RIGHT/FULL OUTER/CROSS JOIN`（裸 `JOIN` = INNER），
  `ON expr` 或 `USING (cols)`。**接线前逐 kind 冒烟验证过 turso 0.9.0-pre.2**
  （同版本 crate 的独立 scratch 二进制）：五类 join 全部原生执行（planner
  实装 `JoinType::{RightOuter,FullOuter}`），无 per-kind 1235 残留。
- **子查询**（`translate/expr.rs` 三个新 arm）：`(SELECT …)` 标量、
  `[NOT] EXISTS (…)`、`x [NOT] IN (SELECT …)`；内层走 `set.rs` 的复合渲染器
  （递归）。相关子查询由 SQLite 原生解析（e2e 只断言结果 parity，不比计划形态；
  rocks 侧的 `correlated_map` 物化机制不适用也不需要）。
- **集合运算 + CTE**（新 `translate/set.rs` + `route.rs` 撤掉整句 1235 arm）：
  `UNION [ALL] / INTERSECT / EXCEPT`、`WITH name [(cols)] AS (…)`（可链式）、
  外层 ORDER BY / LIMIT / OFFSET。两个引擎侧事实，冒烟确认后落成渲染决策：
  - SQLite 复合 SELECT 语法**不接受括号操作数**（`near "(": syntax error`），
    `QueryBody::Nested` 改渲染成派生表包裹 `SELECT * FROM (…)` —— 保形且是
    标准 SQLite；括号外的链 turso 与 rdb 的 sqlparser 树同为等优先级左折叠，
    平铺即同树。
  - `INTERSECT ALL` / `EXCEPT ALL` 不在语法里（ALL 仅随 UNION）→ 维持 1235
    （偏差 (z2)；rocks 侧多重集语义照答）。

## 验证

- 新 `tests/turso_setops_e2e.rs`（1 用例）：同 schema 同语句双引擎
  （`USE rdb` vs `USE turso_setops`）——JOIN 矩阵、USING×各 kind、别名/
  限定列/链式 join、子查询七型、UNION vs UNION ALL 去重、INTERSECT/EXCEPT、
  括号操作数、无优先级混合链、首分支列名 parity、CTE 四型、事务内集合读；
  引擎侧断言 (z1)/(z2) 与派生表 1235。
- 先行套件两处与本次翻转**直接冲突**的旧断言随批更新（不弱化其余）：
  `turso_smoke_e2e` 的 `SELECT 1 UNION SELECT 2` 1235 探针换成仍拒绝的
  `INTERSECT ALL`；`turso_crud_e2e` 1235 矩阵摘除 JOIN/UNION/IN 子查询三行。
- 全绿：`cargo build --features full`；七套 turso e2e + `sql_e2e`；
  `cargo test --lib`；`cargo check --no-default-features --features store`；
  `cargo fmt --check` / clippy（本批文件零告警）。

## 新偏差（COMPAT.sql.md，续 (z) 后按 (z1)/(z2) 后缀式编号——文件从未用过
多字符字母，沿用其单字母空间的编号精神）

- (z1) USING join 下的 `SELECT *`：turso 合并 join 列（MySQL 形，列出现一次），
  rocks 展开两侧全部列（join 列出现两次）；裸引用该列 turso 无歧义、rocks
  1054 ambiguous。
- (z2) `INTERSECT ALL` / `EXCEPT ALL`：rocks 多重集语义照答，turso 1235。

## 残留 1235（矩阵已注明）

`INTERSECT ALL` / `EXCEPT ALL`（引擎语法）、派生表 `FROM (SELECT …)`、
`SELECT DISTINCT`、锁子句、`WITH RECURSIVE`（parse 侧）、逗号 cross join
（parse 侧）、子查询做函数实参（parse 侧）。
