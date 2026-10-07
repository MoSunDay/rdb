# SQL H0：静默错误结果清零（RENAME GC 数据丢失 + ODKU/相关子查询/聚合包裹/参数序/整型回绕/CONCAT_WS）

Commit: (working-tree, 随本提交入库)

## 背景

M0–M5 评审（4 份去重后 31 项）发现一批静默错误结果缺陷，其中 1 项数据丢失级别。
`plans/2026-10-06-mysql-hardening/`（H0–H4）承接；本里程碑为 H0：静默错误结果零容忍，
全部修复 + 单测矩阵 + 每项 e2e。

## 修复清单（10 项 P0）

1. **RENAME 数据丢失（ship-blocker）**：`RENAME TABLE` 原先发 `Drop(旧名, 同id)`，
   把存活 id 写进 GC dropped 集，30s 后台清扫逐步删光改名后表的行/索引。
   现在 dropped ids 改为 **id 键控侧集 `sql_dropped/<id>`**：`queue_drop` 写两条
   （`sql_catalog/<旧名>=""` 无 id 墓碑 + `sql_dropped/<id>` 标记），RENAME 只写
   无 id 墓碑（`CatalogMutation::Kv`），id 在新名下存活、永不进 dropped 集。
   `catalog.rs`/`ddl.rs`/`ddl_alter.rs`/`gc.rs`。
2. **TRUNCATE/DROP 旧 id 永不回收**：同名 Put 立即覆盖旧 id 墓碑 → 行/索引永久
   泄漏。侧集键控后同名 Put 不再抹除 dropped 记录，GC 真正回收（模块文档与
   本台账的"GC 后台回收"声明随之成真）；legacy 十进制墓碑仍被识别（升级兼容）。
   GC 周期可经 `RDB_SQL_GC_PERIOD_MS` 缩短供 e2e 使用。
3. **ODKU/REPLACE 唯一快照过期**：同语句前几行腾出的唯一值仍按快照判冲突 →
   ODKU 错走 update 分支 / REPLACE 误删无辜行（affected 4 vs 3）。每唯一索引的
   value→pk 映射改为**每写维护的活 overlay（唯一事实源）**，顺带消除 O(rows²)
   线性探测（改 O(log n) 查找）。`upsert.rs`。
4. **pk 挪移命中活行静默覆盖 → 1062**：ODKU 赋值把 pk 挪到另一活行（普通
   `UPDATE` 同病）原先 last-writer-wins 静默合并。两路径均在写前探活行占用并
   报 `ER_DUP_ENTRY`（UPDATE 的额外扫描仅在实际发生 pk 挪移时付费）。
   `upsert.rs`/`write.rs`。
5. **相关子查询遮蔽 ×2**：`substitute_refs` 不查 `shadowed()` → 内层同名未限定列
   被外层字面量替换（`WHERE k=d.k` 变 `lit=lit`）；派生表/CTE 输出列不在 shadow
   集 → 同样误绑。绑定侧接入同一 `shadowed()` 谓词；派生表与 CTE 的输出列
   静态派生进 shadow 集（超集启发式：误遮蔽只可能导致响亮 unknown-column，
   绝不静默错绑）。`correlated.rs`。
6. **标量函数包裹聚合 1235**：`substitute_aggs` 无 `Func` 臂（`has_agg` 却下钻）→
   `ROUND(SUM(x),2)`/`COALESCE(SUM(x),0)` 响亮拒绝。补 `Func` 臂逐参下钻
   （逐变体比对两函数，确认唯一缺口就是 Func）。`agg.rs`。
7. **会话函数不进 ODKU 赋值 / UPDATE·DELETE `ORDER BY`**：`substitute` 补
   `ConflictAction::OnDuplicate` 赋值与两 DML 的 order_by 键。`session_funcs.rs`。
8. **`LIMIT ?,?` 参数静默对调**：逗号形文本序 offset-先，绑定器 limit-先 → 双
   占位符形按文本序对调。双占位符逗号形改为**响亮 1064**（指向
   `LIMIT ? OFFSET ?`）；单占位符逗号形（`LIMIT ?, 5`/`LIMIT 5, ?`）位置唯一、
   保持支持。`query.rs`/`translate.rs`。
9. **整型回绕 → 1690 风格响亮错**：Add/Sub/Mul/取负（Int 与 Decimal 尾数）、
   `MIN / -1`、`ABS(MIN)`、整数 `SUM` 溢出全部改 checked，报
   `BIGINT value is out of range in '...'`（复用 1292/WrongValue 通道，措辞为
   MySQL 1690 原文）；`MIN % -1` 数学上为 0，保持合法。`expr.rs`/
   `func/numeric.rs`/`agg.rs`。
10. **`CONCAT_WS` 丢分隔符**：`out.is_empty()` 误判已发参数 → 空串参数吞掉分隔符
    （`CONCAT_WS(',','','b')` 得 `"b"`）。改 `emitted` 标志，得 `",b"`。
    `func/string.rs`。

## 测试

- 单测：catalog 侧集两态（同名 Put 不抹除 / RENAME 形非 dropped）、gc 侧集清扫 +
  改名存活、ddl id 单调、upsert 4 例（腾出值/误删/1062/所有权迁移）、write 2 例、
  subquery 4 例（同名遮蔽/派生表/CTE/真外层引用回归）、agg 5 例（包裹/溢出）、
  session_funcs 2 例、order_limit 1 例、expr/numeric/string 溢出与 CONCAT_WS 矩阵。
- e2e：新增 `tests/sql_rename_gc_e2e.rs`（`RDB_SQL_GC_PERIOD_MS=200`，RENAME 后
  5+ 轮清扫数据完好；TRUNCATE 后旧数据确清）；`sql_upsert_e2e`（affected 3 +
  1062 线上矩阵）、`sql_subquery_e2e`（同名遮蔽）、`sql_query_semantics_e2e`
  （ROUND(SUM)/ODKU 赋值 DATABASE()）、`sql_e2e`（`LIMIT ? OFFSET ?` 顺序 +
  逗号双参 1064）、`sql_funcs_{numeric,string}_e2e`（1690 风格 ×5、CONCAT_WS）。
- 验收：`cargo test --workspace --no-fail-fast` 连跑三次 **1726/1726/1726 全绿**
  （首轮并行曾见 `rocksmq_http_e2e` 已知负载 flake，隔离复跑绿，与本里程碑无关）。

## 遗留与移交

- 偏差台账更新：`COMPAT.sql.md`（GC 侧集、RENAME 无 id 墓碑、ODKU overlay/1062、
  LIMIT 逗号双参 1064 入偏差台账）。
- 已知但不属 H0：列式表 RENAME 后 segment meta 仍记旧表名（columnar gc 按缺席名
  归类回收，行为等价 DROP——入 H1 观察）；`func/meta.rs` Neg 折叠回绕（实际不可达）。
- H1–H4 见 `plans/2026-10-06-mysql-hardening/`。
