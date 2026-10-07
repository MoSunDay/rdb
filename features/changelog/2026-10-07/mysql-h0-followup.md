# SQL H0 收口补丁（评审问五：H0-4b 多行 pk 挪移 / ODKU 唯一值抢占 / 列式 GC id 键控）

Commit: (working-tree, 随本提交入库——与 mysql-gap M0–M5、H0 主体同一 SQL 流提交)

## 背景

H0（`2026-10-06/mysql-h0-silent-wrong-results.md`）落地后的评审问五追查出三处
收尾缺口——两项静默错误结果、一项列式数据丢失——以及若干文档失真，2026-10-07
同日收口。计划侧记录见
`plans/2026-10-06-mysql-hardening/01-h0-silent-wrong-results.md` 的"收口补丁"节。

## 修复清单（3 项代码）

1. **H0-4b：普通 UPDATE 同语句多行 pk 挪移**
   - 缺陷：H0-4 的探活行占用只对照一次语句前快照——同语句先行写腾空的目标 pk
     被误报 1062（合法链式挪移被拒，假阳性），而两行收敛到同一目标时第二写
     漏过检查、静默合并（假阴性）。
   - 修复：探针集改为**按语句序折叠每条已决策写**的活视图——先腾空的目标可落，
     两行收敛到同一目标在第二行响亮报 1062；跨语句事务碰撞由 staged 写合并
     覆盖（MySQL 按检索序逐行处理，语义对齐）。
   - 代码：`src/sql/exec/write.rs`（`any_moved` 门控，普通 UPDATE 不挪 pk 零开销）。
   - 测试：`src/sql/exec/write_tests.rs`（同目标双行 1062 / 语句内腾空可落 /
     自赋值与链式挪移静默三例）。
2. **ODKU update 分支唯一值抢占（H0-3 收尾）**
   - 缺陷：ODKU 只查 pk 挪移命中活行；赋值把唯一索引值改挂到**另一条活行**名下
     时靠批量 `vacated` 集白洗——静默抢走所有权（事务内错误还被推迟到 COMMIT）。
   - 修复：决策期即拒——更新行的新唯一值若被**不同的**活行持有则报 1062
     （pk 挪移检查的镜像；被更新行自身释放的 claim 不算抢占，pk 挪移情形已由
     前置检查排除）；事务内错误在语句处浮出而非 COMMIT。
   - 代码：`src/sql/exec/upsert.rs`（`run_odku` update 分支，活 overlay
     `unique_owner` 直查）。
   - 测试：`src/sql/exec/upsert_tests.rs`——`odku_unique_preemption_is_1062_same_statement`、
     `odku_unique_preemption_errors_at_statement_in_txn`（事务内语句处浮出）+
     语句内值在行间合法迁移回归。
3. **列式 GC 改 id 键控分类（数据丢失级）**
   - 缺陷：列式 segment meta 的 table_name 在 RENAME 后不更新，`sql/columnar/gc.rs`
     按缺席名归类——改名后的列式表被后台清扫**整表静默清库**；非 leader 节点
     TRUNCATE 只有急切删路径在 leader 上跑，旧 id segment 永久泄漏。
   - 修复：分类改按 **meta 键内解析出的 table_id** 对照 raft 事实：live 优先保留 /
     dropped（`sql_dropped/<id>` 侧集或 legacy 十进制墓碑）回收 / unknown 保留
     （重启窗口 FSM 视图未载入时两者皆空，缺席不证明死亡）。live 优先顺带保护
     升级前的改名表（legacy 墓碑仍读作 dropped 也先看 live）。
   - 代码：`src/sql/columnar/gc.rs`（模块文档同步重写分类规则）。
   - 测试：`src/sql/columnar/tests_gc.rs`（id 三分类：改名存活 / dropped 清扫 /
     TRUNCATE 旧 id 回收 / unknown 保留）+ e2e `tests/sql_rename_gc_e2e.rs`
     覆盖行存侧回归。

## 文档收口

- `COMPAT.sql.md` 偏差台账 +4 条（编号续接 M0-M5 台账）：(6) 整型算术全 checked、
  1690 措辞响亮，但 errno/SQLSTATE 走 1292/22007 通道（按 errno 分类的客户端会
  误判）；(7) `SUM(decimal)` 尾数溢出报 1235 而非 MySQL 越界措辞；(8) `SUM(double)`
  f64 路径可静默饱和 ±inf；(9) 整型列宽 TINYINT..BIGINT 全坍缩为单一有符号 64 位
  Int（TINYINT 列静默存 200），UNSIGNED 整型（如 BIGINT UNSIGNED）在 CREATE TABLE
  响亮 1235 拒绝（sqlparser Unsigned 变体落入不支持兜底）——评审简报所称"静默
  收窄"有误，静默的只有宽度坍缩，UNSIGNED 是显式拒绝。
- `plans/2026-10-06-mysql-hardening/05-h4-test-hygiene.md` 勘误：工作包 5 原文
  "overlay 哈希化已随 H0 落地"失实——H0 落地的是 **BTreeMap 活 overlay**（每条
  已决策写维护、O(log n) 直接查找，消除 O(rows²) 线性探测），哈希化仍是后续
  可选优化；INSERT..SELECT `into_source` 取走条目不变。
- `plans/2026-10-06-mysql-hardening/02-h1-protocol-session.md` 新增工作包 0：把
  评审升级的列式 GC P0 记为**已由本补丁解决**，升级决策留档、勿重开。
- `plans/2026-10-06-mysql-hardening/README.md` H0 矩阵行追加收口补丁指引；
  `plans/2026-10-06-mysql-hardening/01-h0-silent-wrong-results.md` 追加收口记录
  （含升级前 RENAME 十进制墓碑迁移评估）。
- `agents/rust/sql.md` 同步：`exec/upsert.rs` 条目改述 BTreeMap 活 overlay 与
  ODKU 决策期双抢占拒绝；tests/e2e 清单 M4 行补登 `sql_rename_gc_e2e.rs`。
- `src/sql/exec/ddl_tests.rs`：`table_ids_stay_monotone_across_drop_recreate`
  的文档注释改为现行墓碑形态（无 id 名字墓碑 + `sql_dropped/<id>` 侧标）。

## 遗留与移交（全部待用户决策）

- **提交拆分（已收口）**：SQL 流以单提交落地（本提交：M0–M5 主体 + H0 +
  本补丁，含全部台账文档）；并行 MQ 流改动留在工作区由其车道另行提交。
- **`RDB_SQL_GC_PERIOD_MS`**：为 e2e 缩短 GC 周期引入的环境变量，是否长期保留
  （还是收敛为测试专用注入）待定。
- **H1/H4 排期**：H1（协议与会话）与 H4（测试卫生）先后/并行由用户排期。
- **升级前 RENAME 十进制墓碑迁移**（问五 #6，仅评估未开工）：现状、选项 A
  （推荐：H1 工作包——启动/首个 DDL 扫描 `sql_catalog/*`，id 另名存活的十进制
  墓碑改写为无 id `""` 墓碑，一次幂等 raft KV 写，须先于首轮 GC、滚动升级需
  全节点上新）/ 选项 B（发布说明要求 dump/reload——否决，静默数据丢失）/
  选项 C（行存 GC 镜像 live-first，误删降级为不回收，可并入 A 的同一工作包），
  详见 `01-h0` 收口记录；行存 GC 现存 dropped-id 优先同被该评估覆盖。
