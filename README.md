# rdb

支持 Redis Cluster 协议的可持久化 KV 存储（Rust 实现）。

> **实现状态**：Rust 实现位于仓库根目录（cargo workspace），为唯一在维护实现；
> Go 原实现已归档至 [`archive/go/`](./archive/go/)（git mv 保留历史，不再维护）。
> Rust 与 Go 的架构/行为差异见 [COMPAT.md](./COMPAT.md)。

## 特性

- **RESP 数据面**：string / keys / Hash / Set / List / ZSet / JSON / VectorSet 七类命令族
  （含 `BLPOP`/`BZPOPMIN` 等阻塞命令、全类型统一 TTL）；JSON 为 RedisJSON v1 legacy
  确定路径子集，VectorSet 为暴力 cosine 相似度子集。
- **Redis Cluster 协议**：CRC16 slot 路由、`MOVED` 重定向、hash-tag；可通过
  `redis-benchmark` / `redis-py-cluster` 直接压测与访问。
- **Raft 控制面**：openraft 复制集群元数据（实例列表、备份映射、迁移任务），
  HTTP `join` / `depart` / `get` 控制 API（与 Go 实现字节兼容）。
- **高可用**：Raft 心跳观察 + `backup_target_map` 实现故障节点与备份实例的槽位切换；
  另起只读备份实例（`backup_bind`）。
- **数据迁移**：`migrate task <slot> <src> <dst>` 单命令编排无损 slot 迁移（redis-cli
  `--cluster reshard` 协议兼容：SETSLOT MIGRATING/IMPORTING/NODE、ASKING、GETKEYSINSLOT、
  MIGRATE/DUMP/RESTORE 传输），`migrate list` 返回任务 JSON；slot 归属经 raft
  `slot_owner_map` 收敛。
- **Lite MQ**：Streams 流命令族（RocketMQ 语义主题/队列、消费组、PEL 窗口）。
- **SQL 数据面**：MySQL 协议接入（opensrv-mysql）、MVCC 快照隔离事务、二级/唯一索引、
  raft 全局时间戳、跨节点 2PC 写与 scatter-gather 读。
- **FT.\* 搜索**：BM25 全文检索（jieba 中文分词）+ SQ8/SPANN 向量 ANN。
- **可观测性**：Prometheus metrics（`rdb_command_latency` 直方图、`raft_stats` gauge）。

## 构建

```bash
cargo build --release
```

- 产物：服务二进制 `target/release/rdb`、压测工具 `target/release/rdb-bench`。
- `.cargo/config.toml` 已配置 `--cfg tokio_unstable`（规避 tokio multi_thread LIFO slot
  丢唤醒冻结，详见 [COMPAT.md](./COMPAT.md) "Critical build requirement"）。

## 运行

### 单实例（快捷模式）

一条命令得到完整可用的单实例：无 yaml、无环境变量、无手工 `CLUSTER INIT`，重启幂等：

```bash
./target/release/rdb standalone [--host H] [--port P] [--dir PATH] [--token X]
# 默认 host=127.0.0.1 port=6379 dir=./rdb-standalone
```

- 首次启动生成随机 token（32 hex），写入 `{dir}/standalone_token`（0600）并打印到 stderr；
  重启复用同一 token；`--token` 显式指定并覆写文件。同一个 token 用于所有协议面：
  RESP `AUTH`、MySQL 密码（用户 `root`）、Kafka SASL PLAIN、ES/S3/RocksMQ HTTP 的 Bearer。
- 多协议前端随基端口一起拉起（偏移沿用 e2e 约定）：

  | 端口 | 协议 |
  | --- | --- |
  | `P` | RESP（Redis 协议） |
  | `P+2` | monitor `/metrics` |
  | `P+3` / `P+4` | raft HTTP / raft TCP |
  | `P+5` / `P+6` | MySQL wire / SQL 2PC RPC |
  | `P+7` / `P+8` | Kafka wire / RocksMQ HTTP |
  | `P+9` / `P+10` | ES HTTP / S3 HTTP |

  （`P+1` 预留给 backup 面，standalone 下不启用。）
- 同一 token 直连各协议（默认端口下）：

  ```bash
  TOKEN=$(cat ./rdb-standalone/standalone_token)
  redis-cli -h 127.0.0.1 -p 6379 -a "$TOKEN" SET k v          # RESP
  mysql -h 127.0.0.1 -P 6384 -uroot -p"$TOKEN"                # MySQL wire
  kcat -b 127.0.0.1:6386 -X security.protocol=SASL_PLAIN \
        -X sasl.mechanisms=PLAIN -X sasl.username=root -X sasl.password="$TOKEN" -L   # Kafka
  curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:6388/_cat   # ES
  curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:6389/       # S3
  ```

- 自动完成：单成员 raft 引导（无需 `RAFT_BOOTSTRAP`）→ 等本节点成为 leader → 幂等写入
  单节点拓扑与 `sql_nodes` 注册表 → `CLUSTER INFO/NODES/SLOTS` 直接可用。
- 护栏：`standalone` 与 `RAFT_JOIN_ADDR` 互斥（exit 1）；store 里已有多节点拓扑时拒绝
  降级（exit 1）；已初始化的 raft 不会二次 initialize。

### 多实例（集群模式）

配置复用 [`config/`](./config/) 下的多实例 yaml（`bind`、`store_path`、`raft_bind_address`、
`raft_http_bind_address`、`raft_token`、`backup_target_map` 等）：

```bash
# 首个节点（引导 raft 集群）
RAFT_BOOTSTRAP=true ./target/release/rdb -config config/conf_32681.yaml

# 其余节点（加入已有集群）
RAFT_JOIN_ADDR=127.0.0.1:12681 ./target/release/rdb -config config/conf_32683.yaml
```

环境变量：

| 变量 | 作用 |
| --- | --- |
| `RAFT_BOOTSTRAP=true` | 引导第一个 raft 节点（仅精确匹配 `true`） |
| `RAFT_JOIN_ADDR=<host:port>` | 向已有节点发起 join（raft HTTP 地址） |
| `RDB_WORKER_THREADS=N` | tokio worker 线程数（默认对齐 Go NumCPU） |
| `RDB_CURRENT_THREAD=1` | 诊断用单线程 runtime（无 LIFO slot，冻结为零） |
| `RDB_BEACON=1` | 心跳诊断日志（静默任务死亡 canary） |
| `RDB_DEBUG_REPL=1` | 每 peer 复制进度调试输出 |

## 压测

仓库自带负载发生器 `rdb-bench`（[`bench/`](./bench/)）：

```bash
./target/release/rdb-bench --addr 127.0.0.1:6379 --token <t> \
  --workload mixed --clients 16 --pipeline 16
```

`--workload` 支持 `ping | set | get | mixed | xadd | xreadgroup | ...`；
pipeline > 1 时延迟按每批 RTT 采样。exit code：参数错 2、服务端错误应答 1。

标准 `redis-benchmark` 亦可直接使用（Redis Cluster 协议兼容）：

```bash
redis-benchmark -h 127.0.0.1 -p 32680 -t set,get -n 1000000 -c 500 -q -r 100000000000000 -d 50 --cluster
```

## Benchmark（实跑）

Rust release 单实例（rdb-bench，本机实测）：
ping **499.9k ops/s**（p99 0.067ms）、get **481.7k ops/s**、
set **4.55k ops/s**（同步 RocksDB 写为主）、mixed **10.75k ops/s**、
mixed+pipeline16 **12.6k ops/s**（批 RTT p99 31ms）。

Go 时代 3 节点集群（redis-benchmark，DELL XPS i7-8550U 8 核）：
SET **53,087** requests/s（p50 5.975ms）、GET **54,145** requests/s（p50 3.271ms）。

## 文档

- [agents.md](./agents.md) — Agent 模块索引；Rust 实现模块文档见 [agents/rust/](./agents/rust/)
- [features/](./features/) — 特性文档：KV 存储 / Cluster 协议 / Raft HA / 迁移 / Lite MQ / SQL 数据面
- [COMPAT.md](./COMPAT.md) — Rust 与 Go 归档实现的兼容性与偏差清单
- [config/](./config/) — 多实例部署 yaml；[scrtips/](./scrtips/) — 部署与压测脚本

## Roadmap

- [ ] 进一步的性能优化

## 参考

- 一些思考：[传送门](https://blog.mself.top/post/kv/)
- [sdb — 基于 rpc 的 KV 持久化存储](https://github.com/yemingfeng/sdb)
