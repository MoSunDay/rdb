Commit: d481b1d708c248f86be394189d01ca7305fc8528
# command

## 职责
- 维护命令注册表 `CommandHander` 与路由白名单 `Whitelist`。
- 实现各 Redis 命令的语义与 RESP 回复。

## 边界
- 负责：命令参数校验、语义处理、错误回复。
- 不负责：slot 计算与 MOVED 路由（server）、数据落盘（store）、Raft 日志写入（直接调用 rcache 的 `Raft.Apply`）。

## 关键设计
- `CommandHander`：ping / quit / get / set / del / mget / mset / config / cluster / raft / migrate。
- `Whitelist`（ping/quit/config/cluster/raft/migrate）：server 侧跳过 slot 路由的命令。
- 注意：`keys`（keys.go）与 `size`（othes.go）处理器存在但未注册到 `CommandHander`。
- string.go：get/set/del 单键、mget/mset 批量；参数数量错误返回 `ERR wrong number of arguments...`。
- othes.go：ping 返回 PONG；quit 回复后关闭连接；config 固定返回 `cluster-require-full-coverage no`（供 redis-cli 探测）。
- cluster.go：
  - `cluster init [instances]`：仅 Raft leader 可执行，raft apply 写 `cluster_slots_stable_instances`；
  - `cluster info/nodes/slots`：基于 `StableAddrs` 生成节点视图，uuid 为 `MD5With40(addr)`（40 字符）；
  - `getNodeSlots`：16384 个 slot 按节点均分，末节点收余数；
  - `cluster test`：写死 `MOVED 5465 127.0.0.1:32681`（调试用）；
  - `ClusterReady=false` 时除 init 外全部拒绝。
- raft.go：`raft stats/leader/nodes` 透出 Raft 状态；`raft set/get` 直接读写 CM（不落 pebble）。
- migrate.go：`migrate task/list/help`，任务登记到 raft CM（见 [数据迁移](../../features/migrate/index.md)）。

## 核心链路
1. server 路由后调用 `fn(CommandContext{Conn, DB, PrefixKey, Args})`
2. 处理器读写 pebble（带 `{slot}/` 前缀）或 raft CM，并写 RESP 回复

## 依赖与接口
- 依赖：store（Pebble）、rcache（经 `conf.Content.CRaft` 访问 CM/Raft）、conf、utils。
- 对外：`CommandHander` / `Whitelist` 供 server 使用。

## 关联模块
- [server](../server/index.md)
- [store](../store/index.md)
- [rcache](../rcache/index.md)
