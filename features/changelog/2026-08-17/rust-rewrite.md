Commit: d481b1d708c248f86be394189d01ca7305fc8528
# Rust 重写（rdb）

## 背景 / Context
Go rdb（Redis Cluster 协议可持久化 KV 存储）整体重写为 Rust 实现，代码位于 `rust/`。
数据面 RESP 协议与 Raft HTTP API 与 Go 版保持字节兼容，可作为对等替换部署。

## 变更摘要 / Change Summary
重写覆盖全部数据面与控制面：
- RESP 数据面：`rust/rdb/src/resp/` 协议解析、`rust/rdb/src/command/` 命令分发，
  slot 路由由 `rust/rdb/src/router.rs` + `rust/rdb/src/hash.rs`（CRC16）承担，
  跨节点请求返回 MOVED 重定向。
- Raft 控制面：从 hashicorp/raft 换为 openraft 0.9.25（`rust/rdb/src/rcache/`），
  保留实例列表、备份映射等元数据复制与 HTTP join/depart/get 接口。
- 存储层：从 pebble 换为 RocksDB（`rust/rdb/src/store/`）。
- 可观测性：Prometheus monitor 保留（`rust/rdb/src/monitor.rs`），
  指标名与文本格式与 Go collector 一致。
- 其余 lib 模块：`conf`、`state`、`topology`、`utils`、`rtypes`；进程入口 `rust/rdb/src/main.rs`。
- 集成测试：`rust/rdb/tests/` 下 `resp_e2e.rs`、`raft_cluster_e2e.rs`、
  `raft_transport.rs`、`ha_failover.rs`。

## tokio LIFO slot 冻结与构建要求
根因：tokio multi_thread 调度器默认的 LIFO slot 在 openraft 工作负载下会丢唤醒，
导致约 6s（及其整数倍）的冻结（tokio-rs/tokio#4941 家族问题）；在完全空闲的
3 节点集群上亦可复现（follower 同样冻结），属 openraft + LIFO slot 调度的固有交互。
修复为调用 `Builder::disable_lifo_slot()`，因此构建必须带 `--cfg tokio_unstable`：
- 仓库内 `rust/.cargo/config.toml` 已设置 `rustflags = ["--cfg", "tokio_unstable"]`；
- 在 `rust/` 目录之外构建时需显式 `RUSTFLAGS='--cfg tokio_unstable'`，
  否则代码回退到带 LIFO slot 的 multi_thread 运行时（冻结复现）。
- 逃生开关：`RDB_CURRENT_THREAD=1` 切换 current_thread 运行时（无冻结但单线程）；
  `RDB_WORKER_THREADS=N` 调整 worker 池大小（默认与 Go 的 NumCPU 对齐）。
详见 `rust/COMPAT.md`。

## 诊断输出门控
beacon 心跳（`rust/rdb/src/beacon.rs`）默认静默；仅当 `RDB_BEACON=1` 时
以 250ms 周期输出 `[beacon]` 诊断行，用于冻结/慢响应排查。

## 验证
本次写入时实跑 `cargo test --workspace`，结果如下：
```text
test result: ok. 116 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.51s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.24s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.22s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
共 123 passed / 0 failed（单元 116 + 集成 7，doc-tests 0）。
- clippy `--workspace --all-targets -- -D warnings` 零警告。
- 3 节点实集群 drill 冒烟全 PASS：集群初始化、MOVED 重定向、raft set/get 跨节点复制、
  NOAUTH 门控等。
- HA drill（COMPAT.md 证据）：kill -9 leader 后约 6s 选出新 leader，写入正常；
  重启节点重新加入并以 follower 追平日志。

## 兼容性影响 / Impact Surface
- Raft TCP 线协议不同：openraft JSON framing vs hashicorp msgpack，
  Go 与 Rust 节点不能组成同一 raft 集群，需整集群切换。
- RESP 数据面与 Raft HTTP API 字节兼容，客户端无感。
- COMPAT.md 中列有“有意修复的 Go bug（设计上字节不兼容）”一节
  （如 MSET 奇数参数、DEL 返回值等），行为与 Go 版有意不同，详见 COMPAT.md。

## 相关文档 / Related Docs
- [rust/COMPAT.md](../../../rust/COMPAT.md)
- [agents/rust](../../../agents/rust/index.md)
- [features/index.md](../../index.md)
