# standalone 单实例快捷初始化（2026-11-09）

## 背景

起一个"能用的单实例"原先要 4 步：手写 yaml（必填键多、默认路径不存在）、
`RAFT_BOOTSTRAP=true` 首启、客户端 `AUTH <raft_token>`、手工 `CLUSTER INIT` 才能过
ready 门禁。仓库中无任何 standalone/一键形态。

## 落地

`rdb standalone [--host H] [--port P] [--dir PATH] [--token X]`，默认
`127.0.0.1:6379`、`./rdb-standalone`：

- **多协议一起拉起**（单实例不是只有 RESP）：端口从基端口按 e2e 约定派生 ——
  RESP `P`、monitor `P+2`、raft HTTP `P+3`、raft TCP `P+4`、MySQL `P+5`、SQL 2PC RPC
  `P+6`、Kafka `P+7`、RocksMQ `P+8`、ES `P+9`、S3 `P+10`（`P+1` 预留 backup，不启用）。
- **一个凭证走所有协议面**：首次生成 32 hex 随机 token，写 `{dir}/standalone_token`
  （0600）并打印 stderr；重启复用；`--token` 显式覆写。同一 token = RESP `AUTH`、
  MySQL 密码（root）、Kafka SASL PLAIN、ES/S3/RocksMQ Bearer。源码零硬编码凭证。
- **bootstrap/拓扑自动化**：`new_raft_node` 增 `force_bootstrap`（env 路径不变）；
  后台 `ensure_topology` 幂等任务 —— 本地 FSM 拓扑为空 → 等本节点 leader（60s 上限，
  超时 exit 1）→ apply 单节点拓扑 + `sql_nodes` 种子（复用 `merged_registry`，与
  `cluster init` 同构）；等于自身 bind → 跳过；非空且不同 → **exit 1 拒绝降级**
  （store 属于多节点集群时绝不静默覆盖）。既有 3s `spawn_topology_sync` 随后翻
  `cluster_ready`。
- **护栏**：`standalone` + `RAFT_JOIN_ADDR` 互斥（exit 1）；raft 已初始化则跳过
  initialize（D7b 语义保持）。多实例模式（yaml + env bootstrap/join）完全不动。

## 文件

- `src/standalone.rs`（新增，316 行）：旗标解析/端口派生/token 持久化/`build_conf`/
  `topology_plan` 纯函数 + `spawn_ensure_topology`。
- `src/standalone_tests.rs`（新增）：上述纯函数单测（拆分守 400 行上限）。
- `src/main.rs`：`startup_conf` 分派（standalone 免 yaml）、强制引导与种子任务挂接。
- `src/rcache/mod.rs`：`new_raft_node` 增 `force_bootstrap` 参数。
- `tests/standalone_e2e.rs`（新增）：全协议探活（MySQL greeting、Kafka ApiVersions、
  ES/S3/RocksMQ 401→Bearer、sql_rpc）、自动拓扑（无手工 INIT）、重启幂等（token/数据/
  ready）、多节点 store 拒绝降级（exit 1）。

## 验证

`cargo build --release`；`cargo test --bin rdb`（9/9，含 standalone 5 条单测）；
`cargo test --test standalone_e2e`（e2e）；env bootstrap/join 相关 e2e 回归不回归
（`new_raft_node` 旧调用点仅补 `false` 实参）。
