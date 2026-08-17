Commit: d481b1d708c248f86be394189d01ca7305fc8528
# Rust 进程级 e2e 与压测工具（rdb-bench）

## 背景 / Context
Rust 重写（见同日 [rust-rewrite.md](./rust-rewrite.md)）的测试停留在库层 e2e，
此前零覆盖 `main.rs` 进程装配面（config 加载、组网、RESP 监听、join）；
且仓库缺少可复现的负载发生器，性能对比只能靠外部脚本。

## 变更摘要 / Change Summary
全部为新增文件，生产代码零改动：
- `rust/rdb/tests/common/mod.rs`：进程级测试公共工具——拉起真实 `rdb` 二进制
  （`CARGO_BIN_EXE_rdb`）、临时 yaml 配置、临时端口分配、leader/就绪轮询、
  RESP 裸socket 读写、kill -9 与同配置重启。
- `rust/rdb/tests/process_cluster_e2e.rs`：3 真实进程组网 drill——NOAUTH 原文、
  AUTH、`CLUSTER INIT`、MOVED 重定向（slot 十进制 + addr 属于 bind 集合）、
  hash-tag、raft 复制跨节点可见、未知命令错误。
- `rust/rdb/tests/process_failover_e2e.rs`：kill -9 leader 后新 leader 选主、
  follower 追平、全停重启后 RocksDB 持久化回读。
- `rust/bench`：新增 workspace 成员（bin `rdb-bench`）——RESP 负载发生器，
  `--workload ping|set|get|mixed` × `--clients` × `--pipeline` × `--duration`，
  延迟按每批 RTT 采样；exit code 约定：参数错 2、服务端错误应答 1。
- `rust/Cargo.toml`：members 增加 `bench`。

## 验证 / Verification
当次实跑（数字为实跑输出，非目标值）：
- `cargo test --workspace`：137 passed / 0 failed / 0 ignored
  （lib 116 + main 2 + ha_failover 1 + process_cluster_e2e 1 +
  process_failover_e2e 2 + raft_cluster_e2e 1 + raft_transport 1 +
  resp_e2e 2 + rdb-bench 11 + doctest 0）。
- `cargo clippy --workspace --all-targets -- -D warnings` 零警告；
  `cargo build --workspace` 干净；`cargo fmt --all --check` 通过。
- rdb-bench release 冒烟（单节点 bootstrap、loopback、16 clients、10s、
  fake token）：ping 499.9k ops/s（p99 0.067ms）、get 481.7k ops/s、
  set 4.55k ops/s（p99 6.0ms，同步 RocksDB 写为主）、mixed 10.75k ops/s；
  mixed+pipeline16 12.6k ops/s（批 RTT p99 31ms）。debug 构建 ping 约 82k ops/s。

## Go A/B 对比：当次实跑结果
方法：同机同参（单节点 bootstrap、loopback、rdb-bench 唯一负载源、16 clients、
15s × 3 取中位、Go 默认优化构建 vs Rust `--release`、各自全新 store 目录、
两侧顺序执行不并发）。阈值：ops/s 中位 ≥ Go×0.97、p99 中位 ≤ Go×1.10。

| case | Go ops/s | Rust ops/s | ops 比 | Go p99 ms | Rust p99 ms | ops gate | p99 gate |
|---|---|---|---|---|---|---|---|
| ping p1 | 45054.0* | 493123.5 | 10.9×* | 4.235* | 0.070 | PASS | PASS |
| set p1 | 7375.5 | 4807.1 | 0.652 | 6.069 | 4.377 | **FAIL** | PASS |
| get p1 | 322537.5 | 487246.8 | 1.511 | 0.228 | 0.064 | PASS | PASS |
| mixed p1 | 14247.6 | 10281.7 | 0.722 | 2.505 | 10.046 | **FAIL** | **FAIL** |
| mixed p16 | 13093.2 | 12887.1 | 0.984 | 122.877 | 33.062 | PASS | PASS |

结论：**读路径显著领先（get 1.51×；ping 即使按 Go 最佳单轮 356k 对比也为 1.38×），
写路径未达标**——set 仅 Go 的 0.65×，mixed(p1) 0.72× 且 p99 为 Go 的 4.0×。
同步写路径为 RocksDB（Rust）vs pebble（Go），后续方向：RocksDB 写路径调参
（write_buffer / WAL / 批量合并）并排查 mixed p1 尾延迟毛刺（max 100ms+，
疑似 compaction/write-stall）。

*基线方差注记：Go 版存在周期性停顿，与 hashicorp/raft 约 40s 一次的快照尝试
（"failed to take snapshot: nothing new to snapshot"）时间点吻合——ping RUN2/3
从 356k 掉至 44k（p99 4.2ms），mixed+pipe16 RUN3 掉至 6.1k（p99 266ms）。
故 Go ping/mixed16 的中位数被拖低；ping 行已附按 Go 最佳单轮的保守对比。
环境解锁记录：`proxy.golang.org` 经 socks5 代理（socks5://127.0.0.1:1080）
可达后 `go mod download` + `go build ./cmd/rdb` 成功，A/B 于本轮完成。

## 安全 / Secrets
所有测试与压测 token 均为伪造（`e2e-fake-token-…`、`bench-fake-token-…`），
真实 token 未进入任何源码或文档。

## 相关文档 / Related Docs
- [agents/rust](../../../agents/rust/index.md)
- [rust-rewrite.md](./rust-rewrite.md)
