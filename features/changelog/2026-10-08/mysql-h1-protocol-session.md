# SQL：mysql-hardening H1 协议与会话正确性落地

Commit: (working-tree, 随本提交入库)

## 背景

`plans/2026-10-06-mysql-hardening/02-h1-protocol-session.md` 工作包 2–7
（item 1 二进制编码器列元数据感知已前置于 `b9bf6c6` 提交）。H0 清零静默错误
结果后，本批修协议/会话面的错误面缺陷：USE 不校验、CONNECTION_ID 可碰撞、
NULL/非有限值处理次序、结果长度无上限、递归 LIKE、日期过严、DML 占位符
盲区、GROUP BY 不认别名、聚合 arity errno 措辞。

## 落地（按 item）

2. **USE 1049 / CONNECTION_ID**（`src/sql/front/shim.rs`）：USE 校验接受默认库
   `rdb`（大小写不敏感），未知库报 1049 "Unknown database 'x'"（不再任意名通过）；
   prepared-USE 一律 1235。连接 id = `epoch_secs.wrapping_add(seq)`（u32 环绕，
   AtomicU32 CAS 分配、跳过 0 槽）：无持久化，重启间隔 ≥1s 即不碰撞。
3. **NULL-first / POW**：ROUND/TRUNCATE(x,NULL)、LOCATE(a,b,NULL) 先判 NULL 再
   取整/定位（`func/numeric.rs`、`func/string_more.rs`）；POW 指数出 [-30,30] 或
   界内非有限结果 → NULL（`func/numeric_more.rs`）——clamp 会给错值，故 NULL。
   FROM_UNIXTIME(NULL) 复核**本就正确**（NULL-first 已在 M1 实现），不在改动面。
4. **TRIM 默认 remstr**（`parse/trim_default.rs`）：`TRIM(x)`/`TRIM(LEADING FROM x)`
   等无 remstr 形态预解析注入 `' '`（sqlparser 0.62 语法本身要求 remstr）。
5. **结果长度上限 + like 迭代化**：REPEAT/LPAD/RPAD 精确字节数预检（u128 乘法，
   ≤1<<24 允许、超出 → NULL，判定先于分配，堵 DoS 面，`func/string.rs`/
   `func/string_more.rs`）；like_match 递归改迭代 DP（新 `exec/expr_like.rs`，
   `exec/expr.rs` 删递归 matcher，`front/vars.rs` SHOW LIKE 改引新路径）——
   54,684 对 exhaustive 等价验证。
6. **日期宽松读**（`temporal.rs`）：1–2 位月日（`'2020-1-1'` 不再 1292/NULL）、
   小数秒超 6 位截断（对齐 MySQL 8 宽松读）。
7. **DML 占位符 / GROUP BY 别名 / arity 1582**：UPDATE/DELETE 的 ORDER BY ?/LIMIT ?
   此前完全不被 count/bind 覆盖——AST 改 `LimitValue`（`parse/ast.rs`），占位符按
   SET→WHERE→ORDER BY→LIMIT 文本序绑定（`parse/translate_dml.rs`），
   `exec/write.rs::matched_rows` 吃 `Option<&LimitValue>`；GROUP BY 引用 SELECT
   别名在 exec 时解析（`exec/select.rs::group_keys_resolving_aliases`，**列名
   优先于别名**，与 ORDER BY 的别名优先相反）；聚合 arity 错 errno 1235→1582
   （`parse/func_forms.rs`，仅 arity；unsupported 形态保持 1235）。

## 测试

- 单测：shim（USE/conn id 5 例）、expr_like/trim_default 各带独立测试文件、
  numeric/string/temporal/translate_dml/select 内联测试扩展；`cargo test -p rdb
  --lib` **1343/1343** 全绿（基线 1320 → +23）。
- e2e：sql_e2e（USE nodb→1049、DML 占位符）、funcs_control（规避断言翻转后改回
  i64→"42"、f64→"1.5" 真值断言）、funcs_string/numeric/datetime、
  query_semantics、ddl_surface 连带修（probe_db/moved_db 改断 1049）。
- 面上回归：31 个 SQL e2e 套件**两轮全绿**。期间 `sql_funcs_string_e2e::
  string_negative_matrix` 在 18 套件并发批量跑时出现一次失败，其后三次复跑
  （含同样 18 套件并发、单套、3 套件组）均绿——归因为 H4 已登记的负载 flake
  类别，非本批回归。
- `cargo check`/`clippy -D warnings` 干净。

## 文档

- `COMPAT.sql.md` 台账七处同步：USE 1049（删"任意名通过"）、conn id 分配语义、
  POW 出界→NULL、REPEAT/LPAD 上限、TRIM 默认 remstr、宽松日期、GROUP BY 别名
  优先级、DML ORDER BY/LIMIT 占位符文本序绑定。
- `plans/2026-10-06-mysql-hardening/`：02-h1 状态 proposed→landed（含逐项落点），
  README 里程碑矩阵同步。
- `features/e2e-coverage.md` 本批未改：工作树中该文件的在途改动属 mq-p3 后续
  批次（memory sync），其入库随该批次落，H1 不越界触碰。

## 后续（非本批）

- H0 review 问五偏差 (6)–(9)（溢出 errno 通道 1690/22003、decimal SUM 措辞、
  SUM(double) 饱和、整型宽度坍缩/UNSIGNED 拒绝）仍开放，不在 H1 文件条目内，
  待独立收口；
- H2（唯一性/索引正确性）、H3（P2 功能补齐）、H4（测试卫生，含上述 flake 类）
  按 hardening 计划在途。
