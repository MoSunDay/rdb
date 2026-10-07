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
## 验收

单测 + e2e 每项至少一条；收尾同步 COMPAT.sql.md / changelog / e2e-coverage；
全量 workspace 复跑无回归。
