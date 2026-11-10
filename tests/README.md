# tests/ 目录说明

e2e 测试按**功能分组**组织：每个功能组目录是**单个测试 crate**
（`tests/<组>/main.rs` 为聚合根，Cargo 自动发现为 `--test <组>` target），
组内每个 e2e 文件是 crate 内的一个 `mod`。重组动机、新旧映射与零丢失
门禁见 `features/changelog/2026-11-10/tests-reorg.md`。

## 目录地图

### 共享 harness（原地不动）

- `common/` —— 全组共享 harness：spawn 真实 `rdb` 二进制、写临时 yaml、
  RESP2/MySQL 客户端、集群生命周期驱动。`mod.rs` 已达 800 行上限，
  **冻结不再增改**；领域子模块 `lite.rs` / `mq.rs` / `mysql.rs`。
- `es_common/`、`kafka_front_common/`、`backup_surface_common/` —— 组共享
  件，原地不动，由对应组 crate 的 `main.rs` 经 `#[path]` 引入，组内文件以
  `use crate::es_common;` 等方式消费。

### 功能组（每个 = 单个测试 crate，`<组>/main.rs` 为聚合根）

| 组 | target | 文件数 | 覆盖面 | 例外（独立 target） |
|---|---|---|---|---|
| `kv/` | `kv` | 22 | RESP/KV 数据面 | — |
| `sql/` | `sql` | 33 | MySQL 协议 SQL | `sql_oracle_cluster_e2e` |
| `lite/` | `lite` | 22 | Lite 流 / MQ-lite | — |
| `kafka/` | `kafka` | 12 | Kafka wire front | — |
| `es/` | `es` | 4 | ES 兼容 HTTP front | — |
| `turso/` | `turso` | 8 | 内嵌 turso 引擎 | — |
| `rocksmq/` | `rocksmq` | 3 | RocksMQ HTTP front | — |
| `search/` | `search` | 3 | FT.* 全文检索 | — |
| `cluster/` | `cluster` | 14 | 控制面 / HA / 进程生命周期 / 迁移 | `ha_failover`、`raft_cluster_e2e` |
| `s3/` | `s3` | — | S3 前端 e2e（用例在 `main.rs`，`client.rs`/`fixture.rs` 为支撑模块） | — |

例外文件物理上仍放组目录，但在 `Cargo.toml` 以 `[[test]] name/path` 声明为
独立 target（原因见约定 3），**不**挂在组 `main.rs` 的 mod 列表里。

## 新增测试约定

1. **落位**：新 e2e 文件放进对应功能组目录，并在该组 `main.rs` 的 mod
   列表**按字母序**追加一行 `mod <name>;`（忘挂 = 不编译不运行，见
   changelog「后续」的 CI 守护项）。
2. **挂 harness**：文件内首行 `use crate::common;`；如需组共享件再加
   对应的 `use crate::es_common;`（es 组）/ `use crate::kafka_front_common;`
   （kafka 组）/ `use crate::backup_surface_common;`（cluster 组备份面）。
   纯内进程 wire 用例可自带 harness 不引 common（先例 `resp_e2e.rs`）。
3. **env 变量副作用测试必须独立 target**：任何 `std::env::set_var`（如
   `RAFT_BOOTSTRAP`）的测试不得并入组 crate —— env 是进程级的，会污染同
   crate 内并行用例。做法：单文件放组目录，并在 `Cargo.toml` 声明
   `[[test]] name = "<name>" path = "tests/<组>/<name>.rs"`（先例：
   `sql_oracle_cluster_e2e`、`ha_failover`、`raft_cluster_e2e`）。仅影响
   子进程读数且语义安全的 env（如 `RDB_SQL_GC_PERIOD_MS`）可并入组 crate。
4. **单文件 <400 行**：超出按命令族/主题拆分新文件。
5. **纯函数式风格**：不引入 class/OOP，helper 一律自由函数组合。

## 运行方式

```sh
cargo test --test kv              # 单个功能组
cargo test --test sql -- sql_e2e  # 组内按 mod 前缀过滤
cargo test --workspace            # 全量（10 个组 crate + 3 个独立 target）
cargo test --test ha_failover     # 独立 target 照常
```

并发度由 `.cargo/config.toml` 的 `[env] RUST_TEST_THREADS = "4"` 统一封顶：
e2e 每用例拉起真实多监听 rdb 节点（一个 free-:0 探测派生 ~10 个绑定端口），
同进程并行度直接决定 free-port TOCTOU 撞车率（EADDRINUSE 重试耗尽、或
per-test 目录名撞端口引发同进程 RocksDB LOCK 冲突）。封顶对齐 CI runner
核数，勿在本机调高后跑全量。

注意：组内用例名带 mod 路径前缀（如 `sql_e2e::xxx`），`-- --exact` 过滤时
需带前缀。

## 历史

本次由根级 120 个测试文件平铺重组而来（`git mv` 保留历史）；`s3_e2e`
目录随重组改名 `s3`（target 名 `s3_e2e` → `s3`，无外部引用，低风险）。
完整新旧映射、例外理由与门禁方法见
`features/changelog/2026-11-10/tests-reorg.md`。
