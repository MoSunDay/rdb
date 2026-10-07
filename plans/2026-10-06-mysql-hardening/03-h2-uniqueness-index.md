# H2 唯一性/索引正确性

> 状态：proposed
> 依赖：H0-3 overlay 设计

## 工作包

1. 唯一索引键级串行化（latch on unique key）保证并发双写 1062 语义；CREATE UNIQUE INDEX 回填失败回滚 schema（undo Put）
2. 索引物理键加 index-id：修同列两索引共享键空间（DROP 一个扫掉另一个），复合/前缀索引前置
3. 集群 veto 收窄为按键 owner 判定（纯本地 band 表放行 ODKU/REPLACE），注释与实现对齐
## 验收

单测 + e2e 每项至少一条；收尾同步 COMPAT.sql.md / changelog / e2e-coverage；
全量 workspace 复跑无回归。
