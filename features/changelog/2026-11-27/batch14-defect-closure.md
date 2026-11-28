# 批次 14 五项缺陷闭环：lite 流名键空间可见性 + turso IS NULL 渲染 + 两处夹具 flake + MySQL 前端 Nagle 停顿

Commit：d0f5992（缺陷 1）、24d801b（缺陷 2）、edb3a5d（缺陷 3）、fd6bd2a（缺陷 4）、8d4c0c7（缺陷 5）；
门禁 follow-up：c78feea（缺陷 1 follow-up）、b37de48（缺陷 3 follow-up）、314c407（缺陷 5 follow-up）、
dcde3a7 / 4fc6c38 / a1a7bdd / c89d4f0（缺陷 4 follow-up：同节点端口互异 → 进程级端口预留 → 私有探测接入）、
7b4ea50 / 9957e8d（缺陷 3 同族：scenario 三处负载时序窗）；本地提交，未推送。
背景：批次 14（#161–#175，`923ec7a`，同目录 `multistore-scenarios-batch14.md` §真实行为偏差）落地过程
暴露 5 项问题——1 项功能缺陷根因、1 项 SQL 渲染缺陷、2 项测试夹具 flake、1 项性能疑点——按
`plans/2026-11-27-lite-stream-keyspace-closure/`（landed）逐项闭环，每项一个提交，不新增能力。

## 缺陷 1：lite 流名在键空间命令中不可见、WATCH 永不触发（d0f5992）
- 症状：对普通流名（`topic/q0`）WATCH 永不触发（#162 该腿被移除）；EXISTS/TYPE 看不见流；
  RENAME 一律 `-ERR no such key`（#163 只能靠同槽不动点名改名）。
- 根因：lite 以**父槽前缀 + 全名**存流（`src/lite/append.rs:53-57`、`entries.rs:89-95`、
  `model.rs stream_prefix`），X 族命令节点本地；键空间命令却拿 `slot(hash(全名))/` 走通用解析
  （`src/command/keys_core.rs`）。WATCH 脏检查在 `w.prefix` 下重算物理键哈希 → 永远比对不到；
  RENAME `require_same_slot` 按全名 → `Missing`。次生：rename/DEL 不触碰 `OffsetCache`。
- 修复（形态 A：命令层解析；否决 lite 改按子槽存储与 router 特判 X 族）：
  - `src/lite/model.rs` `stream_key_target(name) -> Option<Vec<u8>>`：合法 Stream
    （`parse_topic_name`）且 `stream_prefix` 为 Some 时给出父槽前缀窗口；
  - `src/command/keys_core.rs` `resolve_located`：先试路由槽（普通含斜杠键仍优先命中），再试父槽，
    返回命中的 `(Cow<[u8]>, KeyState)`，使 EXISTS/TYPE/DEL/EXPIRE/PERSIST 读写**同一窗口**；
    `lock_key_latches` 对两个窗口都上闩；
  - RENAME：`src/ds/setops.rs` `rename_slot` 把键对分为 `Ordinary | Stream(prefix) | CrossSlot | Mixed`
    ——同一父主题下两条流按父槽同槽校验后经 `keys_core::rename_stream` 搬迁（`stream_prefix(src)` →
    `stream_prefix(dst)`，移动期间同时持有两名的 `lite::stream_latch_key` 刷盘闩，使每个 offset
    刷盘轮次整体落在移动之前或之后）；不同父主题仍 CROSSSLOT；一流一普通键回 `RENAME_MIXED_ERROR`
    （"ERR RENAME source and destination must both be lite stream names or both be ordinary keys"）；
  - `src/lite/mod.rs` `Runtime::stream_renamed(old, new)`：`offset::rename_stream` 把缓存的组状态重键到
    新名并标脏（未刷盘的 ack 增量保留；租约持有者重置），`ordered::drop_stream` 丢弃两名——不再入
    reaps 队列；
  - DEL/EXPIRE：`stream_reaped` 改为收到父前缀，延迟的 `reap_stream` 扫描真正范围删除组窗口
    （核实结论：`family_delete_entries` 已覆盖流族 0x0C..=0x0F、OFFSET 族与延迟行，无需改）；
  - WATCH：`src/command/tx_cmd.rs` 同时记录路由槽与父槽两个窗口，脏检查沿用 `tx/watch.rs` 的逐窗口比对。
- 测试：新 `src/command/keys/tests_stream.rs`（220 行：exists/type/del、rename 搬迁 + 重键、窗口规则）；
  `keys/tests.rs` `rename_stream_moves_offset_ledger` 改走 `rename_stream`；`tx_cmd.rs` WATCH 流窗口；
  `offset.rs` `rename_stream`；`setops.rs` `rename_slot` 四分类。e2e 回填：#162
  `scenario_watch_json_crossfront_e2e` 以**普通流名**断言 WATCH 触发 → EXEC 返回 nil、EXISTS/TYPE 可见流；
  #163 `scenario_rename_typed_families_e2e` 增普通流名 RENAME 腿（条目迁移 + XLEN/XRANGE/XINFO 守恒 +
  旧名 EXISTS=0）。follow-up `c78feea`：`scenario_key_lifecycle_e2e`（#36）曾把该缺陷钉成
  "stream TYPE quirk"（`TYPE ktstream/q0` → none），改断 `stream`——WP1 后首次全量跑的唯一失败。
- 已知局限（入 `agents/rust/index.md`）：cluster 下 X 族节点本地，流名键空间命令仅在持有该流的节点
  生效；RENAME **不迁移**消费组水位/PEL（新名 read-through 为空）。

## 缺陷 2：turso 渲染器把 `x IS NULL` 写成 `(x IS) NULL`（24d801b）
- 症状：turso 库上 `IS NULL` / `IS NOT NULL` 一律 1064 `near ")": syntax error`（#171 turso 腿因此被摘）。
- 根因：`src/sql/turso/translate/expr.rs` `format!("({} IS{}) NULL", ..)` 括号归位错误。
- 修复：改为 `({} IS{} NULL)`；`#[cfg(test)]` 增 4 个渲染单测（列与嵌套表达式 × IS NULL / IS NOT NULL，
  180→287 行）。
- 测试：`tests/turso/turso_crud_e2e.rs` 增空值谓词块（IS NULL / IS NOT NULL / `= NULL` 空集 / `<=>` 仍钉
  1235，→320 行）；#171 `scenario_sql_conjunct_join_null_e2e` 恢复 turso IS NULL 腿，双前端同断言（→287 行）。
  台账：`COMPAT.sql.md` turso 段加已修注记。

## 缺陷 3：RocksMQ http e2e 私有夹具 `resp_cmd` 固定 300ms 读窗（edb3a5d）
- 症状：`rocksmq_http_e2e::produce_group_consume_ack_flow` 满载下偶发 panic `mid > len`
  （批次 14 门禁首轮，`tests/rocksmq/rocksmq_http_e2e.rs:171` `split_at(5)` 越界）。
- 根因：文件私有 `resp_cmd` 固定 300ms 排水后按固定偏移切片，满载下读不到 AUTH 回复即切片。
- 修复：删 `resp_cmd`，行回复（XACK `:1` / XADD bulk id）改 `common::cmd_one_shot`（10s `read_one`），
  数组回复（XRANGE / XPENDING）改 `common::lite::cmd_full_reply_wait`，断言保持字节级；
  `:1` 断言去掉尾部 CRLF；423→399 行。
- 验证：`--test rocksmq_http_e2e` 连跑 3 轮全绿。
- 残余（登记）：同文件私有夹具另有 EADDRINUSE 偶发（负载下约 5–15%），未在本批处理——私有探测随后由
  `a1a7bdd` 接入进程级端口预留（见缺陷 4），条目 4 关闭。
- 同族 follow-up `7b4ea50`（第 2 轮全量门禁，scenario 302s、共享主机负载 ≈19，两处时序窗第 1 轮未撞）：
  `scenario_checkout_watch_e2e` 的 `cmd_raw` 首字节同样只等 200ms 静默窗（`exec replies: ""`，WATCH 会话里
  AUTH +OK 与 EXEC 回包之间被负载拉开），改首字节 10s、之后 120ms 静默；`scenario_lite_gc_redelivery_e2e`
  空 PEL 消费者 GC 等待 3s → `GC_BUDGET = 10s`，并计入 `perf::within("gc127/consumer-gc", …)`。
  同族第二例 `9957e8d`（第 5 轮全量，scenario 369s）：`scenario_cashier_abort_discard_e2e` 的私有 `cmd_raw`
  同样首字节只等 120ms 静默窗（`:185` `commit after discard: []`，DISCARD 后的 MULTI/EXEC 会话），同样改首字节
  10s、之后 120ms 静默。残余：scenario 目标仍有约 20 个文件各带私有静默排水（`rf`/`cmd_raw`，首字节只等静默窗），
  负载下会逐个暴露（登记 2）。

## 缺陷 4：harness 对瞬时 join head timeout 不重生（fd6bd2a）
- 症状：负载下多节点套件偶发 `join raft cluster failed` → 节点 `exit(1)` → 用例失败
  （`sql_join_cluster_e2e`、`sql_2pc_e2e`、`scenario_sqlrpc_twopc_e2e`，批次 11/13/14 均有记录）。
- 根因：`/join` 首字节超时 `HEAD_TIMEOUT=10s` 硬编码（`src/rcache/http.rs`），失败即 `exit(1)` 无重试
  （`src/main.rs`）；harness 只对 EADDRINUSE 重生（`tests/common/mod.rs`）。
- 修复：`tests/common/mod.rs` 已满 800 行，把 `spawn_child`/`SPAWN_ATTEMPTS`/`spawn_node`/
  `spawn_node_yaml` 整体迁到新文件 `tests/common/spawn.rs`（200 行），`mod.rs` 回到 703 行；
  `spawn.rs` 内：子进程早退且 stderr 含 `join raft cluster failed:` → 视同 EADDRINUSE 路径重生
  （`JOIN_ATTEMPTS = 3`、300ms 退避、换新端口），`join_peer_alive` 门保证死 peer 不被重试，
  `JOIN_WINDOW = 20s`。**src 侧 `HEAD_TIMEOUT`/`JOIN_TIMEOUT` 不动**（生产缺省，H4-3 原则）。
- 验证：`--test cluster` 连跑 3 轮 25/25；全工作区 ×2（见门禁）。
- 残余（登记）：`spawn_node_sql/_mysql/_es/_backup` 仍直调 `spawn_child`，不在 join 重生覆盖内。
- follow-up `dcde3a7`（同节点端口互异）：3 轮并发 `--test cluster` 中 1 轮
  `process_cluster_e2e::failed_first_join_is_retried_on_restart` 的 node1 抽到 `raft=http=127.0.0.1:38475`
  （`free_addr()` bind :0 → drop，连续两次探测内核会发回同一临时端口）——首次启动死于**本意中**的死 peer
  join（EADDRINUSE 重生从未看到），用例自带的同配置重生再撞自己的 `listen`。新增 `common::free_ports::<N>()`
  （逐对互异，重复重抽）替换全部多端口抽取点：`spawn_node_yaml`、`spawn_node_mysql/_sql/_es/_backup`、
  `mysql::spawn_node_sql`、`mq::spawn_mq_node`、`scenario_common::spawn_front_node`（两处私有 `free_addr` 副本删除；
  `mod.rs` 703→696 行，`mq.rs` 302→290、`scenario_common/mod.rs` 414→401）。
- follow-up `4fc6c38`（进程级端口预留）：第 3 轮全量门禁 `es_bulk_interop_e2e::es_bulk_auto_id_items` **首个 bind**
  即 `listen 127.0.0.1:38215 failed`（探测→bind 窗口内端口被兄弟探测拿走，逐对互异管不到）；并发 cluster 轮
  `backup_failover` 的同配置 `respawn()` 也是这样死在 43787。实测本机（6.8 内核）bind(0) 的口池远小于
  `ip_local_port_range`：20000 次探测只落在 32769–46883 的 **奇数** 端口，6642 个不同值，即均匀于 ≈7058 个口
  （范围的 1/4），单口被重发的概率 ≈1/7000 而非 1/28000，一次全量跑数千次探测必然反复重发。`common::free_addr()`
  改为进程级集合（`OnceLock<Mutex<HashSet<u16>>>`）：发过的口不再发，内核重发即重抽；`free_ports::<N>()` =
  N × `free_addr()`；standalone 夹具 base+0..+10 的派生块经新 `common::reserve_port()` 入同一集合（顺序 cluster 轮
  `standalone_boots_turso_engine_with_flag` 首个 bind 同样死在 38985：块探测释放后、子进程 bind 前被兄弟探测拿走）。
  跨进程（并发跑、外部会话）仍是接受的残余。
- follow-up `a1a7bdd` + `c89d4f0`（私有探测接入预留）：`kafka_front_common::free_addr`、`s3/fixture.rs::free_addr`（s3 目标
  挂载 `common`）、`rocksmq_http_e2e` / `rocksmq_wait_pending_e2e` 的 5 口抽取、`kafka_wire_e2e` 的 0.0.0.0 探测
  （`a1a7bdd`）与 `scenario_redirect_metrics_backup_e2e::free_port`（`c89d4f0`）全部改走 `common::free_addr` /
  `free_ports`，进程内不再有预留之外的探测。
- 可诊断性（随 `4fc6c38`）：`failed_first_join_is_retried_on_restart` 断言打印子进程完整 `ExitStatus`；kill9 failover
  的 `raft set k2` 断言打印全部节点 ctx（两者都对应登记 10 的单次观测）。
- 局限（服务端缺陷，§登记 1）：join 一旦在 leader 侧卡满 `MEMBERSHIP_TIMEOUT=30s`，leader 即被毒化，WP4 的重生
  只换来 `internal error`；首轮全量门禁 `process_failover_e2e::kill9_leader_new_leader_elects_and_old_leader_catches_up`
  的失败即此形态（head timeout ×2 → 第 3 次 internal error）。

## 缺陷 5：MySQL 前端每个结果集固定 ~42ms（8d4c0c7）
- 症状：MySQL 前端每条结果集语句固定 ~42ms（批次 12/13/14 changelog 均记录；#170 `explain-ladder`
  预算按 ~20 个结果集 × 42ms 定）。
- 根因：opensrv-mysql 0.7 全库无 `set_nodelay`，shim 把裸 `OwnedWriteHalf` 交给它，一个结果集以多个小
  `write_all` 出 socket（列数、列定义、EOF/OK、行、EOF/OK）；Nagle 开启时第 2 包要等客户端对第 1 包的
  ACK，而等待完整结果集的客户端内核把 ACK 延迟到 ~40ms——每次环回往返都撞上 Linux 延迟 ACK 定时器。
- 实测（临时探针：单连接顺序环回，各 3 轮，BEFORE/AFTER 之间机器状态相同）：
  - `SELECT 1` ×300：avg 41.05ms、300/300 样本 >5ms、min 40.8ms → avg 0.09–0.37ms、0–1/300；
  - 20 行结果集 ×100：avg 42.4ms、100/100 >5ms → avg 1.2–1.6ms、0/100；
  - RESP `PING` ×300：avg 0.016–0.066ms → 0.028–0.053ms（RESP 本就每批一次写回包，无停顿）。
- 修复：`src/sql/front/serve.rs`、`src/resp/mod.rs` accept 后 `sock.set_nodelay(true).ok()`
  （对照 `sql/dist/server.rs:63`）；RESP 侧是与 Redis `acceptCommonHandler`/`sql::dist` 对齐，非性能收益。
  探针不入库（无可稳定断言的内容，数字以本条为准）。
- follow-up `314c407`（去掉停顿后暴露的两处夹具时序假设）：
  - (a) 节点首次分布式提交先于其首段 ts lease 到期时 `1213 sql ts: ts authority unreachable, no block above
    the write frontier (distributed commits fail fast; retry)`（`src/sql/tx/global.rs:64`，`REFILL_INTERVAL` 200ms；
    reserve 先于任何写入，重试安全）：`tests/common/mysql.rs` 增 `ER_LOCK_DEADLOCK` + `seed_insert(conn, sql, what)`
    （仅重试 code 1213 且文案含该串；`conflict:` 1213 不重试；200ms 间隔、15s 上限、重试 eprintln），用于
    `columnar_e2e`(1)/`sql_join_cluster_e2e`(3)/`sql_setop_cluster_e2e`(3)/`sql_upsert_cluster_e2e`(3) 全部种子插入；
  - (b) 2PC 提交后立即跨节点读：既非协调者也非参与者的节点按自身快照读点读（`exec/select.rs:40-50`，
    `COMPAT.md:549-551` columnar "Reads"），一个 refill tick + raft apply 内追平：`sql_upsert_cluster_e2e` 增文件内
    `wait_all_nodes(conns, sql, want)` 轮询（15s），用于 INSERT..SELECT 后的汇总计数与末尾 `ci` 读。
  - 验证：`--test sql -- cluster` 5 个目标 9/9 绿 ×5 轮（重试日志未触发，低负载未复现）；失败原文留档
    `sql_upsert_cluster_e2e.rs:224` 旧读 `left: "n5"`。

## 登记（后续，不在本批解决）
1. **服务端：join 在 leader 侧卡满 30s 即毒化 leader（此后客户端写全部超时）**。手工探针复现（单节点 leader，
   `curl '/join?peerAddress=127.0.0.1:1&raft-token=…'`，peer 永不上线）：30.0s 后 `internal error`（stderr
   `membership change timed out after 30s`）；此后每次 `/join` **立即** `internal error`（stderr
   `the cluster is already undergoing a configuration change at log … index: 3, last committed membership log id … index: 2`），
   `raft set` → `internal error err: apply timeout after 5s`，`raft stats` `num_peers: 1`、`latest_configuration_index: 3`
   > `commit_index: 2`、`last_log_index: 4`。机制：`add_voter`（`src/rcache/http.rs:382-397`）里
   `add_learner(id, addr, blocking=true)` 并不等 learner 真正追上——openraft 0.9.25 `check_replication_upto_date`
   以 `replication_lag_threshold`（缺省 5000 条，`src/rcache/mod.rs:84-93` 未改）判定"追上"，死 peer matching=None
   在日志很短时 lag≈3 ≤ 5000 即视为追上、立刻返回；随后 `change_membership` 把死 peer 提升为 **voter**，这条成员
   日志需要它自己的 ack 才能提交 → 永不提交 → 30s 超时；leader 留下一条未提交、把死 peer 算进 quorum 的成员
   日志（openraft `InProgress` 守卫拒绝一切后续成员变更，客户端写全部 5s 超时，直到该 peer 上线）。
   客户端 `JOIN_TIMEOUT`/`HEAD_TIMEOUT` 10s 先于服务端 30s 放弃，只让毒化对客户端不可见。候选修复（另立计划）：
   `add_learner(blocking=false)` 后显式等 `metrics.replication[id].matching ≥ membership_log_id` 再提升 voter，
   超时 `remove_learner` 回滚；或 voter 提升走 joint consensus / 对齐两处超时。WP4 重生救不了这条路径；
   负载下首次 join 为何 >10s 未明（该轮 node0 stderr 随 tempdir 删除）。
2. harness 残余：`wait_leader`（`tests/common/mod.rs:596`）标签滞后 raft metrics ≤~500ms（35 处使用）；
   scenario 目标约 20 个文件的私有静默排水（`rf`/`cmd_raw`/`cmd_full`：首字节只等 20–500ms 静默窗，checkout WATCH、
   cashier、lite dlq 三例已在门禁暴露并改首字节耐心；候选：`scenario_common` 共享一个首字节耐心的排水助手后迁移）；
   11 个 `tests/sql` 文件各带私有 `ddl`/`connect` 副本与各自的 leader 重试（H4-4）；`sql_2pc_e2e`/`sql_dist_read_e2e`/
   `sql_failover_e2e`/`sql_ddl_visibility_e2e`/`sql_ddl_surface_cluster_e2e`/`starrocks_model_e2e` 首次插入仍为裸
   `.expect`（同一 1213 时序窗口未覆盖）；`tests/lite/lite_proc_e2e.rs`、`lite_e2e.rs` 仍用无等待的 `cmd_full_reply`。
3. src 候选：ts reserve 的严格模式可改为短暂等待在途 lease 而非直接 1213（`src/sql/tx/global.rs:64`）。
4. `tests/rocksmq/rocksmq_http_e2e.rs` 私有夹具 EADDRINUSE 偶发（负载下约 5–15%）——私有探测已由 `a1a7bdd`
   接入进程级预留，进程内两类撞口关闭；跨进程残余同缺陷 4，继续观测。
5. `spawn_node_sql/_mysql/_es/_backup` 未纳入 join 重生（仍直调 `spawn_child`）。
6. 多键 EXISTS/DEL 对流名仍按全名哈希（CROSSSLOT）；SCAN/KEYS 本就经 `user_entry_of` 分类器列出流名（同一父槽，无需改）。
7. turso `InList` 空列表无守卫（`src/sql/turso/translate/expr.rs:38-54`）。
8. EXEC 对排队 XADD 的闩锁 `latch_key(slot_prefix(multi.slot), k)`（`src/tx/exec.rs:57-62`）与 lite
   元闩锁（`src/lite/append.rs:57`）不一致。
9. 流 RENAME 不迁移消费组水位/PEL（已知局限）。
10. 负载下单次/少数次观测（复跑即绿，不作修复；前两条的断言已补证据打印）：
   - `process_cluster_e2e::failed_first_join_is_retried_on_restart` 第 3 轮全量：`status.code()` 为 `None`
     （信号退出），stderr 仍是正常的 `join raft cluster failed:Connection refused`；单跑与之后各轮均 `Some(1)`。
   - `process_failover_e2e::kill9_leader_new_leader_elects_and_old_leader_catches_up` 顺序 cluster 轮：新 leader
     `raft set k2` → `internal error err: apply timeout after 5s`，stderr `VerifyLeader() err:not enough for a quorum,
     got: {self}`，即幸存 follower 5s 内未 ack（其 stderr 当时未打印，现打印全部节点）。
   - `scenario_regulatory_typed_report_e2e` 第 3 / 7 轮全量：perf 回归门 `typed123/50-insert+20-select` 5.05s
     （负载 ≈19）/ 3.41s > 3s；单跑同一块 **0.11s**（27× 余量），预算不放宽（放宽会让它对 ~40ms/句的 Nagle 类回归失明）。
   - kafka 目标（既有负载时序类，batch 3 曾记录同目标抖动）：第 7 轮 `kafka_fetch_e2e::fetch_budget_truncation`
     两条 XADD 后立即 Fetch，hwm `Some(1)`≠`Some(2)`（流 meta `len` 可见性滞后）；复跑 ×3 中 1 次
     `kafka_group_failover_e2e::session_timeout_sweeps_silent_member_to_rebalance` 新成员 commit 得 3
     （UNKNOWN_TOPIC_OR_PARTITION）而非 0，其余 2 次 31/31 绿。

## 门禁

全量门禁 = `RUST_TEST_THREADS=4 cargo test --workspace --no-fail-fast`（18 个 target、2196 例；共享 16 核主机，
外部负载 3–20），每轮按当时树如实记录，失败不隐藏；日志 `/tmp/gate_ws_*.log`（本机，不入库）。

| 轮 | 树 | 结果 | 失败 → 处置 |
| --- | --- | --- | --- |
| 0 | 314c407 之前 | exit 101 | `process_failover_e2e::kill9_leader…`：join head timeout ×2 → 第 3 次 `internal error`（leader 毒化路径，登记 1） |
| 1 | `314c407` | **全绿** 2196/0（scenario 272s） | — |
| 2 | `dcde3a7`（负载 ≈19，scenario 302s） | 2194/2 | `scenario_checkout_watch_e2e` `exec replies: ""`、`scenario_lite_gc_redelivery_e2e` 空 PEL 消费者 3s 未回收 → `7b4ea50` |
| 3 | `7b4ea50`（负载 ≈17–19，scenario 299s） | 2193/3 | `es_bulk_interop_e2e::es_bulk_auto_id_items` 首 bind EADDRINUSE 38215 → `4fc6c38`；`failed_first_join_is_retried_on_restart` `code() == None`、`scenario_regulatory_typed_report_e2e` perf 5.05s > 3s → 登记 10 |
| 4 | `4fc6c38` 之前的工作树 | 编译失败（预留静态量用了非 const 的 `HashSet::new()`），0 例 | 改 `OnceLock`，rustfmt/clippy 后重跑 |
| 5 | `4fc6c38`（scenario 369s） | 2195/1 | `scenario_cashier_abort_discard_e2e` `commit after discard: []` → `9957e8d` |
| 6 | `a1a7bdd` + `c89d4f0` 内容 | **全绿** 2196/0（scenario 291s） | — |
| 7 | `9957e8d`（最终树，scenario 345s） | 2194/2 | `kafka_fetch_e2e::fetch_budget_truncation` hwm `Some(1)`≠`Some(2)`、`scenario_regulatory_typed_report_e2e` perf 3.41s > 3s → 均为负载时序单次观测，登记 10 |

- `--test cluster` 顺序（`RUST_TEST_THREADS=4`）：`dcde3a7` ×3 25/25；预留前工作树 ×3 = 25/25、23/25、25/25
  （23 那轮：kill9 `raft set k2` apply timeout → 登记 10；`standalone_boots_turso_engine_with_flag` 首 bind
  EADDRINUSE 38985 → 派生块入预留）；`4fc6c38` ×4 25/25（21s 级）。`--test es` ×2 12/12。
- 并发 ×3 `--test cluster`（`dcde3a7`，外部负载 ≈10）：25/25、24/25（`backup_failover` 同配置 respawn EADDRINUSE
  43787 → `4fc6c38`）、24/25（`cluster_init_seeds_leader_binds_in_sql_nodes` join head timeout ×2 → internal error，
  登记 1）。
- 定向：scenario `cashier|checkout_watch|lite_gc_redelivery` 3/3、`standalone_e2e` 3/3、`--test kafka` ×3
  （30/31、31/31、31/31，失败见登记 10）、`regulatory_typed_report` 单跑 perf 块 0.11s；
  `cargo clippy --workspace --all-targets -- -D warnings` 在 `4fc6c38` / `a1a7bdd` / `9957e8d` 各干净一次。
