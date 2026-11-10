# 2026-11-10 · tests/ 按功能分组重组（目录式测试 crate）

## 动机

tests/ 根目录平铺 **120 个** e2e 文件（`tests/<name>.rs`），Cargo 为每个
文件自动发现一个集成 target —— 加上目录 crate `s3_e2e/` 共 **121 个**测试
二进制。`cargo test --workspace` 的墙钟大头不在编译而在**链接**：每个二进制
独立链接整棵依赖树与 harness，改一行 src、全量重跑就要重复链接 121 次。

重组为「目录式测试 crate」：每个功能组一个目录，`tests/<组>/main.rs` 是
聚合根（`#[path]` 引入共享 harness + 按字母序 `mod` 组内文件），组内每个
e2e 文件退化为 crate 内模块。集成 target **121 → 13**：

- 9 个聚合组 crate：`kv` / `sql` / `lite` / `kafka` / `es` / `turso` /
  `rocksmq` / `search` / `cluster`（120 个根级文件全部收编于此）；
- `s3`：目录 crate，原名 `s3_e2e` 改名而来（用例本就写在 main.rs）；
- 3 个保留独立的 `[[test]]` target：`sql_oracle_cluster_e2e` /
  `ha_failover` / `raft_cluster_e2e`（理由见「例外与理由」）。

搬移一律 `git mv`（历史可追）。文件内容的系统性改动只有一处：harness 引用
`mod common;`（及 `mod es_common;` 等组共享件）→ `use crate::common;`，
**1:1 单行替换**，文件行数不变 —— COMPAT 台账与 `features/*.md` 中指向
tests 用例的行号锚点不漂移。共享件目录 `common/`（mod.rs 冻结在 800 行 +
`lite.rs` / `mq.rs` / `mysql.rs` 子模块）、`es_common/`、
`kafka_front_common/`、`backup_surface_common/` **原地不动**，由各组
main.rs 以 `#[path = "../common/mod.rs"]` 形式引入。

## 结构（新旧映射）

旧：`tests/<name>.rs`（一文件一 target）→ 新：`tests/<组>/<name>.rs`
（一组一 target）。下表文件数不含 `main.rs`；分组清单按重组后目录实际
内容记录，略去 `.rs` 后缀、字母序。

| 组 | target | 文件数 | 覆盖面 |
|---|---|---|---|
| `kv/` | `kv` | 22 | RESP/KV 数据面 |
| `sql/` | `sql` | 33 | MySQL 协议 SQL 数据面 |
| `lite/` | `lite` | 22 | Lite 流 / MQ-lite |
| `kafka/` | `kafka` | 12 | Kafka wire front |
| `es/` | `es` | 4 | ES 兼容 HTTP front |
| `turso/` | `turso` | 8 | 内嵌 turso 引擎 |
| `rocksmq/` | `rocksmq` | 3 | RocksMQ HTTP front |
| `search/` | `search` | 3 | FT.* 全文检索 |
| `cluster/` | `cluster` | 14 | 控制面 / HA / 进程生命周期 / 迁移 |
| `s3/` | `s3` | — | S3 前端 e2e（用例在 main.rs，`client` / `fixture` 为支撑模块） |

- **kv（22）**：bits_e2e、blocking_concurrency_e2e、ds_e2e、expire_e2e、
  gaps_e2e、hash_read_e2e、hash_set_e2e、json_e2e、keys_more_e2e、
  kv_newcmds_proc_e2e、list_e2e、resp_e2e、resp_reply_semantics、
  server_surface_e2e、set_more_e2e、string_e2e、string_more_e2e、tx_e2e、
  vectorset_e2e、wire_families_e2e、zset_e2e、zset_more_e2e。
- **sql（33）**：auto_increment_e2e、columnar_e2e、mysql_compat_e2e、
  sql_2pc_e2e、sql_composite_pk_e2e、sql_ddl_surface_cluster_e2e、
  sql_ddl_surface_e2e、sql_ddl_visibility_e2e、sql_decimal_e2e、
  sql_dist_read_e2e、sql_e2e、sql_failover_e2e、sql_funcs_control_e2e、
  sql_funcs_datetime_e2e、sql_funcs_numeric_e2e、sql_funcs_string_e2e、
  sql_index_e2e、sql_insert_select_e2e、sql_join_cluster_e2e、
  **sql_oracle_cluster_e2e（独立 target）**、sql_outer_join_e2e、
  sql_query_semantics_e2e、sql_rename_gc_e2e、sql_restart_e2e、
  sql_setop_cluster_e2e、sql_subquery_e2e、sql_txn_e2e、sql_types_e2e、
  sql_upsert_cluster_e2e、sql_upsert_e2e、sql_vector_e2e、
  starrocks_model_e2e、txn_semantics_e2e。
- **lite（22）**：flushdb_lite_e2e、lite_claim_opts_e2e、lite_consumer_gc_e2e、
  lite_consumer_gc_proc_e2e、lite_delay_e2e、lite_delay_migrate_e2e、
  lite_delay_proc_e2e、lite_dlq_e2e、lite_dlq_triggers_e2e、lite_e2e、
  lite_group_dlq_e2e、lite_group_e2e、lite_multistream_e2e、lite_ordered_e2e、
  lite_pel_e2e、lite_proc_e2e、lite_reap_e2e、lite_redeliver_e2e、
  lite_redeliver_proc_e2e、lite_streams_e2e、lite_trim_minid_e2e、
  lite_xinfo_full_e2e。
- **kafka（12）**：kafka_admin_e2e、kafka_codec_e2e、kafka_fetch_e2e、
  kafka_group_e2e、kafka_group_failover_e2e、kafka_headers_fidelity_e2e、
  kafka_headers_roundtrip_e2e、kafka_offsets_e2e、kafka_produce_e2e、
  kafka_rename_ledger_e2e、kafka_topics_e2e、kafka_wire_e2e。
- **es（4）**：es_auth_e2e、es_bulk_interop_e2e、es_e2e、es_search_e2e。
- **turso（8）**：turso_catalog_e2e、turso_crud_e2e、turso_parity_e2e、
  turso_setops_e2e、turso_smoke_e2e、turso_sqlite_interop_e2e、turso_txn_e2e、
  turso_vector_e2e。（`turso_sqlite_interop_e2e` 为重组后直接落在新布局的
  在途新增用例，不在 120 个根级文件的搬移账内。）
- **rocksmq（3）**：rocksmq_batch_range_e2e、rocksmq_http_e2e、
  rocksmq_wait_pending_e2e。
- **search（3）**：ft_highlight_e2e、search_e2e、wire_ft_config_e2e。
- **cluster（14）**：asking_restore_e2e、backup_failover_e2e、
  backup_readonly_e2e、backup_surface_e2e、**ha_failover（独立 target）**、
  migrate_e2e、process_cluster_e2e、process_failover_e2e、process_metrics_e2e、
  process_sigterm_e2e、**raft_cluster_e2e（独立 target）**、raft_transport、
  routing_newcmds_e2e、standalone_e2e。
- **s3**：`s3_e2e/` → `s3/`（main.rs + client.rs + fixture.rs 三文件整体
  改名，内容零改动）。

## 例外与理由

- **3 个 RAFT_BOOTSTRAP 文件保持独立 target**（Cargo.toml 3 个 `[[test]]`
  name/path 声明，文件物理放组目录、不挂组 main.rs）：三者都
  `std::env::set_var("RAFT_BOOTSTRAP", …)` —— env 是**进程级**的，
  `new_raft_node` 直接读进程 env 决定 bootstrap 还是 join；并入组 crate
  会让同进程并行的其他用例意外各自 bootstrap 出独立集群，拓扑语义被污染。
  这三个文件自带 harness、不引 common，重组中内容零改动（纯 rename）。
- **`RDB_SQL_GC_PERIOD_MS` 并入 sql 组**（`sql_rename_gc_e2e` 用
  `set_var` 缩短 GC 周期后 spawn 真实二进制）：该 env 只被子进程在 GC
  循环启动时读一次，且缩短周期对并发读到同一值的任何子进程**语义安全** ——
  GC 只删 watermark 不可见的行版本与已 drop 表数据，活跃快照永远读不到
  （结论记录在该测试头注）。故无需独立 target。
- **`s3_e2e` → `s3` 重命名**：目录 crate 的 target 名随目录名走
  （`s3_e2e` → `s3`）。全仓 grep 无任何文档/脚本引用旧 target 名，
  低风险；同时统一「组目录名 = target 名」的直觉。

## 门禁（零丢失）

- 重组前后各跑一次 `cargo test --workspace -- --list` 导出清单，**测试
  总数一致**（零丢失）。
- 组 crate 内用例名带 mod 路径前缀（旧 `foo_case` → 新 `foo_e2e::foo_case`），
  比对按「**去 mod 前缀后的用例名集合**」做集合差，前后集合相等。
- 3 个独立 target 照常出现在清单中。

## 后续

- 新增测试的落位 / 挂 mod / env 副作用独立 target / 行数上限等约定成文于
  `tests/README.md`。
- 可选后续（本次不做）：CI 守护脚本 —— 扫描 `tests/<组>/*.rs` 中未出现在
  该组 main.rs mod 列表、也无对应 `[[test]]` 声明的文件，防止「新文件忘挂
  main.rs」静默不编译不运行。

## 并行度封顶

组 crate 把最多 161 个 e2e 收进**同一进程**并行跑，同进程并发节点数成为
free-port TOCTOU 撞车率的主变量（表现：EADDRINUSE 击穿 harness 4 次重试、
或 per-test 目录名撞端口导致同进程 RocksDB LOCK 冲突）。处置：
`.cargo/config.toml` 增加 `[env] RUST_TEST_THREADS = "4"`（对齐 CI runner
核数；16 线程满并发实测 sql 组两轮各 2 例撞车，封 4 后多轮全绿）。
