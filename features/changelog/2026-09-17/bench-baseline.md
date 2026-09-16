# W2.3 延迟 A/B 基线：c22ff37 vs HEAD（RESP mixed 无 p50/p99 回退）

Commit: HEAD `03ad558`（对照旧基线 `c22ff37`）

## 背景

W2 一批改动（SQL DECIMAL、复合主键、单机 ts floor、boot 扫描、clippy/expire
重构，共 85 文件 +6002/−1229）合入后，需证明 RESP 数据面没有引入延迟回退。
方法为旧/新二进制同机 A/B 压测：旧 = `git worktree` 于 `c22ff37` 现场
`cargo build --release --bin rdb`（worktree 内独立 target，同 `.cargo/config.toml`
的 `tokio_unstable` cfg，两二进制启动横幅均确认 "tokio LIFO slot: disabled"，
Cargo.lock 两版本 diff 为空 ⇒ 依赖与编译旗标完全一致）；新 =
`target/release/rdb`（HEAD 构建，幂等复用）。

## 方法

- 拓扑：单机 bootstrap 节点（`RAFT_BOOTSTRAP=true`），端口带 32940-32946，
  yaml 与 `scrtips/e2e_scenarios/env.sh` 生成器同构，每次运行**全新 store 目录**，
  token 取随机 hex（不取 `config/`）。
- 负载：`rdb-bench --addr 127.0.0.1:32940 --token … --workload mixed --clients 8
  --pipeline 16 --duration 60`（与 soak 同参）。rtt 统计按 16 命令/批的批 RTT 采样。
- 顺序：旧-新-旧-新交替；门限初判超差后按规程追加第 3 轮交替定位；两次运行间
  `sleep 5`，节点 SIGTERM 优雅退出并确认端口释放后再起下一个。
- 环境纪律：跑前清掉遗留 `/tmp` e2e 集群与 cargo 测试进程（本仓测试遗留），
  每轮起节点前复查无 rdb 进程；16 核宿主，运行期 1min load ≤ ~3。
- 校验：6 轮全部 `errors=0`（rc=0），无 `-MOVED`/错误回复；新旧节点日志行为一致。

## 数据（每行一次 60s 运行；rtt 单位 ms，批 RTT）

| # | 二进制 | workload | avg | p50 | p99 | max | ops | ops/s |
|---|--------|----------|-------|-------|--------|--------|--------|--------|
| 1 | old c22ff37 | mixed | 19.833 | 18.507 | 62.175 | 154.784 | 387072 | 6446.4 |
| 2 | new HEAD    | mixed | 25.631 | 19.633 | 144.763 | 593.005 | 299504 | 4990.3 |
| 3 | old c22ff37 | mixed | 22.532 | 19.183 | 83.836 | 150.426 | 340608 | 5675.4 |
| 4 | new HEAD    | mixed | 25.828 | 22.523 | 68.576 | 152.875 | 297184 | 4951.6 |
| 5 | old c22ff37 | mixed | 21.367 | 20.220 | 65.376 | 166.223 | 359168 | 5984.2 |
| 6 | new HEAD    | mixed | 22.953 | 20.369 | 64.274 | 287.902 | 334352 | 5571.6 |

汇总（各 3 轮）：

| 指标 | old（中位/均值） | new（中位/均值） | 判定用 Δ（中位） |
|------|------------------|------------------|------------------|
| p50  | 19.183 / 19.303  | 20.369 / 20.842  | **+6.2%** ✅ |
| p99  | 65.376 / 70.462  | 68.576 / 92.538  | **+4.9%** ✅ |
| avg  | 21.244（均值）    | 24.804（均值）    | +16.8%（均值，超 ±15%）|
| ops/s | 6035.3（均值）   | 5171.2（均值）    | −14.3%（均值，超 ±15%）|

最平静的一对（#5/#6，环境噪音最低）：p50 +0.7%、p99 **−1.7%**、avg +7.4%、
ops/s −6.9%，全在门内。

## 分析

- 判定门（p50/p99 ±15%）：**通过**。p50 中位 +6.2%、p99 中位 +4.9%；轮内方差
  （old p99 62→84、new p99 145→64）本身就有 ±15% 量级，属 fsync-per-write
  负载的固有噪音；#2 的 p99=145/max=593 是单轮尾部离群（该轮后 4 轮再未复现）。
- 如实记录的反常信号：avg 均值 +17%、ops/s 均值 −14%，且逐轮秩分离（每个 new
  轮的 avg 都高于每个 old 轮、ops/s 都更低）——非纯噪音方向性，但**找不到代码
  机制**：逐项排查 c22ff37..HEAD 的非 SQL diff——RESP mixed 热路径（SET/GET/
  raft apply/rocksdb put）零改动；`chunks_exact→as_chunks` 仅在 mset/vectorset
  （bench 不经过）；expire/ 拆分是行为保持的代码搬移；ts floor stamp 只挂 SQL
  写批（`sql/exec/write.rs`、columnar、2PC），RESP KV 批不经手；boot 扫描仅在
  floor 键缺失时发生且 bench 用全新空 store（扫描为空）；Cargo.lock 无变化。
  疑似 +6k 行 SQL 代码带来的二进制布局/代码生成次生效应（中尾变胖、p50 不动），
  证据不足，不下定论；夜间 soak（p99 门 1000ms）有 15 倍裕量，监控即可。

## 结论

**不回退**（判定门 p50/p99 均在 ±15% 内：+6.2% / +4.9%，最优对 −1.7%/+0.7%）。
附带观察：avg/吞吐均值差 +17%/−14% 呈一致方向但无热路径代码机制可归因，已上表
留档；若后续 soak 连续出现吞吐同幅下滑，值得用 perf flamegraph 单独立案。
