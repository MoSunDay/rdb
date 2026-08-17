Commit: d481b1d708c248f86be394189d01ca7305fc8528
# rcache

## 职责
- 基于 hashicorp/raft 的集群控制面：节点成员管理、元数据复制、快照与恢复。
- 元数据存储 `cacheManager`：concurrent-map 实现的 string→string KV（`CM`）。
- 对外 HTTP 接口：节点 join/depart、元数据读取。

## 边界
- 负责：Raft 节点生命周期、FSM、快照、HTTP 控制接口、心跳观察。
- 不负责：业务数据（走 store）、命令语义（command）、HA 切换编排（server 消费观察事件）。

## 关键设计
- `Cached` 聚合 `Opts`（DataDir/HttpAddress/RaftTCPAddress/JoinAddress/RaftToken）、`CM`、`Raft`（RaftNodeInfo）。
- `NewRaftNode`：
  - LocalID = `RaftTCPAddress`；TCP transport；
  - log/stable 存储用 boltdb（`raft-log.bolt`、`raft-stable.bolt`），快照为文件快照（DataDir，保留 1 份）；
  - `SnapshotInterval=30s`、`SnapshotThreshold=1`；
  - `RAFT_BOOTSTRAP=true` 时 `BootstrapCluster` 单节点；
  - 观察者仅过滤 `FailedHeartbeatObservation` 事件。
- FSM：`Apply` 反序列化 `RaftLogEntryData{Key,Value}` 写入 `CM`；`Snapshot` 全量 JSON 序列化 CM；`Restore` 反序列化覆盖。
- HTTP（`NewHttpServer`）：`/get?key=&raft-token=`、`/join?peerAddress=&raft-token=`（AddVoter）、`/depart?peerAddress=&raft-token=`（RemoveServer）；`/set` 被注释禁用；`EnableWrite` 标志随 leader 状态切换，当前无实际作用。
- `JoinRaftCluster`：向 `JoinAddress` 的 `/join` 发起加入请求。
- 关键元数据键（均经 raft 复制）：`cluster_slots_stable_instances`、`backup_target_map_{raftAddr}`、`backup_target_map_init`、`migrate_task`。

## 核心链路
1. server.newRCache 构造 Cached → NewRaftNode（bootstrap 或 join）→ HTTP server 监听
2. 命令通过 `Raft.Apply` 写入 → FSM.Apply → CM
3. 心跳失败 → ObserverChan → server 侧 HA 处理

## 依赖与接口
- 依赖：rtypes（RaftLogEntryData）、utils；外部：hashicorp/raft、raft-boltdb、concurrent-map。
- 对外：`HttpAddress` HTTP 接口；Raft TCP 端口 `RaftTCPAddress`。

## 关联模块
- [server](../server/index.md)
- [command](../command/index.md)
