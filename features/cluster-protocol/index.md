Commit: d481b1d708c248f86be394189d01ca7305fc8528
# Redis Cluster 协议兼容

## 能力概述
- 对外表现为 Redis Cluster：`redis-benchmark --cluster`、`redis-py-cluster` 可直连使用。
- 客户端通过 `CLUSTER` 子命令获取节点/slot 视图；跨节点 key 请求收到 `MOVED` 重定向。

## 触发方式
- 任意 Redis 客户端命令；`cluster init` 初始化集群实例列表。

## 行为与规则
- slot 计算：CRC16(key) % 16384；支持 `{hash tag}`，取花括号内片段计算。
- 初始化：`cluster init [instances]` 仅 Raft leader 可执行；实例列表经 raft 复制，由 server 3s 同步到 `ClusterReady` / `StableAddrs` / `PerNodeslots`。
- 路由：slot 按节点均分（`16384/len(addrs)`），末节点收余数；非本节点请求返回 `MOVED {slot} {addr}`。
- 视图命令：`cluster info`（cluster_state、epoch=term+commit_index）、`cluster nodes`（uuid 为 `MD5With40(addr)`）、`cluster slots`。
- `config` 命令固定回复 `cluster-require-full-coverage no`。

## 关键状态与异常
- 状态：`ClusterReady=false` 时除 `cluster init` 外所有 cluster 子命令被拒绝（`cluster not ready...`）。
- 异常：未知命令返回 `ERR unknown command`；参数错误返回对应 `ERR`。
- 限制：无 `cluster meet/forget` 等动态运维命令；`cluster test` 为写死 MOVED 的调试命令。

## 关联逻辑模块
- [server](../../agents/server/index.md)
- [command](../../agents/command/index.md)
