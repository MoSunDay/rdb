Commit: (working-tree, 随本提交入库)

# 目录扁平化 + 线上稳定性扫除（P0/P1/P2）+ SIGTERM 优雅停机

## 背景
上线前第二轮扫除：工作区扁平化（`rust/rdb/` 层级移除）、已核实 P0/P1 缺陷全修、
低风险 P2 加固、SIGTERM 优雅停机（用户确认）与 e2e 缺口补齐。

## 目录扁平化（BREAKING，仅路径）
- `git mv rust/rdb/src -> rust/src`、`rust/rdb/tests -> rust/tests`；`rust/Cargo.toml`
  合并为 workspace 根包 `rdb` + `members = ["bench"]`；lock 无变化。

## P0
- `rust/src/utils.rs`：`glob_match` 递归改两指针+末星回溯迭代——O(1) 栈、
  O(|p|·|s|) 最坏，64KB key（KEYS `*zz`）与多 `*` 病态 pattern 不再爆栈/指数爆炸。
- `rust/src/lite/append.rs`：`xdel` 同命令重复 ID 去重（批量删除对物理读不可见导致
  XLEN 计数损坏）；`xtrim` 受害者扫描补 `starts_with(base)` 守卫 + 预分配上限
  （腐化 meta.len 不再越界删除同 slot 邻居流记录）。

## P1
- **阻塞命令专用 park 池**（新 `rust/src/park.rs`）：BLPOP/BZPOPMIN/XREAD BLOCK 的
  condvar park 不再占用 tokio 共享 blocking 池——512 个永久阻塞等待不再饿死
  RocksDB fsync 任务（此前可复现写路径全停）。600 并发 BLPOP 下 SET <1s 回归测试。
- `rust/src/resp/`：单 bulk 512MB 上限 + 每连接 1GB 缓冲上限 + 未认证 30s 读截止 +
  panic 后关连接 + AUTH 常数时间比较；accept 错误退避。
- `rust/src/ds/expire.rs`：lazy/主动过期清除写路径移出 tokio worker（detached
  spawn_batch_write / 采样轮次 spawn_blocking）；`store/ops.rs` 迭代错误如实上抛。
- `rust/src/rcache/store_snapshot.rs`：快照写改临时名+rename 原子替换——同 meta
  重投/中途崩溃不再截断 live 快照（崩溃环修复）。
- `rust/src/rcache/transport.rs`：拨号纳入 RPC_TIMEOUT（黑洞对端不再悬挂）；帧读
  分块增长 + 长度上限；raft 连接双向 TCP keepalive + 每帧空闲超时。
- `rust/src/lite/append.rs`：XIDLE/XADD 的 idle deadline 全链 checked 运算，溢出
  回错不落盘（此前 u64 回绕可致整流被错误回收）。
- `rust/src/lite/offset.rs`：组偏移缓存键改原始字节 `(Vec<u8>, Vec<u8>)`——
  lossy-UTF8 组名不再碰撞（NOGROUP 假阳性/幻影组/跨组 ack）。

## P2 加固
- apply 通道有界(1024)+try_send 快速报错；启动拒绝空 `raft_token`；
  backup_map 种子失败改重试（FSM 幂等覆盖）；join 判定改 `raft::is_initialized()`
  （目录/CURRENT 信号在首次 join 失败后误判，导致重试被跳过）；启动打印 LIFO-slot
  生效状态；`/metrics` 头读 5s 超时。

## SIGTERM 优雅停机
- `rust/src/main.rs`：SIGTERM/SIGINT → 日志 → 5s 限时 `lite::flush_offsets_once` →
  exit(0)；进程级 e2e 断言退出码与 offset 水位重启后不复投已 ack 条目。

## e2e 补齐
- 新增 `string_e2e.rs`（SET 全选项矩阵+MSET/MGET/CROSSSLOT）、`process_metrics_e2e.rs`
  （真实进程 scrape `rdb_command_latency`/`raft_stats`）、`lite_group_e2e.rs`
  （非 UTF8 组名隔离）、`process_sigterm_e2e.rs`。
- 扩展 list/zset/expire/process_failover（七家族 kill -9 存活）/process_cluster
  （/depart+重 join、MIGRATE 正/错路径）。
- 全量 26 个测试二进制 504 用例通过；`rdb-bench --workload mixed` 冒烟无冻结
  （227k ops/s，p99 1.9ms）。

## 明确不做（记录）
HA 探活升级为真实 RPC（保留 Go parity）、WaitHub 全面异步化、KEYS 大 slot
异步化、purge 确认-删除窗口 latch（del 竞态类，已接受）。
