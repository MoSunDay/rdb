# SQL M0：查询语义修复（序数、别名解析、LIMIT ?、FROM DUAL）

Commit: (working-tree, 随本提交入库)

## 背景
MySQL 兼容差距计划（`plans/2026-10-06-mysql-gap/`）F 组 4 项 P0 定位出四个**静默错误或
错误拒绝**的查询语义缺口，不涉及执行器结构性改动，作为收敛计划第一个里程碑（M0）落地：

1. `ORDER BY 1` / `GROUP BY 1` 的裸整型被翻译成**常量排序键**——排序恒为 no-op，行序错且
   无任何提示；
2. ORDER BY / HAVING 引用 SELECT 别名按 unknown column 拒绝（MySQL 先查投影别名再回落 FROM）；
3. `LIMIT ?` / `OFFSET ?` 占位符被直接拒绝；
4. `SELECT 1 FROM DUAL` 报表不存在。

## 修复内容（src/sql，待提交工作区）
- 新增 `src/sql/parse/order_limit.rs`（+ `order_limit_tests.rs`）：排序/分组键与 LIMIT/OFFSET
  翻译从 `translate.rs` / `query.rs` 按职责拆出（文件规模预算，translate.rs 已近 800 行上限）：
  - **序数**：裸无符号整型 = 1-based 输出列位置，替换为对应投影表达式；越界 / 0 按 MySQL
    ER 1054 语义报 `Unknown column 'N' in 'order clause'/'group statement'`；序号指向 `*`
    投影（宽度执行前未知）或含 `?` 的投影（替换会复制参数、错位绑定）→ 大声 NotSupported；
    `'1'`、`-1`、`1+1` 等非裸数字保持常量语义。
  - **别名**：`substitute_aliases` 把 ORDER BY / HAVING 中的裸标识符按大小写不敏感匹配
    SELECT 别名并代入投影表达式（别名优先于同名 FROM 列，与 MySQL 常见行为一致；不做二次
    替换，别名不能成链成环）；无匹配仍走 FROM scope 校验，报 unknown column（1054）。
  - **LIMIT ? / OFFSET ?**：以 `LimitValue::Param` 随语句带到执行期，绑定后强转 u64；
    负数 / 小数 / 字符串绑定报 `LIMIT must be a non-negative integer`（1064），prepare 期
    不校验（与 MySQL 协议行为对齐）；占位符计入 `placeholder_count`，二进制协议
    limit-then-offset 顺序绑定。
- `src/sql/parse/table.rs`：`FROM DUAL`（单段名、无别名、大小写不敏感）归一为
  `TableRef::NoTable`，与无 FROM 完全同路径；`db.dual`、`dual d`、`dual2` 仍是普通表名。
- 两组静默错误（序数、别名）清零；行为由 `parse/order_limit_tests.rs` 与
  `exec/select_tests.rs` M0 段单测钉住。

## e2e
- 新增 `tests/sql_query_semantics_e2e.rs`（400 行，6 用例，进程级真实 MySQL 协议进程）：
  - 序数回归：`ORDER BY 2` ≡ `ORDER BY b`、表达式投影 `ORDER BY 1 [DESC]` 真实排序，且
    `'1'` / `-1` / `1+1` 常量键为 no-op（与序数结果顺序不同，钉死"序数不再退化"）；
  - `GROUP BY 2` 分组聚合（与 `GROUP BY tag` 等价断言）+ 表达式投影 `GROUP BY 1`；
  - ORDER BY / HAVING 别名（`a+b AS s`、聚合别名 `cnt` 大小写不敏感、别名胜出同名源列、
    未知名仍 1054）；
  - `prep()+exec()` 二进制协议：`LIMIT ?`、`LIMIT ? OFFSET ?`、常量 LIMIT + `OFFSET ?`，
    及负 / 小数 / 字符串绑定的 1064 拒绝；文本协议字面量不受影响；
  - FROM DUAL：`SELECT 1 FROM DUAL` / `dual` / `VERSION() FROM DuAl`、prepared
    `WHERE 1=1 LIMIT ?`、`dual2` 仍报 1146；
  - 负矩阵：`ORDER BY 5` / `ORDER BY 0`（1054 措辞断言）、`SELECT * ... ORDER BY 1`
    （1235）、序号指向 `?` 投影（prepare 期 1235）。

## 验证
- `cargo test --test sql_query_semantics_e2e`：6/6 绿（两轮复跑）；`cargo fmt --check` 干净。
- 全量 `cargo test --workspace` 以落地提交的 CI 运行为准。

## 关联与残留
- 计划：`plans/2026-10-06-mysql-gap/m0-query-semantics.md`（总纲 `gap-matrix.md`，F 组销账；
  后续 M1 文件拆分 → M2 DML → M3 子查询 → M4 DDL/session → M5 e2e 盲区）。
- 文档同步（`COMPAT.md` SQL 章节、`agents/rust/sql.md`、`features/sql-dataplane.md`、
  `features/e2e-coverage.md` 登记新 e2e）为 M0 计划清单项，随 M0 收尾提交一并补齐。
