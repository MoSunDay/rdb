# H4 流程与测试卫生

> 状态：proposed
> 依赖：无

## 工作包

1. tests/common/mysql.rs 固定测试口令随机化（密钥规则边缘）
2. /tmp/rdb-sql-world-* 清理守卫
3. 负载 flake 缓解：重进程套件错峰/分类运行器（kafka/2pc 已知隔离全绿）
4. 剩余 9 个集群套件迁 common::mysql
5. ODKU/INSERT..SELECT 性能项：勘误——H0 落地的是 **BTreeMap 活 overlay**（每条已决策写
   维护、O(log n) 直接查找，消除 O(rows²) 线性探测），"overlay 哈希化已随 H0 落地"为
   误记：哈希化并未落地，仍是本包的后续可选优化；INSERT..SELECT 深拷贝改 into_source
   取走（条目不变）
## 验收

单测 + e2e 每项至少一条；收尾同步 COMPAT.sql.md / changelog / e2e-coverage；
全量 workspace 复跑无回归。
