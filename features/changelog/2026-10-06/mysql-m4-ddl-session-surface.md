# SQL M4：DDL 与 session 表面积（TRUNCATE / RENAME / ALTER INDEX / SHOW 面 / 会话函数 / USE）

Commit: (working-tree, 随本提交入库)

## 背景

MySQL 兼容差距计划（`plans/2026-10-06-mysql-gap/m4-ddl-session-surface.md`）D、E 组定位出
DDL 语义与元数据/会话表面积的整块缺口：无 TRUNCATE/RENAME/ALTER INDEX，`exec/show.rs` 只有
TABLES/COLUMNS/INDEX 三种 SHOW，`front/vars.rs` 只答 @@-query，会话函数（DATABASE/USER/
CONNECTION_ID）不存在。M4 的**第一动作是拆文件**——733 行的 `exec/ddl.rs` 已逼近 800 上限。

## 修复内容（src/sql，待提交工作区）

### 拆分：`exec/ddl_alter.rs`（377）+ `parse/translate_ddl.rs`（236）
- `exec/ddl.rs` 收缩到 531 行：CREATE/DROP TABLE、catalog 临界区（`catalog_txn`）、表缓存失效；
  CREATE/DROP INDEX 迁出，TRUNCATE/RENAME 落入新文件 `exec/ddl_alter.rs`（377），全部复用
  同一 raft 守护临界区（schema 读 + catalog 写原子，ack 保持到 followers serve）。
- `parse/translate_ddl.rs`（236，+ `translate_ddl_tests.rs` 181）：DDL 表面积专用翻译——
  TRUNCATE（多表/IF EXISTS 拒绝）、RENAME（单对；多对/跨库名 1235）、ALTER TABLE 的
  ADD [UNIQUE] INDEX / DROP INDEX / RENAME [TO|AS] 归一到同一批语句，其余操作点名
  "P2 deferred" 拒绝；索引键**单列**约束在此钉死（复合/前缀 → 1235）；SHOW 的
  `[LIKE 'pat' | WHERE]` 过滤形态（WHERE → 1235）。

### TRUNCATE TABLE：同名换 table_id
schema 逐字节不变但分配**全新 table_id**，旧 id 进 catalog tombstone——物理上等价 DROP +
同定义 re-CREATE，因此本地与集群一次生效：
- 行与二级/唯一索引条目都按 table_id 编码，swap 落地即全量不可达（复制效果即 truncate
  标记，无需逐节点数据 RPC）；MVCC GC（`storage::gc`，与 DROP 同路径）后台回收全部版本与
  索引条目；列存表额外在 leader 急清段（`columnar::commit::drop_table_segments`），其余节点
  的列存清扫把旧 id 段归类为垃圾；
- AUTO_INCREMENT 计数（按表名键控）同窗口重置为 1，truncate 后写入从头开始；
- 事务内拒绝（TxnDdl → 1235 "DDL not allowed inside a transaction"），不可回滚；接受与
  DROP 相同的竞态窗口（换 id 前已解析旧 schema 的迟到写成为孤儿键，不可达即无害）。

### RENAME TABLE：纯 catalog 改名
- `RENAME TABLE a TO b`（及 `ALTER TABLE a RENAME TO b`）在 raft catalog 里以新名重写条目、
  **table_id 与物理 key 编码不变**（数据/索引零拷贝）；`sql_sequence/<name>` 计数器按原始值
  搬迁（预留批次不回退），AUTO_INCREMENT 跨改名**续烧不重置**（可能落在批次上界之上，
  MySQL 式 gap 接受）；旧名计数键清空，之后同名新表从 1 开始；
- 旧名即刻 1146（读/写/prepared 语句 re-exec——on_execute 走 execute 入口重新解析 catalog，
  干净报错不悬挂）；改名前 PREPARE、改名后 EXECUTE 的用例由 e2e 钉死；
- 拒绝矩阵：目标被占 1050（大小写不敏感比对，同表 case-only 改名允许）、源缺失 1146、
  多对形式与跨库限定名 1235。

### ALTER/CREATE/DROP INDEX + CREATE TABLE 内联 KEY
- `ALTER TABLE t ADD [UNIQUE] INDEX` / `DROP INDEX` 与 `CREATE [UNIQUE] INDEX` /
  `DROP INDEX i ON t`、CREATE TABLE 内联 `KEY`/`UNIQUE KEY` 全部收敛到同一对执行器
  （`ddl_alter::create_index`/`drop_index`，id 由 `catalog::next_index_id` 编号）；
- **UNIQUE 预检**在 catalog 条目落地前读当前已提交快照：既有重复 → 大声 1062（"the column
  already holds duplicates"），干净拒绝不留残留；建索引后新写冲突照常 1062；
- 在线回填（`backfill_index`）：catalog 提交后重扫全部 live 行，条目按 slot owner 路由
  （跨 owner 走 2PC，本地键集单批写），规划器的少读由 wide-scan fallback 界定（M2 语义）；
- DROP INDEX 先落 catalog 再扫条目（孤儿键不可达即无害）；复合/前缀键翻译期 1235。

### SHOW 面 + sysvar 表（`exec/show.rs` 307、`front/vars.rs` 379）
- `SHOW CREATE TABLE t`：由 schema 确定性渲染 MySQL 风格 DDL（反引号、类型、NULL/NOT NULL/
  DEFAULT NULL、AUTO_INCREMENT、PRIMARY KEY、KEY/UNIQUE KEY、starrocks 模型的 DUPLICATE KEY/
  DISTRIBUTED BY、ENGINE=——row 引擎报 InnoDB、列存报 columnar），**往返一致**（渲染文本
  重新执行可重建等价 schema，单测 + e2e 双钉）；
- `SHOW DATABASES`：单库模型——隐式 `rdb` + 会话 USE 目标（去重、名序稳定）；
- `SHOW [GLOBAL|SESSION] VARIABLES [LIKE 'pat']`：与 @@-query 同一张 sysvar 表（25 项：
  version/max_allowed_packet/wait_timeout/sql_mode/transaction_isolation/max_connections=151/
  auto_increment_increment=1/utf8mb4 字符集组等），LIKE 走 SHOW 专用**大小写不敏感**匹配
  （`%`/`_` 通配；数据 LIKE 保持 bytewise，gap-matrix 决策 3），WHERE 形态 1235；
- `SHOW STATUS`：只答诚实的计数器——`Uptime`（进程启动秒数），不伪造连接/负载计数。

### 会话函数 + USE（`exec/session_funcs.rs` 355）
- 连接身份经 `SessionInfo { user, connection_id }` 显式穿透：`serve.rs` 握手期由认证用户 +
  peer host + 连接 id 构造，存于每连接 `SqlSession`（无可变全局）；
- `DATABASE()`/`SCHEMA()`/`USER()`/`CURRENT_USER()`/`SESSION_USER()`/`CONNECTION_ID()` 的
  绑定发生在**每次执行**的 executor 入口（`session_funcs::substitute` 把零参调用原位改写为
  字面值）：求值层保持 session-free 的纯函数边界，同时保住 MySQL 语义——PREPARE 后跨 USE 的
  re-exec 读到当前库，且语句内一致；
- `USE db` 仅设置会话库（无库注册表可校验，任意名接受），`SHOW DATABASES` 与 `DATABASE()`
  随之反映；限定名保持"single-part names only"的大声拒绝（`db.table` 形态 1235，单库模型
  语义明确化）。

## e2e（`tests/`，待提交工作区）

- `tests/sql_ddl_surface_e2e.rs`（382 行，5 用例，共享 `common::mysql` 脚手架）：
  - TRUNCATE：行清零、唯一/二级条目随旧 table_id 清扫（旧唯一值可重插不 1062、旧值索引
    查询为空、新行唯一约束照常生效）、AUTO_INCREMENT 重置为 1、INSERT..SELECT 回填、
    事务内拒绝 1235 且行无损；
  - RENAME：新名即刻可读（计数 + 逐行）、二级索引在新名下仍被规划（EXPLAIN IndexScan）、
    旧名读/写 1146、**prepared 旧名 re-exec 干净报错（timeout 包裹，悬挂即失败）**、
    ALTER..RENAME TO 同执行器、AUTO_INCREMENT 跨两次改名续烧（严格递增）、拒绝矩阵
    （1050/1146/跨库 1235/多对 1235）、case-only 改名允许；
  - 索引 DDL 形态全家：CREATE INDEX / ALTER ADD INDEX 点查走索引（EXPLAIN 断言）、ADD
    UNIQUE 既有重复大声 1062 + 新写冲突 1062、DROP INDEX 回退全扫且结果不变、内联
    KEY/UNIQUE KEY 建表、复合/前缀 1235（每索引一列——两个索引共享同列会互扫条目，
    M2 键空间语义，套件按列错开）；
  - SHOW 面：SHOW CREATE TABLE 三表**逐字形状**断言（普通/唯一键/AUTO_INCREMENT）+ 渲染
    DDL 重建往返；SHOW DATABASES（rdb + USE 目标）；SHOW VARIABLES 无过滤 ≥25 项 +
    max_connections=151 / auto_increment_increment=1 / version 具名断言 + LIKE 'char%'/
    'CHAR%'（大小写不敏感）+ '%timeout'（中缀 %）+ '_ax_connections'（单字符 _）+
    GLOBAL/SESSION 前缀 + WHERE 1235；SHOW STATUS Uptime ≥ 0；
  - 会话函数：USER() 前缀 root@ + CURRENT_USER/SESSION_USER 同值、CONNECTION_ID 连接内
    稳定/跨连接相异、DATABASE/SCHEMA 跟随 USE、**PREPARE 在 USE 前 EXECUTE 在后读到新库**
    （per-exec 绑定）。
- `tests/sql_ddl_surface_cluster_e2e.rs`（183 行，1 用例，`start_sql_cluster` 三进程，
  `sql_ddl_visibility_e2e` 模式）：TRUNCATE ack 即刻——旧行经**每个**节点不可达；新表 id
  下新写（含旧唯一值重用）全集群可见、被扫旧值不再应答索引查询；RENAME ack 即刻——新名
  在 followers 带全量数据可读、旧名 1146、SHOW TABLES 反映；SHOW CREATE TABLE 各节点渲染
  逐字一致；follower 上 TRUNCATE/RENAME 以 "requires the raft leader" 拒绝（控制面约定）。

## 验证

- `cargo test --test sql_ddl_surface_e2e`：**5/5 绿（连跑两轮）**；
  `cargo test --test sql_ddl_surface_cluster_e2e`：**1/1 绿（连跑两轮）**；
  `cargo fmt --check` 干净。
- 单测随落地工作区：`exec/ddl_alter_tests.rs`（283）、`parse/translate_ddl_tests.rs`（181）、
  `exec/show_tests.rs`（318，含渲染往返）、`exec/session_funcs.rs` 内嵌用例；全量
  `cargo test --workspace` 以落地提交的 CI 运行为准。

## 关联与残留

- 计划：`plans/2026-10-06-mysql-gap/m4-ddl-session-surface.md`（D、E 组 P0/P1 销账）。
  P2 保持延期：**复合/前缀索引**（M2 索引编码恰一列值）、**ALTER COLUMN**
  （ADD/MODIFY/DROP COLUMN，与 schema 演进一起重估）、KILL、多对 RENAME、SHOW rowset 上的
  表达式引擎（WHERE）。
- 后续：M5 e2e 盲区（outer join / failover / `common::mysql` 收敛，已落地见
  `mysql-m5-e2e-blindspots.md`）。
- e2e 钉死的实现侧注意点：RENAME 按原始值搬计数器，改名后首个自增 id 可能越过预留批次
  上界（gap，非重置）；USE 不校验目标库（无注册表）；同一列上两个索引共享键空间（DROP
  其一会扫掉另一条的条目）——均已按现状断言或绕开。
