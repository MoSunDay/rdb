# H0 静默错误结果清零（landed）

> 状态：landed（2026-10-06）
> 摘要：`features/changelog/2026-10-06/mysql-h0-silent-wrong-results.md`

## 范围（10 项 P0，全部落地）

1. RENAME GC 数据丢失 → dropped ids 改 id 键控侧集 `sql_dropped/<id>`，RENAME 只写无 id 墓碑
2. TRUNCATE/DROP 旧 id 回收 → 侧集条目不再被同名 Put 抹除；文档修正
3. ODKU/REPLACE 唯一快照过期 → 每唯一索引 value→pk 活 overlay（唯一事实源，消 O(rows²)）
4. pk 挪移命中活行 → ODKU 与普通 UPDATE 均报 1062
5. 相关子查询遮蔽 ×2 → `substitute_refs` 接入 `shadowed()`；派生表/CTE 输出列入 shadow 集（超集安全）
6. `substitute_aggs` 补 `Func` 臂（ROUND(SUM(x),2) 等不再 1235）
7. 会话函数补 ODKU 赋值与 UPDATE/DELETE order_by
8. `LIMIT ?, ?` 双占位符逗号形响亮 1064（参数序歧义）；单占位符形保持
9. 整型 Add/Sub/Mul/Neg/ABS/SUM/MIN÷-1 全部 checked，1690 措辞（1292 通道）；MIN%-1=0
10. CONCAT_WS `emitted` 标志（空串参数不再吞分隔符）

## 验收证据

- 单测矩阵 + 每项 e2e（新增 `tests/sql_rename_gc_e2e.rs`，GC 周期经
  `RDB_SQL_GC_PERIOD_MS` 缩短）
- `cargo test --workspace --no-fail-fast` ×3：1726/1726/1726 全绿
- 负向验证：逐项回退修复可使对应测试失败（各工作包内完成）

## 2026-10-07 收口补丁（评审问五）

评审问五追查出三处 H0 收尾缺口 + 若干文档失真，同日以补丁收口（代码在待提交
工作区，与 H0 主体一并落地）：

1. **H0-4b：普通 UPDATE 同语句多行 pk 挪移**——原先探活行占用只查一次快照，
   同语句先行的腾空目标仍误报/后写的占用漏报。现探针集按语句序折叠已决策写：
   先行腾空的目标可落，两行收敛到同一目标在第二行报 1062；跨语句事务碰撞由
   staged 写合并覆盖。`write.rs`；测试 `write_tests.rs`（同目标双行 1062 /
   语句内腾空可落 / 自赋值静默三例）。
2. **ODKU update 分支补唯一值抢占检查**——原先只查 pk 挪移命中活行，赋值把
   唯一值改到另一活行名下走批量 `vacated` 集白洗。现决策期即拒（pk 挪移检查的
   镜像，被更新行自身释放的 claim 不算抢占）；事务内错误在语句处浮出而非
   COMMIT。`upsert.rs`；测试 `upsert_tests.rs`。
3. **列式 GC 改 id 键控分类**——`src/sql/columnar/gc.rs` 按 meta 键内 table_id
   对照 raft 事实分类：live 优先保留 / dropped 回收 / unknown 保留。列式表
   RENAME 不再被 GC 清库；非 leader TRUNCATE 旧 id segment 也能回收；live 优先
   还顺带保护了升级前的改名表（legacy 十进制墓碑仍读作 dropped 也先看 live）。
4. **文档收口**：`COMPAT.sql.md` 偏差台账 +4 条（整型溢出走 1292 通道、
   decimal SUM 溢出 1235、double SUM 饱和 ±inf、整型宽度坍缩 + UNSIGNED 1235
   大声拒）；`05-h4` 勘误"overlay 哈希化已随 H0 落地"（未落地，见该文件）。

### 遗留决策（未开工，待用户）：升级前 RENAME 的十进制墓碑迁移（问五 #6）

- **现状**：修复前二进制的 RENAME 写下十进制墓碑（旧名键、value=id），升级兼容
  读路径的 `dropped_ids_state` 至今把这类条目计为 dropped——升级后，被改名表的
  **行存**字节仍会被 `storage/gc.rs` 清扫（列式已由 live-first 保护）。无任何
  迁移代码。
- **选项 A（推荐，开 H1 工作包）**：启动/首个 DDL 时的迁移扫描 `sql_catalog/*`——
  value 可解析为 u32 且该 id 同时以另一名字存活 → 改写为无 id 的 `""` 墓碑
  （一次 raft KV 写，幂等）；必须在首轮 GC 之前完成，滚动升级需全节点上新二进制
  后才执行。
- **选项 B（声明式）**：发布说明要求对改名表 dump/reload——否决（静默数据丢失）。
- **选项 C（低成本前置）**：行存 GC 镜像列式的 live-first 规则（id 同时存活与
  dropped 视为存活）——把故障从"误删"降级为"不回收"；可并入同一 H1 工作包。
- 同一评估也覆盖行存 GC 的 dropped-id 优先现状：id 在新名下存活 + 旧名 legacy
  十进制墓碑，行字节仍会被清扫——选项 A/C 即为其解。
