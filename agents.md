Commit: d481b1d708c248f86be394189d01ca7305fc8528
# rdb Overview

## Overview
- rdb 是支持 Redis Cluster 协议的可持久化 KV 存储。**Go 原实现已归档至 `archive/go/`（git mv 保留历史，不再维护）；Rust 实现（仓库根目录）为唯一在维护实现**，见 [agents/rust/index.md](./agents/rust/index.md)。
- 以下描述对应归档的 Go 实现（`archive/go/` 下，路径省略该前缀）：
- 数据面：`redcon` 解析 RESP 协议，`internal/command` 分发命令，`internal/store` 基于 pebble 持久化。
- 控制面：`internal/rcache` 基于 hashicorp/raft 复制集群元数据（实例列表、备份映射、迁移任务）。
- 集群路由：key 计算 CRC16 slot，物理存储带 `{slot}/` 前缀；跨节点请求返回 `MOVED` 重定向。
- 高可用：Raft 心跳观察 + `backup_target_map` 实现故障节点与备份实例的槽位切换；另起只读备份实例（`backup_bind`）。
- 可观测性：Prometheus metrics（`rdb_command_latency` 直方图、`raft_stats` gauge）。
- Rust 实现的架构/行为差异见 `COMPAT.md`。

## Agent 模块索引
- [rust](./agents/rust/index.md) — 当前实现（RESP + openraft + RocksDB，cargo workspace）
- [server](./agents/server/index.md) — 【已归档】Go：进程装配、RESP 接入、slot 路由、HA 观察者
- [rcache](./agents/rcache/index.md) — 【已归档】Go：Raft 控制面：FSM、快照、HTTP join/depart/get
- [command](./agents/command/index.md) — 【已归档】Go：命令注册表与处理器
- [store](./agents/store/index.md) — 【已归档】Go：pebble 存储封装与 DB 接口

归档实现支撑包（`archive/go/` 下，规模较小，无独立文档）：
- `internal/conf`：yaml 全局配置单例 `Content`，`Sentinel.RTime` 为 5ms 递增的粗粒度时钟
- `internal/rtypes`：`CommandContext`、`RaftLogEntryData` 等共享类型
- `internal/monitor`：Prometheus collector
- `internal/utils`：CRC16 slot/hash、分段锁、bitmap（迁移预留）等工具
- `cmd/rdb`：main 入口；`examples/`：短链接示例。仍在仓库根目录：`scrtips/`（二进制部署与压测脚本）、`config/`（多实例 yaml，Rust 复用）

## Features 索引
- [features/index.md](./features/index.md)
