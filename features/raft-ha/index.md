Commit: d481b1d708c248f86be394189d01ca7305fc8528
# Raft 控制面与高可用

## 能力概述
- 集群元数据（实例列表、备份映射、迁移任务）由 Raft 复制，多节点一致。
- 节点加入/退出：HTTP `/join`、`/depart`（需 `raft-token`）。
- 简单 HA：节点心跳失败后，其槽位切换到 `backup_target_map` 指定的备份实例；节点恢复后切回。

## 触发方式
- 运维：首节点设 `RAFT_BOOTSTRAP=true` 启动；新节点在 DataDir 不存在时设 `RAFT_JOIN_ADDR` 自动 join。
- 故障：Raft `FailedHeartbeatObservation` 事件驱动故障切换；leader 每 5s 向 peers 发 `AppendEntriesRequest` 探测驱动恢复切换。
- 命令：`raft stats/leader/nodes/set/get`。

## 行为与规则
- `backup_target_map`（yaml 配置 `{raftAddr: {src, target}}`）由 leader 启动时写入 raft（`backup_target_map_{raftAddr}` = `src,target`，完成后置 `backup_target_map_init=done`）。
- 故障切换：`cluster_slots_stable_instances` 中 src 替换为 target；恢复切换反向替换。
- 备份实例：`backup_bind` 上运行独立服务（mode=backup）；其数据依赖外部同步，当前未见自动同步逻辑（迁移能力未完成，见 [数据迁移](../migrate/index.md)）。
- 控制接口鉴权：HTTP 以 `raft-token` query 参数校验；Redis 命令走 redcon 服务密码（同为 `raft_token`）。
- `raft set/get` 直接读写控制面 KV（不落 pebble）。

## 关键状态与异常
- 状态：Raft 状态（Leader/Follower/Candidate/Shutdown/Unknown）由 `raft_stats` gauge 暴露（main 每 5s 刷新）。
- 异常：`/join`、`/depart` 失败返回 `internal error`；`cluster init` 非 leader 时返回 leader 地址提示。
- 限制：`EnableWrite` 写开关当前无实际作用（`/set` 未注册）；快照仅保留 1 份。

## 关联逻辑模块
- [rcache](../../agents/rcache/index.md)
- [server](../../agents/server/index.md)
