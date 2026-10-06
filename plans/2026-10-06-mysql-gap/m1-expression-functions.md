# M1 — 表达式与函数族（expr.rs 拆分 + CASE/CAST/函数注册表）

对应 `gap-matrix.md` A 组。前置：M0 已合入（`parse/expr.rs` 将被本里程碑大幅扩展）。
**第一动作是拆分 `src/sql/exec/expr.rs`**——708 行已逼近 800 上限，禁止直接追加。

## 目标

1. 把标量函数求值从 `exec/expr.rs` 拆到 `exec/func/` 子模块，注册表保持**纯 dispatch**
   （函数名 → 纯函数的 match/查表；无 struct 注册器、无内部状态）。
2. 补齐 A 组 P0/P1 函数：CASE（两形态）、IF/IFNULL/NULLIF/COALESCE、CAST/CONVERT、字符串族、
   数值族（DECIMAL 精确）、日期时间族、`<=>`/REGEXP/位运算/XOR、GROUP_CONCAT（SEPARATOR）。
3. 既有 13 个函数入口（UPPER/LOWER/ABS/CHAR_LENGTH/LENGTH/VERSION/NOW 族/CURDATE 族/
   LAST_INSERT_ID）行为不变，但获得 e2e 元数据断言（结果列类型随函数确定）。
4. GROUP_CONCAT 内层 ORDER BY 明确 **deferred**（P2，理由：需分组内排序基建）。

## 现状与证据

- `src/sql/exec/expr.rs`（708 行）：`eval_func`（:535）单 match 承载全部 13 个函数入口；
  `like_match`（:506）已是独立纯函数（byte-wise，决策点 3 保持）；DECIMAL 比较与算术在
  `exec/expr_decimal.rs`（237 行，`checked_abs`/`decimal_out_of_range` 等基建齐备）。
- `src/sql/parse/expr.rs`（273 行）：`translate_expr`（:9）无 CASE 分支；`translate_function`
  （:145）做 arity 校验后只认聚合枚举（`AggFunc`，`parse/ast.rs` :219）与白名单标量；
  `translate_binop`（:125）无 `<=>`/位运算映射。
- `src/sql/exec/render.rs`（479 行）：`Expr::Func` 的结果列元数据在 :361 分支统一给宽类型；
  EXPLAIN 渲染在 :96。函数真返回类型（如 CHAR_LENGTH→BIGINT、ROUND(DECIMAL)→DECIMAL）目前
  无表可查。
- `src/sql/parse/ast.rs`（317 行）：`Expr`（:228）无 `Case`/`Cast` 节点；`BinOp`（:285）缺
  NullSafeEq/BitAnd/BitOr/BitXor/Shl/Shr/Xor/Regexp。

## 实现拆分（文件与职责，标注行数预算）

- `src/sql/exec/func/mod.rs`（新增，预算 ≤200）：注册表纯 dispatch —— `pub fn eval(name, args)
  -> SqlResult<Value>` 与 `pub fn result_type(name, arg_types) -> SqlType`（元数据查表），
  按 name 前缀路由到子模块；不持有任何状态。
- `src/sql/exec/func/string.rs`（新增，预算 ≤350）：CONCAT/CONCAT_WS/SUBSTRING/TRIM/REPLACE/
  LEFT/RIGHT/LPAD/RPAD/REPEAT/LOCATE/HEX + 迁入 UPPER/LOWER/LENGTH/CHAR_LENGTH（均为
  `fn(&[Value]) -> SqlResult<Value>` 纯函数，NULL 传播统一在 dispatch 层做约定）。
- `src/sql/exec/func/numeric.rs`（新增，预算 ≤350）：ROUND/CEIL/FLOOR/MOD/POW/SQRT/TRUNCATE/
  SIGN/GREATEST/LEAST + 迁入 ABS；DECIMAL 分支复用 `expr_decimal.rs` 模式（ROUND/TRUNCATE/
  CEIL/FLOOR 对 Decimal(m,s) 产出精确 Decimal，除零/溢出走既有 `decimal_out_of_range`）。
- `src/sql/exec/func/datetime.rs`（新增，预算 ≤350）：DATE_ADD/DATE_SUB（INTERVAL 单位集）、
  DATEDIFF/DATE_FORMAT/YEAR/MONTH/DAY/HOUR/MINUTE/SECOND/UNIX_TIMESTAMP/FROM_UNIXTIME +
  迁入 NOW 族/CURDATE 族；日历数学复用 `src/sql/temporal.rs`（252 行，如需扩展单位再评估
  是否拆 `temporal_interval.rs`）。
- `src/sql/exec/func/*_tests.rs`（每文件 ≤400）：NULL 传播、边界（空串/负数/i64 边界/DECIMAL
  scale 推断）、REGEXP 语义（锚定差异不引入，MySQL 与 Rust regex 的 ^$ 语义差记 COMPAT.md）。
- `src/sql/exec/expr.rs`（708 → 预算 ≤450）：保留 `eval`/`cmp_values`/`coerce`/`like_match`
  与聚合求值；`eval_func` 缩为对 `func::eval` 的转发；`expr_tests.rs`（586 行）保留通用
  算子用例，函数用例迁至 func 子模块测试。
- `src/sql/parse/ast.rs`（+~30）：`Expr::Case { operand, branches, else_ }`（operand=None 为
  搜索式）、`Expr::Cast { expr, ty }`；`BinOp` 增加 NullSafeEq/BitAnd/BitOr/BitXor/Shl/Shr/
  Xor/Regexp。
- `src/sql/parse/expr.rs`（273 → 预算 ≤450）：CASE/CAST/CONVERT 翻译、新 BinOp 映射、标量函数
  白名单放开为"注册表已知名 + arity 表"；**若超预算则把 arity/签名表拆到
  `src/sql/parse/func_sig.rs`**（新增，预算 ≤250：函数名 → (min,max arity, 结果类型规则)，
  纯查表）。
- `src/sql/exec/agg.rs`（258 行，+~50）：GROUP_CONCAT（SEPARATOR 仅此一个修饰符，内层 ORDER BY
  翻译期拒绝并给 unsupported——deferred 而非静默忽略）；拼接按分组首次出现顺序。
- `src/sql/exec/render.rs`（479 行，±~20）：`Expr::Func` 元数据分支改为查 `func::result_type`
  表；EXPLAIN 文本对新节点（Case/Cast/新 BinOp）出可读形态。

## 单测

- `func/*_tests.rs`：逐函数正确值 + NULL 传播 + 类型错误（`upper(123)` 类现状是 NotSupported
  错误，保持大声）；
- `parse/expr.rs` 用例（或新 `parse/func_sig_tests.rs`）：arity 越界、CASE 两形态、
  `CAST('2026-01-01' AS DATE)`、`1 <=> NULL`、`5 & 3`、`'a' REGEXP '^a'`；
- `expr_decimal` 联动：ROUND(DECIMAL, d) 精确断言（`1.005` 类银行家舍入差异按 MySQL
  "round half away from zero" 记录用例）。

## e2e

新文件 `tests/sql_funcs_e2e.rs`（预算 ≤400；若超限按族拆 `sql_funcs_string_e2e.rs` /
`sql_funcs_datetime_e2e.rs`，共享 M5 的 `tests/common/mysql.rs` 样板）：

- 三值逻辑：`IFNULL(NULL,1)`、`NULL<=>NULL`、`CASE WHEN NULL THEN 1 ELSE 2 END`；
- 函数进 WHERE / GROUP BY 键（`GROUP BY UPPER(name)` 走分组路径）；
- prepared statement 参数作函数实参（`WHERE UPPER(name) = ?`）；
- DECIMAL 精度：`ROUND(price, 2)` 结果列类型与值；
- 元数据断言：结果集列类型（CHAR_LENGTH→LONGLONG、CONCAT→VAR_STRING、ROUND(DECIMAL)→NEWDECIMAL）；
- 既有 13 函数的 e2e 补齐（UPPER/LOWER/ABS/CHAR_LENGTH 等，当前仅散见于 `string_e2e.rs`/
  `sql_types_e2e.rs`）。

## 文档同步

- `COMPAT.md`：函数清单（已有/新增/不支持）、UTC 时钟与无 collation 的 REGEXP/LIKE 差异；
- `agents/rust/sql.md`：`exec/func/` 模块地图与注册表约定（纯 dispatch、无状态）；
- `features/sql-dataplane.md`、`features/e2e-coverage.md`、`features/changelog/<日期>/`。

## 验收

- A 组 P0/P1 全部标记"已有"；GROUP_CONCAT 内层 ORDER BY 在矩阵保持 P2 + deferred 理由；
- `exec/expr.rs` ≤450 行，`exec/func/*` 每文件 ≤400 行，`parse/expr.rs`（或 +`func_sig.rs`）
  各 ≤450 行；
- `cargo test --workspace --no-fail-fast` 全绿。

## 风险

- sqlparser 0.62 的 CASE/CAST AST 形态与 `parse/expr.rs` 直翻有出入（`CastKind`、`CharLengthUnits`
  等），翻译层需窄化到 MySQL 子集，超出子集大声拒绝；
- REGEXP 语义：Rust `regex` crate 与 MySQL（ICU/Henry Spencer）方言差异（反向引用不支持等），
  v1 接受差异并在 COMPAT.md 记录，不做正则引擎替换；
- DECIMAL scale 推断（ROUND 的 d 超过列 scale、CEIL 结果 scale=0）规则多，靠
  `expr_decimal` 既有错误路径兜底，先覆盖常见形；
- `parse/expr.rs` 若同文件承载翻译+arity 表导致超 450 行，立即拆 `func_sig.rs`，不硬扛。
