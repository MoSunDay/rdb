# M0 — 查询语义修复（ORDER BY/GROUP BY 序数、别名解析、LIMIT ?、FROM DUAL）

对应 `gap-matrix.md` F 组全部 4 项 P0。这四项都是**静默错误或错误拒绝**，不涉及执行器结构性
改动，可在一个小 PR 序列内完成，因此排在 M1（文件拆分）之前。

## 目标

1. `ORDER BY 2` / `GROUP BY 1` 的整型字面量按 MySQL 语义解析为**输出列位置**（1-based），
   越界按 MySQL 风格报错（ER 1054 "Unknown column 'N' in 'order clause'/'group statement'"
   语义），而不再退化为常量排序键（当前是 no-op，结果顺序错且无提示）。
2. ORDER BY / HAVING 可引用 SELECT 别名：FROM 作用域解析失败后回落到**投影上下文**
   （输出列名/别名表），仍失败才报未知列；与 MySQL 的解析顺序一致（表达式先用别名，聚合内
   先用源列——v1 按"别名优先于源列"的 ORDER BY/HAVING 简化语义，与 MySQL 常见行为吻合）。
3. `LIMIT ?` / `OFFSET ?`：接受 `?` 占位符（二进制协议绑定后取值），绑定值必须可转 u64；
   负数 / 非整数 / 超范围 → 解析期风格错误（"LIMIT must be a non-negative integer"）。
4. `SELECT 1 FROM DUAL`：DUAL 表名在 FROM 翻译时归一为 `TableRef::NoTable`（该枚举已存在，
   `src/sql/parse/ast.rs` :167），与无 FROM 完全同路径；大小写不敏感（`dual`/`DUAL`）。

## 现状与证据

- 序数：`src/sql/parse/translate.rs`(727 行) `translate_order`（:718）直接 `translate_expr`，
  `SqlExpr::Value(Number)` 产出 `Expr::Lit(Int)` 常量键——排序键恒定即无操作。GROUP BY 在
  `src/sql/parse/query.rs`(255 行) `translate_select`（:108）同样走 `translate_expr`。
- 别名：`src/sql/exec/select.rs`(550 行) `validate_refs`（:227）对 WHERE/GROUP BY/HAVING/
  ORDER BY/投影项全部只按 FROM scope（`scan::check_expr`）校验，别名列不在 scope 内 → 报
  unknown column。SELECT 别名信息（`SelectItem::Expr { alias }`，`parse/ast.rs` :206）在校验
  时可达但没有被使用。
- LIMIT：`translate_limit`（`parse/translate.rs` :703）只接受 `Value::Number`；SELECT 侧
  `translate_limit_clause`（`parse/query.rs` :205）复用同一函数。`Expr::Placeholder`（AST
  :235）与参数绑定机制已存在（prepared statements e2e 在跑），缺的只是 LIMIT 位置放行。
- DUAL：`parse/query.rs` :120 仅 `from: None` → `TableRef::NoTable`；带名字的 `dual` 走普通
  表名 → catalog 查不到 → table not exist。

## 实现拆分（文件与职责，标注行数预算）

- `src/sql/parse/ast.rs`（317 行，+~15）
  - `OrderKey.expr` 不变；新增 `GroupKey`/或在 `Query.group_by` 处用带标记的表达式：
    `enum SortRef { Position(usize), Expr(Expr) }` 供 ORDER BY 与 GROUP BY 共用（位置引用仅
    在翻译期产生，执行器不猜）。
- `src/sql/parse/query.rs`（255 行，+~40，预算 ≤300）
  - `translate_select`：`SELECT ... FROM DUAL` 表名 `dual`（忽略大小写）→ `TableRef::NoTable`；
  - ORDER BY / GROUP BY 翻译改走 `translate_sort_ref`：整型字面量 → `Position(n)`，n==0 或
    n>输出列数 → ER 1054 风格错误（错误信息带原文数字，如 `Unknown column '3' in 'order clause'`）。
    注意：仅裸整型字面量算序数，`ORDER BY 1+1`、`ORDER BY '1'` 仍是表达式（回归测试覆盖）。
- `src/sql/parse/translate.rs`（727 行，+~10）
  - `translate_limit` / `translate_offset` 增加 `SqlExpr::Value(Placeholder("?"))` 分支 →
    返回 `Option<LimitRef>`（`Lit(u64) | Param`），参数绑定阶段解析为 u64（负/非整/超界报
    "LIMIT must be a non-negative integer"）。UPDATE/DELETE 的 LIMIT 同步受益。
- `src/sql/exec/select.rs`（550 行，+~60，预算 ≤650）
  - `validate_refs` 扩展：先构造投影上下文（输出列名 → 位置，来自 `SelectItem` 别名或列名），
    WHERE 仍只认 FROM scope（MySQL 语义），GROUP BY/HAVING/ORDER BY 在 FROM scope 失败后回落
    投影上下文；两处都失败才报 unknown column（错误信息与现状同形）。
  - `Position(n)` 求值：按输出列位置取已物化投影值（排序/分组发生在投影之后，符合 MySQL
    "ORDER BY 输出列"语义）。

## 单测

- `src/sql/parse/mod.rs`（449 行，追加用例）：DUAL 归一、序数翻译（合法/越界/0）、
  `ORDER BY 1+1` 与 `ORDER BY '1'` 不按序数、LIMIT ? 翻译形态。
- `src/sql/exec/select_tests.rs`（249 行，追加用例）：别名回落解析成功/失败矩阵
  （别名在 ORDER BY、HAVING 聚合条件、别名与源列同名时 FROM 优先）、位置排序取投影值
  （含 DISTINCT 投影后再排序的顺序断言）。

## e2e

新文件 `tests/sql_query_semantics_e2e.rs`（预算 ≤400 行）：

- 序数回归：`SELECT a, b FROM t ORDER BY 1` vs `ORDER BY '1'`（常量，no-op，行序=扫描序），
  两查询结果顺序必须不同；`GROUP BY 1` 与按首列分组等价断言。
- 别名：`SELECT a AS x ... ORDER BY x` / `HAVING cnt > 1`（cnt 为聚合别名）。
- LIMIT ?：prepared statement（`stmt_exec` 二进制协议）绑定 2；负数、小数、字符串绑定 →
  客户端可见错误。
- DUAL：`SELECT 1 FROM DUAL`、`SELECT VERSION() FROM dual`；负矩阵：`FROM dual2` 仍报表不存在。
- 负矩阵汇总：`ORDER BY 9`（越界）、`ORDER BY x`（别名不在投影也不在 FROM，仍报未知列）。

## 文档同步

- `COMPAT.md` SQL 章节：新增"ORDER BY/GROUP BY 序数与别名解析遵循 MySQL；DUAL 为无表占位"；
- `agents/rust/sql.md`：`parse/query.rs` 职责描述补 `translate_sort_ref`；
- `features/sql-dataplane.md`、`features/e2e-coverage.md`（登记新 e2e 文件）；
- `features/changelog/<落地日期>/`：M0 条目。

## 验收

- F 组 4 项在 gap-matrix 标记"已有"，静默错误清零；
- `cargo test --workspace --no-fail-fast` 全绿（含旧断言迁移：如 `parse/mod.rs` 中把
  `ORDER BY 1` 旧翻译行为的隐式依赖用例改写）；
- 现有 12 个 `tests/sql_*.rs` e2e 无回归（它们大量使用 ORDER BY 列名形态，不受影响）。

## 风险

- 别名回落与源列同名冲突：采用 FROM 优先、别名兜底的顺序，MySQL 在 ORDER BY 亦有此歧义
  （sql_mode 依赖），v1 固化为文档化选择；
- `Position` 求值依赖"投影先于排序"的执行顺序，若 SELECT 管线中排序在投影前（DISTINCT 路径），
  需在 `select.rs` 内部保证投影列缓存可见——单测显式覆盖 DISTINCT + ORDER BY 位置组合；
- LIMIT 占位符绑定失败时机在执行前（prepare 阶段仍可 OK，exec 报错），与 MySQL 协议行为
  对齐即可，不追求 prepare 期校验。
