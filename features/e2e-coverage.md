Commit: 98e17a5
# E2E 覆盖地图与缺口台账

> 缺口基线为 git `f071032`（2026-09-26 全量只读比对，30/188 命令零覆盖等）；基线缺口
> 已全部闭合（过程与修复叙事见 `changelog/2026-09-26/e2e-coverage-closeout.md`），
> 下文为覆盖现状 + 残余缺口。

## 测试层级地图

| 层级 | 载体 | 说明 |
|---|---|---|
| in-proc registry | `tests/common/lite.rs`（真实 store + 真实命令注册表） | 断言精确 RESP 字节，无 TCP |
| in-proc wire | `resp::serve` 临时监听（`resp_e2e`/`gaps_e2e`/`bits_e2e`/`server_surface_e2e`/`routing_newcmds_e2e`） | 真实编解码 + 原始 TcpStream |
| in-proc raft | `raft_cluster_e2e`/`raft_transport`/`ha_failover`/`sql_oracle_cluster_e2e` | 进程内 openraft 节点 |
| 进程级 e2e | `tests/common/mod.rs`（`CARGO_BIN_EXE_rdb` + 临时 yaml + 临时端口） | 最接近真实部署 |
| 前端专用 mini-harness | `kafka_front_common/`、`es_common/`、`s3_e2e/`、`rocksmq_http_e2e` | 真实二进制 + 手搓协议客户端 |
| shell 场景 | `scrtips/e2e_scenarios/scenario_*.sh`（run_all.sh glob，当前 9 个） | redis-cli/iredis/redis-py/mysql/confluent-kafka 等真实客户端 |

CI 入口：push 跑 `cargo test --workspace --no-fail-fast`（含全部 Rust e2e）；
夜间 `soak.yml` 跑 `soak_kill9.sh` + `run_all.sh`（soak 已装 confluent-kafka）。

## 覆盖现状（基线缺口 → 对应套件）

- **命令面 30 个零覆盖 → 0**：`hash_read_e2e`（HSETNX/HMGET/HEXISTS/HSTRLEN/HKEYS/HVALS/HRANDFIELD）、
  `set_more_e2e`（SPOP/SRANDMEMBER/SSCAN/SDIFF/SINTER/SMISMEMBER）、`zset_more_e2e`
  （ZCOUNT/ZMSCORE/ZREVRANK/ZRANDMEMBER/ZREM/ZPOPMAX/ZREVRANGEBYSCORE/ZLEXCOUNT/ZREMRANGEBYSCORE/
  ZRANGEBYLEX + ZREVRANGEBYLEX 首条成功路径）、`keys_more_e2e`（UNLINK/EXPIREAT/PEXPIREAT/PERSIST/
  RANDOMKEY/RENAMENX/TYPE/KEYS/CONFIG）、`list_e2e` 补 LPUSHX/RPUSHX、`search_e2e` 补 FT.DROPINDEX
  （与 FT.DROP 同 handler、连文档一起删——与 RediSearch 的 DROPINDEX-保文档语义不同，已钉住）。
- **RESTORE 成功路径 + ASKING 门（wire）**：`asking_restore_e2e`——IMPORTING 槽无 ASKING 得 MOVED、
  同连接 ASKING 单次放行、`SETSLOT STABLE` 复位；DUMP（库内 `ds::dump::dump_key`）→ wire RESTORE
  成功往返（typed 家族 re-root 到目标键）、REPLACE/BUSYKEY/ttl/ABSTTL(空操作)/坏载荷错误面。
- **JSON/Vector/FT/CONFIG 传输层盲区**：`wire_families_e2e`（JSON/Vector）+ `wire_ft_config_e2e`
  （FT.*/CONFIG）全部走真实 TCP（空拓扑单节点）。
- **进程级 backup 接管**：`backup_failover_e2e`（3 真进程 + `spawn_node_backup`、yaml 注入
  backup_target_map、kill -9 → MOVED 指向存活同伴的 backup 监听 → 可读/拒写 -READONLY → 重启回切）；
  `backup_surface_e2e`（readonly allowlist **87/87** 全量过门证明 + 33 个写命令负向；表与播种助手在
  `tests/backup_surface_common/`，另含 table 与 `readonly::ALLOWED` 的集合相等运行时断言）。
- **ES 前端**：`es_auth_e2e`（es_token 401 矩阵 + 双节点 remote-slot 400 `routing_exception` 精确信封）。
- **S3 checkpoint**：retention 剪裁（盘上 = 协议面 = 保留数）+ meta.json 文件清单与 S3 GET 字节一致性。
- **Kafka 边角**：`kafka_group_e2e` 补 LeaveGroup 未知成员/group（均 25）、DescribeGroups 未知
  group（group 级 NONE + state Dead，非 25）。
- **Shell 场景**：新增 `scenario_ha_failover.sh`（首次启用 `e2e_kill_node`，kill→MOVED-backup ~4-5s、
  回切 ~5s）、`scenario_lite_mq.sh`（XADD/XREADGROUP/XPENDING/XACK/XAUTOCLAIM + ORDERED/INFLIGHT）、
  `scenario_migrate.sh`（`migrate task` 全流程 + 反向回迁）；`scenario_redis_session.sh` 增
  redis-py `RedisCluster()` 探针（**已连接成功**：protocol=2 跳过未实现的 HELLO；无 redis-py 自跳过）。
- **漂移与卫生**：RESULTS.md/soak.yml 的“4 场景”漂移改为 glob 措辞；soak.yml 补装 confluent-kafka；
  `scrtips/redis-py.py`/`redis-async-py.py` 内嵌真实 raft token 换 `RDB_TOKEN` 环境变量占位。

## 残余（记录在案，非本次范围）

- `CONFIG` 仍是固定桩（无 GET/SET 子命令解析，仅回 `cluster-require-full-coverage/no`）；
  `RANDOMKEY` 同 slot 全键时确定性地返回字典序首键。
- redis-py 探针未覆盖 pipeline/pubsub/`CLUSTER SHARDS`（RESULTS.md known gaps）。
- `config/conf_3268*.yaml` 明文 raft token（P3 债务，非 e2e 范围）。
