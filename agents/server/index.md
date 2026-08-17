Commit: d481b1d708c248f86be394189d01ca7305fc8528
# server

## 职责
- 进程装配：初始化 Raft 控制面（`newRCache`）与 pebble 数据面（`newDB`），组装 `RDB`。
- RESP 协议接入：基于 redcon 提供 Redis 兼容 TCP 服务，服务密码为配置 `raft_token`（`redcon.NewServer` 第二参数，客户端需 AUTH）。
- slot 路由：对非白名单命令计算 slot，跨节点请求返回 `MOVED` 重定向。
- HA 观察：消费 Raft 心跳观察与 leader 探测结果，驱动集群实例列表切换。
- 备份实例：`backup_bind` 上启动独立服务（mode=backup），用于故障切换后的数据承接。

## 边界
- 负责：协议接入、认证、slot 路由、延迟埋点、HA 切换编排、备份实例装配。
- 不负责：命令语义（command）、Raft 状态机（rcache）、数据落盘（store）。

## 关键设计
- `newDB(bind, storePath, mode)`：`store.OpenPebble(storePath/bind)` + redcon server；mode 作为延迟指标 label（normal/backup）。
- 请求处理：`recover` 兜底写 `fatal error`；白名单命令（ping/quit/config/cluster/raft/migrate）跳过 slot 路由；其余命令：
  1. 支持 hash tag：取 `{...}` 内片段计算 slot，否则整个 key；
  2. `slot = CRC16(key) % 16384`，`prefixKey = "<slot>/"`；
  3. 按 `StableAddrs` 顺序检查 `slot <= (index+1)*perNodeslots`，不属于本节点则写 `MOVED {slot} {addr}` 并返回。
- 延迟埋点：以 `Sentinel.RTime`（5ms 递增的粗粒度时钟）计算耗时，写入 `rdb_command_latency{type, mode, ack}`。
- HA 切换（`handlerObserver`）：
  - `backup_target_map_{raftAddr}` = `src,target`，leader 启动时将 yaml 配置灌入 raft，完成后置 `backup_target_map_init=done`；
  - `FailedHeartbeatObservation`：`cluster_slots_stable_instances` 中 src 替换为 target 并 raft apply；
  - `ResumedHeartbeatObservation`：leader 每 5s `VerifyLeader` 后向 peers 发 `AppendEntriesRequest` 探测，成功时 target 换回 src。
- 3s ticker：把 raft 中的 `cluster_slots_stable_instances` 同步到 `conf.Content`（`ClusterReady`、`StableAddrs`、`PerNodeslots`）。
- 启动约定：`RAFT_BOOTSTRAP=true` 时 bootstrap 单节点集群；DataDir 不存在时以 `RAFT_JOIN_ADDR` 加入既有集群。

## 核心链路
1. main 启动 → conf 加载 → monitor 启动 → `NewRDB()`
2. `newRCache()`：Raft 节点/快照/日志 → join（按需）→ HTTP server → HA 观察 goroutine
3. `newDB()`：pebble + redcon 监听
4. 客户端命令 → AUTH → slot 路由 → `command.CommandHander[cmd]` 执行 → 延迟埋点

## 依赖与接口
- 依赖：command（处理器）、store（pebble）、rcache（Raft）、conf（全局配置）、monitor（埋点）、rtypes（CommandContext）。
- 对外：RESP TCP 服务（`Bind` / `BackupBind`）。

## 关联模块
- [rcache](../rcache/index.md)
- [command](../command/index.md)
- [store](../store/index.md)
