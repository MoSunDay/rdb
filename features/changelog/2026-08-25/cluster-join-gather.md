# 集群模式 JOIN 漏读修复（P0）

## 背景
上线终审判定：集群模式下 JOIN 走 `scan::materialize` 本地嵌套循环——列存侧只读本节点
段文件（段随事务提交节点分布）、行存侧只读本节点 slot band，其余节点的行被**静默丢弃**
（无报错、无文档承认、无测试）。列为上线阻塞（P0）。

## 修复
- `src/sql/exec/scan.rs`：嵌套循环组合逻辑提取为纯函数 `join_sources(l, r, on)`，单机
  `materialize` 的 Join 分支与集群 gather 路径共用同一实现。
- `src/sql/dist/gather.rs`：`materialize` 递归处理 `TableRef::Join`——两侧各自走
  gather（行存按 band、列存扇出全成员），协调者再做嵌套循环；集群就绪时 JOIN 不再
  回退本地扫描。`headline`/EXPLAIN 新增 `Gather(join)`（`join_gathers` 递归判定任一
  叶子表扇出即显示）。

## 验证
- 新增 `tests/sql_join_cluster_e2e.rs`（3 进程集群）：列存段 3 节点各 5 行 + 行存
  300 行跨 band 分布，每个节点执行 列存⋈行存 / 行存⋈行存（自连接），断言全集恰好
  一次；EXPLAIN 首行 `Gather(join)`。
- 变异验证：修复前该测试红——node0 混引擎 JOIN 仅返回本节点段命中的 `["2","3"]`
  （期望 1..=15）。
- 全量回归：`cargo test --workspace` 绿（单测 695 + 全部 e2e）；fmt / clippy
  `-D warnings` / release build 通过。
