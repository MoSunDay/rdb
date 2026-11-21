# H3 P2 功能补齐

> 状态：proposed
> 依赖：H2-2 索引物理键加 index-id（复合/前缀索引项）

## 工作包（按 gap-matrix 顺序）

1. GROUP_CONCAT 内部 ORDER BY（组内排序）
2. WITH RECURSIVE（语法 + 迭代执行上限）
3. ALTER ADD/DROP COLUMN（schema 版本化；MODIFY 暂缓）
4. KILL
5. 复合/前缀索引（吃 H2-2 红利）

窗口函数维持另立计划。

## 候补登记（2026-11-21，源：src 孤儿 TODO W2.0-exec）

decimal 精确算术两处已知粗化路径，暂不立项、随 H3 执行时一并处置或按触发条件拾起：

- `src/sql/exec/expr_decimal.rs:142`：decimal 除法在 |ma| * 10^(sa+4-sa+sb) 超 i128
  （34+ 位有效数字被除数或宽 scale 除数，如 1e33 / 1e-5）时回退 f64 商，
  2^53 以上有效位粗化；需更宽中间体的精确长除法。
- `src/sql/exec/expr.rs:484`：Decimal 与 Double/Str 比较经 f64 / 字符串解析粗化；
  需 Double/Str 操作数的精确 decimal 路径。

## 验收

单测 + e2e 每项至少一条；收尾同步 COMPAT.sql.md / changelog / e2e-coverage；
全量 workspace 复跑无回归。
