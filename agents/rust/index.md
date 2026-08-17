Commit: d481b1d708c248f86be394189d01ca7305fc8528
# rust（Rust 重写实现）

## 职责
- Rust 版 rdb 的完整实现，位于 `rust/`：独立 cargo workspace（`rust/Cargo.toml`，成员 `rust/rdb` 与 `rust/bench`），服务二进制与库同名 `rdb`，压测工具 `rdb-bench`。
- 数据面：tokio TCP + RESP2 编解码（与 Go 的 redcon fork 字节对齐），命令分发、slot 路由与 `MOVED` 重定向；已支持 string/keys 族/Hash/Set/List/ZSet 命令（含 BLPOP/BZPOPMIN 等阻塞命令族，经 `ds/wait.rs` WaitHub 唤醒）与全类型统一 TTL（`ds/` 基座，七类结构分阶段推进）。
- 控制面：openraft 0.9.25 复制集群元数据（实例列表、备份映射、迁移任务），对外提供 HTTP join/depart/get。
- 存储：RocksDB 持久化，物理 key 带 `<slot>/` 十进制前缀（如 `5465/`，见 `store::slot_prefix`）。
- 与 Go 实现功能对齐：RESP 数据面与 Raft HTTP API 字节兼容；Raft TCP 线协议不同（openraft JSON 帧 vs hashicorp msgpack），兼容性细节与偏差清单见 `rust/COMPAT.md`。

## 边界
- 负责：整个 Rust 进程（入口 `rust/rdb/src/main.rs`）——RESP 接入、命令处理器、openraft 节点装配、HTTP 控制 API、HA 心跳观察、备份实例监听。
- 与 Go 代码（`internal/`、`cmd/`）无编译期依赖、无共享代码，仅行为对齐：共用 yaml 配置格式（`config/` 下文件可直接复用）、RESP 应答文本与 HTTP 路由。
- Go 与 Rust 节点不能混组同一 raft 集群（线协议不兼容），部署须全 Go 或全 Rust。

## 构建要求（关键）
- `rust/.cargo/config.toml` 设置 `rustflags = ["--cfg", "tokio_unstable"]`，使 `main.rs` 中 `Builder::disable_lifo_slot()` 编译生效：tokio multi_thread 默认 LIFO slot 在本负载下丢唤醒，导致整个 runtime 冻结（6s+ 停顿）。
- 在 `rust/` 目录外构建需显式 `RUSTFLAGS='--cfg tokio_unstable'`；缺该 cfg 时回退为带 LIFO slot 的 multi_thread runtime（冻结复现）。
- 环境变量：`RDB_CURRENT_THREAD=1` 改用 current_thread runtime（应急逃生，单线程）；`RDB_WORKER_THREADS=N` 设置 worker 数（默认对齐 Go NumCPU）；`RDB_BEACON=1` 开启诊断心跳（默认关闭）。
- 详见 `rust/COMPAT.md`。

## 模块结构
源码位于 `rust/rdb/src/`，布局对照 Go `internal/`：
- `main.rs`：进程入口——runtime 构建（含 `disable_lifo_slot`）、`-config` 参数解析、进程装配；私有 `mod beacon`。
- `conf.rs`：yaml 配置 `Config`（字段键名与 Go 一致）+ env 读取（`RAFT_BOOTSTRAP` 严格等于 `true` 才生效、`RAFT_JOIN_ADDR`），默认配置路径 `config/config.yml`。
- `resp/`：RESP 层。
  - `codec.rs`：RESP2 解析/写出，与 redcon fork 字节对齐（测试内联于 `codec_tests.rs`）；
  - `conn.rs`：连接状态机——AUTH 门（仅 `raft_token` 可通过）、白名单跳过路由、slot 路由与 MOVED、`catch_unwind` 兜底、延迟埋点；
  - `mod.rs`：bind/accept，一连接一 task。
- `command/`：命令注册表 `lookup` 与处理器。
  - `string.rs`：GET/SET/DEL/MGET/MSET/PING/QUIT/CONFIG；
  - `cluster.rs`：CLUSTER（init/nodes/test 等，拓扑读 `state::Shared.topology`）；
  - `raft_cmd.rs`：RAFT（help/stats/leader/nodes/set/get）；
  - `migrate.rs`：MIGRATE（任务经 raft 键 `migrate_task` 复制）。
  - `keys*.rs`：TYPE/EXISTS/DEL/UNLINK/EXPIRE 族（NX/XX/GT/LT）/TTL/PTTL/PERSIST/SCAN/KEYS/RANDOMKEY/RENAME(NX)（核心状态 `keys_core.rs`，游标类 `keys_scan.rs`）；
  - `hash_*.rs`：Hash 全命令——`hash_cmd.rs` 写/读、`hash_scan.rs` HGETALL/HKEYS/HVALS/HSCAN/HRANDFIELD、`hash_incr.rs` HINCRBY/HINCRBYFLOAT；
  - `set_*.rs`：Set 全命令——`set_cmd.rs`、`set_scan.rs` SSCAN/SRANDMEMBER、`setops_cmd.rs` SDIFF/SINTER/SUNION（±STORE）；
  - `mod.rs` 注册表为 async（`Handler -> HandlerFuture`），多 key 命令做 CROSSSLOT 校验。
  - `lite/`（crate 根 `src/lite/`）：流命令 XADD/XLEN/XRANGE/XTRIM/XDEL/XIDLE/XREAD/XREADGROUP/XACK/XGROUP/XINFO/XPICK。
- `lite/`：Lite Mode（RocketMQ 风格父主题 + 动态队列）。`mod.rs` 运行时与装配、`model.rs` 主题名/物理布局（slot 前缀取父主题名）、`select.rs` 队列挑选（round_robin/hash/least_backlog）、`append.rs` xadd/xrange/xtrim/xdel/xidle、`read.rs` xlen/xread/xreadgroup（单流）、`ack.rs` xack（同步持久化组水位）、`entries.rs` 条目扫描公共件、`group.rs` xgroup、`offset.rs` 组水位内存缓存 + 200ms 刷盘（kind-0x0E）、`info.rs` xinfo/xpick；空闲 TTL 复用统一过期信封，到期整流回收；无 PEL，重启自已提交水位 at-least-once 恢复。stream 类即由此定形：不做 Redis Streams 全仿真，语义模型为 RocketMQ 主题/队列；`ds/codec.rs` 的 `KIND_STREAM_PEND 0x0F` 是为将来完整 PEL 预留的 kind，Lite 实现不落盘该 kind（组已提交水位落在 kind-0x0E 记录）。
- `rcache/`：openraft 控制面。
  - `mod.rs`：TypeConfig、`raft_config()`（heartbeat 500ms、election 1000–2000ms、快照策略 LogsSinceLast(8192)）、`new_raft_node` 与 RAFT_BOOTSTRAP 初始化；
  - `store.rs` + `store_snapshot.rs`：RocksDB 日志/快照存储，快照仅保留 1 份；
  - `fsm.rs`：`KvMap` 状态机，快照为全量 JSON，Restore 为合并语义（对照 Go `cacheManager.UnMarshal`）；
  - `transport.rs`：RPC 客户端，u32 大端长度前缀 + JSON 帧，NodeId 由地址 md5 确定性派生（u64）；
  - `service.rs`：RPC 服务端，监听 `raft_bind_address`；
  - `http.rs`：`/get`、`/join`、`/depart` 控制 API（保持 Go 的 token 不强校验等既有行为）；
  - `join.rs`：join 客户端（应答必须为 `ok`）；
  - `ha.rs`：`backup_target_map` 灌入、5s leader 探测（TCP connect），`handler_observer` 执行 src/target 切换。
- `store/`：RocksDB 封装（`rocksdb.rs`），物理 key = `<slot>/` + key（`slot_prefix`），全部同步写；库路径 `store_path/bind`（`data_path`）。
- `ds/`：数据结构基座（七类结构共用，纯函数式）。
  - `codec.rs`：typed 物理编码——`<slot>/<kind:u8><u32 BE key_len><key><suffix>`；kind 0x00 为 raw string（无信封，零开销）；其余 value = LEB128 `expire_ms` 信封 + payload；`0xFD` 过期索引键；`family_delete_ranges` 整键族删除（测试外置 `codec_tests.rs`）；
  - `expire.rs`：全类型统一 TTL——读路径惰性判定 + `spawn_active_expire` 后台采样清理（`main.rs` 装配）；
  - `latch.rs`：用户键分片读写锁（读改写串行化）；`wait.rs`：阻塞命令 WaitHub（BLPOP 族备用）；
  - `hash_ds.rs`/`set_ds.rs`/`setops.rs`：Hash/Set 的派生键读写与集合代数。
- `router.rs`：MOVED 路由纯函数（`slot <= (index+1)*per_node_slots`，保留 Go 越界本地兜底）、白名单判断。
- `hash.rs`：CRC-16/XMODEM（与 Go `crc16tab` 同表）与 hash tag 解析。
- `state.rs`：`Shared`/`RaftState` 共享状态、apply loop（`spawn_apply_loop`，5s apply 超时对齐 Go）、openraft metrics → leader 状态同步。
- `topology.rs`：raft 键 `cluster_slots_stable_instances` → `Topology`（cluster_ready / stable_addrs / per_node_slots，SLOT_NUMBER=16384）。
- `monitor.rs`：`rdb_command_latency` 直方图（labels type/mode/ack，LinearBuckets 对齐 Go）与 `raft_stats` gauge（label status），`/metrics` 端点；Lite 指标 `rdb_lite_messages{op=add|read|ack}`、`rdb_lite_streams{kind=live|reaped}`、`rdb_lite_offset_dirty`。
- `beacon.rs`：env 门控诊断心跳（`RDB_BEACON=1`，仅 main 二进制内使用）。
- `rtypes.rs`：共享 wire 类型 `RaftLogEntryData`（JSON 字段名 `Key`/`Value`，对齐 Go 无 tag 编码）。
- `utils.rs`：工具函数（`md5_with40`、`exists`）。

## 关键流程
进程装配（`do_main`，对照 Go `server.NewRDB`）：
1. 加载 yaml 配置 → 启动 monitor `/metrics` 端点。
2. join 决策先于建节点：数据目录（`store_path/bind/raft`）已存在则不 join，否则取 `RAFT_JOIN_ADDR`。
3. `rcache::new_raft_node`：建目录、打开 RocksDB 日志存储与 FSM；`RAFT_BOOTSTRAP=true` 且未初始化时单节点 initialize。
4. 可选 beacon → raft RPC 监听（`service::serve`）→ HTTP 控制 API 绑定并服务。
5. join 地址非空时 `join::join_cluster`（应答必须为 `ok`，否则退出）。
6. apply loop 与共享状态：`RaftState`（leader 状态/统计/FSM 实时读）→ 启动时立即读一次拓扑 → 3s 拓扑同步任务 → metrics 同步任务 → 5s `raft_stats` gauge 刷新。
7. HA 任务：`spawn_backup_map_init`（leader 将 yaml `backup_target_map` 灌入 raft 并置 init 标记）、`spawn_leader_probe`（每 5s TCP 探测 peers，驱动 `handler_observer` 做 src/target 互换）。
8. 可选备份监听（`backup_bind` 非空，mode=backup，独立 store 路径）→ 正常监听：`store::open` + `resp::serve`（不返回）。

## 测试
- 单元测试内联于各模块 `#[cfg(test)]`；集成测试位于 `rust/rdb/tests/`：
  - `resp_e2e.rs`：RESP 层 e2e（AUTH 门、字符串命令、MOVED、协议错误）；
  - `lite_e2e.rs` / `lite_streams_e2e.rs` / `lite_proc_e2e.rs`：Lite Mode e2e——父主题自动选队列、XPICK、XINFO、组生命周期与补读、重启自已提交水位恢复、空闲 TTL 整流回收、BLOCK 跨连接唤醒、指标暴露；XRANGE 边界/COUNT、XTRIM MAXLEN 裁剪、XDEL 命中与 missing；进程级 kill -9 重启恢复；公共工具 `tests/common/lite.rs`；
  - `raft_cluster_e2e.rs`：bootstrap + HTTP join/depart 的两节点集群；
  - `raft_transport.rs`：双节点复制，覆盖 install-snapshot 路径；
  - `ha_failover.rs`：`backup_target_map` 故障切换与恢复；
  - `ds_e2e.rs` / `expire_e2e.rs` / `hash_set_e2e.rs`：数据结构 e2e——信封 roundtrip、主动过期采样、EXPIRE 族/TTL 持久化、Hash/Set 全命令生命周期与 CROSSSLOT；`list_e2e.rs` / `zset_e2e.rs`：List/ZSet 全命令生命周期、LREM compaction、TTL 惰性清理、ZSCAN 游标、BLPOP/BZPOPMIN 跨连接唤醒与超时（含丢失唤醒回归用例）；
  - `lite_e2e.rs`：流命令 e2e；
  - `process_cluster_e2e.rs` / `process_failover_e2e.rs`：进程级 e2e——`CARGO_BIN_EXE_rdb` 拉起真实二进制 + 临时 yaml 组 3 节点集群，断言协议应答原文（`-ERR: NOAUTH`、`-MOVED <slot> <addr>`、kill -9 后新 leader 选主、RocksDB 重启回读），公共工具在 `tests/common/mod.rs`。
- 集成测试统一用 `tempfile` 临时目录与临时端口（端口 0），无固定端口依赖。
- 压测工具 `rust/bench`（bin `rdb-bench`，见 `rust/bench/src/`）：RESP 负载发生器，`--workload ping|set|get|mixed` × `--clients` × `--pipeline`，延迟按每批 RTT 采样（pipeline>1 时为批 RTT 非单命令 RTT）；自带单元测试。示例：`rdb-bench --addr 127.0.0.1:6379 --token <t> --workload mixed --clients 16 --pipeline 16`。

## 相关文档
- Go 对应模块：[server](../server/index.md)、[rcache](../rcache/index.md)、[command](../command/index.md)、[store](../store/index.md)
- 特性文档：[raft-ha](../../features/raft-ha/index.md)
- 兼容性说明：[rust/COMPAT.md](../../rust/COMPAT.md)
