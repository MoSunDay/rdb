# Kafka 订阅/消费上线 P0 修复：重启世代死区 + 广告地址 + 连接配额 + 指标失真

Commit: HEAD（与 kafka/es/rocksmq/s3 前置批次同批入库）

## 背景
上线评估确认功能面就绪（组协议/消费/持久化 e2e 全绿），但有 1 个已确认 bug +
2 个部署级缺口阻断上线（详见 `features/kafka-front.md` "上线修复" 节）。

## 修复
1. **重启 generation 死区（bug）**：runtime 世代纯内存从 0 重计，0x20 账本却持久化
   旧值——重启前组达 gen N≥2 时，重启后合法新组（gen 1）的 OffsetCommit 持续被
   层2 账本检查拒 22 `ILLEGAL_GENERATION`，客户端需盲目 rejoin N-1 轮才追平。
   修法：组**首次 JoinGroup** 以 `ledger::scan_group` 取账本 max(generation) 播种
   runtime 计数起点（`state::new_group_seeded`），新组首个世代恒高于全部账本行。
   - `src/kafka/coordinator/state.rs`（`new_group_seeded`）、`join.rs`（播种 +
     单测）、`group_api.rs`/`conn.rs`（`shared` 透传）。
   - e2e `restart_wipes_membership_but_keeps_offsets` 改造为重启前 gen=3 的死区
     回归（修复前该用例即失败）；单测 `first_join_seeds_generation_from_ledger`。
2. **通配绑定广告 localhost（部署缺口）**：生产绑 0.0.0.0 时远程客户端 bootstrap
   拿到不可用地址。新增 `kafka_advertised_host`/`kafka_advertised_port` 配置键，
   `handshake::advertise` 优先采用（Metadata brokers 行 + FindCoordinator）。
   - e2e `advertised_overrides_beat_wildcard_bind`（0.0.0.0 绑定 + 覆盖，Metadata
     与 FindCoordinator 双验）。
3. **连接面零配额（部署缺口，配额级缓解；鉴权仍缺）**：进程级连接上限
   `kafka_max_connections`（0=默认 4096，超限 accept 即断）、空闲读超时 10min
   （对齐 connections.max.idle.ms）、单帧上限 100MiB→16MiB、帧缓冲跨请求复用。
   - `src/kafka/conn.rs`（ConnGuard 计数/读超时/缓冲复用）；e2e
     `connection_cap_closes_excess_sockets`。
4. **指标失真**：Fetch 长轮询 park 时长不再计入 `rdb_kafka_api_latency`
   （`fetch::handle_fetch` 返回 parked_ms，conn 侧扣除）；acks=0 无响应帧路径补打点。

## 验证
`cargo test --lib "kafka::"` 70 过；kafka 全套 e2e（wire/produce/fetch/offsets/
group/group_failover）14 过 + rocksmq_http 3 过 + lite 全套 41 过；
`scenario_kafka_sdk.sh`（真实 librdkafka）回归通过。
