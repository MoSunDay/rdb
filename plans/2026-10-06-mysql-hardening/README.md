# 2026-10-06 mysql-hardening：M0–M5 评审缺陷修复计划

> 状态：H0 landed（摘要 `features/changelog/2026-10-06/mysql-h0-silent-wrong-results.md`）；H1 landed（摘要 `features/changelog/2026-10-08/mysql-h1-protocol-session.md`）；H2–H4 proposed
> 日期：2026-10-06
> 输入：M0–M5 四份评审（去重后 31 项：P0 静默错误结果 10、P1 错误面/健壮性、P2 钉死怪癖、P3 测试/流程）

原则：延续本程序标准——**静默错误结果零容忍**，先修数据正确性（H0），再修错误面
（H1），再唯一性/索引正确性（H2），最后 P2 功能补齐（H3）；测试卫生并行推进（H4）。

## 里程碑矩阵

| 里程碑 | 主题 | 状态 | 文档 |
|---|---|---|---|
| H0 | 静默错误结果清零（RENAME GC 数据丢失、TRUNCATE id 泄漏、ODKU/REPLACE overlay、相关子查询遮蔽、聚合包裹、会话函数面、LIMIT 双参对调、整型回绕、CONCAT_WS） | **landed** | `01-h0-silent-wrong-results.md`（2026-10-07 收口补丁（问五）：H0-4b/ODKU 唯一抢占/列式 GC id 键控，见 01-h0 收口记录） |
> 状态：H0 landed（摘要 `features/changelog/2026-10-06/mysql-h0-silent-wrong-results.md`）；H1 landed（摘要 `features/changelog/2026-10-08/mysql-h1-protocol-session.md`）；H2–H4 proposed
| H2 | 唯一性/索引正确性（依赖 H0 overlay：唯一键级串行化保证 1062、CREATE UNIQUE INDEX 回填失败回滚 schema、索引物理键加 index-id、集群 veto 收窄为按键 owner） | proposed | `03-h2-uniqueness-index.md` |
| H3 | P2 功能补齐（按 mysql-gap gap-matrix 顺序：GROUP_CONCAT 内部 ORDER BY → WITH RECURSIVE → ALTER ADD/DROP COLUMN → KILL → 复合/前缀索引吃 H2-2 红利；窗口函数另立计划） | proposed | `04-h3-p2-features.md` |
| H4 | 流程与测试卫生（随机化测试口令、/tmp 清理守卫、负载 flake 缓解、剩余 9 个集群套件迁 common::mysql、ODKU/INSERT..SELECT 性能项） | proposed | `05-h4-test-hygiene.md` |

## 验收与文档

每里程碑收尾同步 `COMPAT.sql.md` 偏差台账（删已修项）、`features/changelog/`、
`features/e2e-coverage.md`；H0 收尾已按约定复跑全量 workspace 三次确认无回归。
