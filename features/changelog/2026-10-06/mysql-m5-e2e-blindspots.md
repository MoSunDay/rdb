# SQL M5：e2e 盲区补齐与测试基建收敛（outer join / leader-failover / common::mysql）

Commit: (working-tree, 随本提交入库)

## 背景

MySQL 差距计划 M5（`plans/2026-10-06-mysql-gap/m5-e2e-blindspots.md`）处理两类覆盖面问题：
**已实现但零 e2e**（LEFT/RIGHT OUTER JOIN、SQL 面真实 leader failover）与**样板重复**
（每个 SQL e2e 文件头部 ~90 行 connect/ddl/rows 样板被近复制）。本里程碑只动 `tests/`
（外加本 changelog），不改 `src/`。

## 交付一：`tests/common/mysql.rs`（新增 301 行，经 `common/mod.rs` 的 `pub mod mysql;` 挂载）

单一共享 MySQL e2e 脚手架，收敛既有 7 个套件的本地副本 + 吸收 `tests/sql_funcs_common/`
（137 行）与 `tests/sql_upsert_common/`(122 行) 两个近期共享模块（目录已删除）。API：

- 连接：`connect(node, user, pass)`（IO 重试 15s）、`connect_root(node)`（root/PASS）；
- 常量：`PASS` 与 errno `ER_BAD_FIELD_ERROR/ER_NO_SUCH_TABLE/ER_DUP_ENTRY/
  ER_PARSE_ERROR/ER_NOT_SUPPORTED_YET/ER_TRUNCATED_WRONG_VALUE/
  ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT`；
- leader 重试写：`ddl()`（panic 措辞保持 `ddl {sql}: {e}`）、`run()`（保持 AUTO_INCREMENT
  套件的 `run {sql}` 措辞，共享同一重试体）；
- 行读取：`rows`/`rows_ordered`（稳定排序语义原样保留）/`one`/`col`/`sorted_cells`/
  `col_types`（元数据）/`ints`/`pairs`/`triples`（text-cell 解码）；
- 探针：`server_error()`（委托 `common::mysql_server_error`）、`aff()`（affected-rows 立即读）、
  `wait_table(conn, table, want)`（SHOW TABLES 轮询）；
- 构造器：`s`/`int`/`i`（`i` 为 funcs 套件历史拼写，与 `int` 同体）；
- 单节点 world：`world(tag)`（node + root 连接）、`funcs_node(tag)`（M1 函数族夹具表 `f`）、
  `assert_exprs`（`SELECT <expr>` 电池）。

helper 逐行等价迁移（仅 `world` 的临时目录前缀归一为 `rdb-sql-world-*`，每测试仍按
`tag + pid` 隔离）。迁移与行数变化（前 → 后）：

| 套件 | 前 | 后 | 备注 |
| --- | --- | --- | --- |
| `sql_e2e.rs` | 405 | 332 | 删 connect/ddl/rows/rows_ordered/int/s |
| `sql_types_e2e.rs` | 556 | 379 | 删 ddl/rows/sorted_cells/s/col_types；超出 400 的自包含 DECIMAL 用例拆出（见下） |
| `sql_txn_e2e.rs` | 409 | 342 | 删 connect/ddl/rows/int/s |
| `sql_index_e2e.rs` | 393 | 331 | 删 connect/ddl/rows/int/s |
| `mysql_compat_e2e.rs` | 442 | 371 | 删 connect/ddl/rows/rows_stable(→`rows_ordered`)/int/s |
| `auto_increment_e2e.rs` | 387 | 321 | 删 connect/run/rows/int/s |
| `columnar_e2e.rs` | 360 | 282 | 删 connect/ddl/errno 常量/poll_catalog(→`wait_table`) |
| `sql_funcs_*_e2e.rs` ×4 | 343/301/358/315 | 342/300/357/314 | `sql_funcs_common` → `common::mysql`（仅改 use） |
| `sql_upsert_e2e.rs` | 387 | 386 | `sql_upsert_common` → `common::mysql`（仅改 use） |
| `sql_insert_select_e2e.rs` | 261 | 260 | 同上 |

拆分：`tests/sql_decimal_e2e.rs`（新增 139 行）承接 `decimal_exact_semantics_end_to_end`
（原 `sql_types_e2e.rs` 内 121 行自包含用例，字节级等价搬家），使 `sql_types_e2e.rs` 回到
400 行以内。

**未迁移（有意保留本地副本）**：集群/HA 编排类 `sql_2pc_e2e.rs`（742 行，自带
`http_get/err_of` 与带 node 取证参数的 `wait_table`）、`sql_join_cluster_e2e.rs`
（其 `col` 返回 `Vec<String>`，语义不同）、`sql_setop_cluster_e2e.rs`、
`sql_upsert_cluster_e2e.rs`、`sql_dist_read_e2e.rs`、`sql_restart_e2e.rs`、
`sql_composite_pk_e2e.rs`、`sql_ddl_visibility_e2e.rs`、`starrocks_model_e2e.rs`——
超出本次最小集，属后续分批收敛项；RESP/其它协议面的 `tx_e2e.rs` 等连接对象不同，
不在 MySQL 样板范畴。

## 交付二：`tests/sql_outer_join_e2e.rs`（新增 314 行，7 用例）

LEFT/RIGHT [OUTER] JOIN 此前 e2e 零覆盖（executor `join_sources` 已实现空补语义）。单节点
夹具：3 dept / 4 emp（dept1 双人、`dee` 无匹配、NULL 外键）/ bonus（bob 双行 → 匹配乘积）/
ghost（永不匹配）：

- `left_join_null_pads_the_right_side` — 基本左补 NULL；
- `left_join_multiplies_matches_and_no_match_pads_all` — 多匹配乘积 + 全体无匹配整侧补 NULL；
- `where_on_null_padded_column_is_anti_and_semi_join` — 补 NULL 列上 `IS NULL`（anti-join，
  含列名与别名两种写法）与 `IS NOT NULL`（semi-join）；
- `right_join_pads_left_side_and_equals_swapped_left` — RIGHT 左侧补 NULL（前缀列 NULL）+
  **RIGHT ≡ 换位 LEFT** 同行集断言 + 右向 anti-join；
- `outer_keyword_is_optional_in_both_directions` — LEFT/RIGHT 与 LEFT/RIGHT OUTER JOIN 等价；
- `outer_chains_filters_and_inner_mix` — `a LEFT JOIN b LEFT JOIN c` 链（中层与尾层各自补
  NULL）、WHERE 过滤 + 别名（semi 语义）、INNER 混排（内连接先淘汰 `hr`）；
- `indexed_join_key_keeps_outer_join_semantics` — join 键建二级索引后基本外连接与 anti-join
  结果不变（miss-side 索引交互）。

集群形态的 join gather 边界已由 `sql_join_cluster_e2e.rs`（内连接形态）覆盖同一
`join_sources` 循环，按计划后置。

## 交付三：`tests/sql_failover_e2e.rs`（新增 283 行，2 用例）

现有集群 e2e 从不杀 SQL leader（failover 只覆盖 RESP 侧）。三真实进程
（`start_sql_cluster`）+ SIGKILL leader：

- `kill9_sql_leader_new_leader_serves_ddl_ts_and_writes` — 杀前基线（DDL+3 行）与幸存
  follower 上的 prepared statement 先验证；kill 后：被杀者旧连接 10s 硬超时内**干净报错**
  （不挂起）；幸存者选出新 leader（`wait_leader` 轮询，`assert_ne!`）；新 leader 接受
  DDL（raft catalog）与 INSERT——活 band 行提交、尸体 band 行**响亮失败**
  （`2pc participant`/`unreachable`，绝不部分提交）；`/sql/ts?n=` 在新 leader 上 200 且
  区间单调（ts 分配器恢复、写入不断流）、另一幸存者 404 `not leader`；幸存 follower 连接上
  的 prepared statement 15s 硬超时内要么工作要么报 1027/`unreachable` 干净错误，且该连接
  仍可执行无 FROM 语句；
- `killed_leader_restarts_and_pre_kill_data_stays_visible` — 杀 leader → 新 leader 建表 →
  尸体以同一 config + 数据目录 `respawn()` → catalog（`wait_table`）与数据面
  （`poll_count`）追平后，**每个成员**都能读到杀前 9 行基线（MVCC 读一致性），随后跨 band
  多行写入恢复并全成员可见（11 行）。

时序不确定处全部用 deadline 轮询（选举 120s、追平 45s），失败断言附 `all_ctx` 日志。

## 验证

- 触达/新增套件全绿（`cargo test --test …` 分批逐套件）：`sql_e2e` 3/3、`sql_txn_e2e` 8/8、
  `sql_index_e2e` 8/8、`mysql_compat_e2e` 4/4、`auto_increment_e2e` 5/5、`columnar_e2e`
  2/2、`sql_types_e2e` 4/4、`sql_decimal_e2e` 1/1、`sql_funcs_string_e2e` 7/7、
  `sql_funcs_numeric_e2e` 8/8、`sql_funcs_datetime_e2e` 7/7、`sql_funcs_control_e2e` 7/7、
  `sql_upsert_e2e` 5/5、`sql_insert_select_e2e` 3/3、`sql_outer_join_e2e` 7/7、
  `sql_failover_e2e` 2/2（新套件复跑两轮无抖动）；
- `cargo fmt --check`：`tests/` 全部干净（工作区中 `src/sql/exec/ddl.rs` 存在并发 M4 工作的
  fmt 差异，非本里程碑文件，未触碰）；
- 迁移后 7 个套件不再含本地 `async fn connect` 副本（grep 归零于已迁移集合）。

## 关联与残留

- 计划：`plans/2026-10-06-mysql-gap/m5-e2e-blindspots.md`（覆盖面组销账）。
- 文档同步（`features/e2e-coverage.md` 登记、`agents/rust/sql.md` e2e 基建约定、
  `COMPAT.md` failover 语义差异如有）为 M5 计划清单项，随收尾一并补齐。
- 残留：上表 9 个集群/HA 套件的本地 connect 样板待后续分批切换到 `common::mysql`；
  failover e2e 暴露的行为语义（尸体 band 的读写响亮失败、重启追平）与计划中"HA failover
  of SQL reads is future work"（`src/sql/dist/gather.rs` 注释）一致，无需 src 变更。
