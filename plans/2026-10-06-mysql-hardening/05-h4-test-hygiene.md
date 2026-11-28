# H4 流程与测试卫生

> 状态：proposed
> 依赖：无

## 工作包

1. tests/common/mysql.rs 固定测试口令随机化（密钥规则边缘）
2. /tmp/rdb-sql-world-* 清理守卫
3. 负载 flake 缓解：重进程套件错峰/分类运行器（kafka/2pc 已知隔离全绿）
   - 多节点 join head timeout（`join raft cluster failed` 偶发 exit(1)）：harness 侧重生已由
     `plans/2026-11-27-lite-stream-keyspace-closure/` WP4 落地（`fd6bd2a`，`tests/common/spawn.rs`：
     子进程早退且 stderr 含 join 失败文本即重生 3 次、300ms 退避、换端口），src 侧 `HEAD_TIMEOUT`/
     `JOIN_TIMEOUT` 保持生产缺省；`spawn_node_sql/_mysql/_es/_backup` 尚未纳入，见
     `features/changelog/2026-11-27/batch14-defect-closure.md` 登记。
   - **服务端待修**（同 changelog 登记 1，手工探针可复现）：`/join` 在 leader 侧卡满 `MEMBERSHIP_TIMEOUT=30s`
     即毒化 leader——openraft 0.9.25 `add_learner(blocking=true)` 以 `replication_lag_threshold`（5000 条）
     判"追上"，死/慢 peer 被提升为 voter 后该成员日志永不提交，此后 `/join` 一律 InProgress internal error、
     客户端写全部 `apply timeout after 5s`；harness 重生救不了这条路径。候选：显式等 learner
     `matching ≥ membership_log_id` 再提升 voter，超时 `remove_learner` 回滚；或对齐两处超时。
   - 同节点端口互异（`dcde3a7`，`common::free_ports::<N>()`）：`free_addr()` bind :0 → drop 连续两次会发回
     同一临时端口，`failed_first_join_is_retried_on_restart` 的同配置重生曾撞自己的 listen。
   - 进程级端口预留（`4fc6c38` + `a1a7bdd`/`c89d4f0`，closure 门禁 follow-up）：本机 bind(0) 实测只发
     32769–46883 的奇数口（≈7058 个），一次全量跑数千次探测必然重发——`free_addr()` 对发过的口重抽，
     standalone 派生块与各私有探测（kafka_front/s3/rocksmq）接入同一集合；es/standalone 首个 bind 与同配置
     respawn 三类 EADDRINUSE 关闭，跨进程残余保留。
4. 剩余 9 个集群套件迁 common::mysql
5. ODKU/INSERT..SELECT 性能项：勘误——H0 落地的是 **BTreeMap 活 overlay**（每条已决策写
   维护、O(log n) 直接查找，消除 O(rows²) 线性探测），"overlay 哈希化已随 H0 落地"为
   误记：哈希化并未落地，仍是本包的后续可选优化；INSERT..SELECT 深拷贝改 into_source
   取走（条目不变）
## 验收

单测 + e2e 每项至少一条；收尾同步 COMPAT.sql.md / changelog / e2e-coverage；
全量 workspace 复跑无回归。
