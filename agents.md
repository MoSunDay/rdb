Commit: d481b1d708c248f86be394189d01ca7305fc8528
# rdb Overview

## Overview
- rdb 是支持 Redis Cluster 协议的可持久化 KV 存储（Go 1.17）。
- 数据面：`redcon` 解析 RESP 协议，`internal/command` 分发命令，`internal/store` 基于 pebble 持久化。
- 控制面：`internal/rcache` 基于 hashicorp/raft 复制集群元数据（实例列表、备份映射、迁移任务）。
- 集群路由：key 计算 CRC16 slot，物理存储带 `{slot}/` 前缀；跨节点请求返回 `MOVED` 重定向。
- 高可用：Raft 心跳观察 + `backup_target_map` 实现故障节点与备份实例的槽位切换；另起只读备份实例（`backup_bind`）。
- 可观测性：Prometheus metrics（`rdb_command_latency` 直方图、`raft_stats` gauge）。
- Rust 重写：功能对齐实现位于 `rust/`（独立 cargo workspace，RESP + openraft + RocksDB），见 [agents/rust/index.md](./agents/rust/index.md)。

## Agent 模块索引
- [server](./agents/server/index.md) — 进程装配、RESP 接入、slot 路由、HA 观察者
- [rcache](./agents/rcache/index.md) — Raft 控制面：FSM、快照、HTTP join/depart/get
- [command](./agents/command/index.md) — 命令注册表与处理器
- [store](./agents/store/index.md) — pebble 存储封装与 DB 接口

支撑包（规模较小，无独立文档）：
- `internal/conf`：yaml 全局配置单例 `Content`，`Sentinel.RTime` 为 5ms 递增的粗粒度时钟
- `internal/rtypes`：`CommandContext`、`RaftLogEntryData` 等共享类型
- `internal/monitor`：Prometheus collector
- `internal/utils`：CRC16 slot/hash、分段锁、bitmap（迁移预留）等工具
- `cmd/rdb`：main 入口；`examples/`：短链接示例；`scrtips/`：部署与压测脚本；`config/`：多实例 yaml

## Features 索引
- [features/index.md](./features/index.md)
