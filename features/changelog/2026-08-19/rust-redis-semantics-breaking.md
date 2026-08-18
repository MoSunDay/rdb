Commit: (working-tree, 随本提交入库)

# Redis 语义对齐（BREAKING）：空 tag slot / SET 选项矩阵 / RAFTGET bulk / 控制面 401

## 背景
评审批准的 7 项 BREAKING 语义对齐（COMPAT.md「Intentional fixes of Go bugs」
条目 5-11）：Go 实现的既有行为偏离 Redis/协议正确性，继续维持属 bug-parity
负担。Go 已归档（见同日 [go-archive.md](./go-archive.md)），Rust 侧正式收敛。
同文件交织的 membership mux 并发修复（评审 B4）一并入库。

## 变更

### slot 语义（条目 8/9/10）——客户端可见 MOVED 变化
- **`rust/rdb/src/hash.rs`**：`hash_tag`（:75）空 hash-tag `{}` 不再视为有效
  tag——Go 把 `foo{}bar` 类 key 钉死 slot 0，现在整 key 参与哈希；golden 向量
  更新（`{}`→15257、`foo{}bar`→14292）。
- **`rust/rdb/src/router.rs`**：`route`（:28）16384%N≠0 时尾节点吸收余数槽至
  16383，每 slot 恰一 owner（Go 中余数槽无 owner、落谁服务谁）。
- **`rust/rdb/src/topology.rs` + `command/cluster.rs`**：`parse_node_slots`
  （:63）末节点范围从运行中 start 到 16383，单节点 `cluster nodes/slots`
  报 `0-16383`（原丢 slot 0 的 off-by-one）。

### SET 选项矩阵（条目 6 关联）与 RESP 帧
- **`rust/rdb/src/command/string_opts.rs`（新）**：SET 选项解析独立模块——
  EX/PX/EXAT/PXAT（大小写不敏感、乱序）+ NX/XX/KEEPTTL/GET；冲突/重复/
  未知/缺值 → `ERR syntax error`；≤0 → `ERR invalid expire time in 'set'
  command`。
- **`rust/rdb/src/command/string.rs`**：完整选项矩阵——NX/XX veto 回 null
  bulk（带 GET 回旧值）、GET 撞非 string 报 WRONGTYPE、KEEPTTL 继承旧
  deadline；测试外移至 `command/string/`（test_util/tests/set_opts_tests）。
- **`rust/rdb/src/resp/conn.rs`**：`arity_error`（:226）——缺 key 参数/空
  多批量 `*0` 回 Redis 标准 arity 错误（Go 越界伪 panic 回复），错误路径不
  采样延迟；`may_block`（:103）dispatch 阻塞命令前预刷管线，前序回复不再
  被阻塞弹住。

### 控制面（条目 5/7/11 + 评审 B4 修复）
- **`rust/rdb/src/command/raft_cmd.rs`**：`raft_get_cmd`（:77）值改 bulk
  string 帧（`$N`）——Go 简单串会让含 CRLF 的值破坏 RESP 帧；missing 仍 `$-1`。
- **`rust/rdb/src/rcache/http.rs`**：token 不符回 `401 Unauthorized`（Go 假
  `ok` 静默放行）；`MembershipMux`（:36）持锁覆盖 add_learner→change_membership
  全序列，并发 /join 不再竞态丢更新；`MEMBERSHIP_TIMEOUT` 30s 封顶，不可达
  peer 不再永久楔死控制面；`remove_voter` 彻底移除 voter 不留 learner。
- **`rust/rdb/src/main.rs`**：`config_missing_error`（:37）悬空 `-config` 报错
  退出（对齐 Go Fatalf）；membership/observer mux 装配接线（:255,:304）。
- **`rust/rdb/tests/lite_e2e.rs`**（评审 B 组 BLOCK 0 修复的回归，随装配链入库）：
  `park_reader` 辅助以「pipeline AUTH + 阻塞命令单写、等 +OK 预刷到达即确认
  parked」的手法测 BLOCK 0 / 超大 block 值（>24h park 切片）永久等待语义，
  依赖 conn.rs 预刷行为，故随本提交入库。
- **`rust/rdb/src/rcache/ha.rs`**（评审 B 组修复，随装配链入库）：`ObserverMux`
  （:52）串行化「读 FSM→决策→apply」，`converge_instances` 每轮 apply 后重读
  收敛，不再基于旧快照盲写覆盖并发 `cluster init`；`self_recover` 同纪律 +
  len==2 防 Go panic。
- **`rust/rdb/src/command/string.rs`**：QUIT 只回一个 `+OK`（删 Go 的 +PONG）。

### 文档
- **`rust/COMPAT.md`**：新增条目 5-11；「Join ordering」补 membership mux
  说明；删除已修复项的旧 quirk 行。

## 测试覆盖
| 功能 | 测试名 | 文件 |
|------|--------|------|
| QUIT 单 +OK | quit_replies_exactly_one_ok_then_closes | `tests/resp_reply_semantics.rs:107` |
| arity 错误 | missing_key_command_replies_arity_error / empty_multibulk_replies_arity_error_with_empty_name | `tests/resp_reply_semantics.rs` / `tests/resp_e2e.rs:226` |
| 空 tag slot | empty_tag_hashes_whole_key_not_slot_zero | `src/hash.rs:198` |
| 余数槽吸收 | five_nodes_last_node_absorbs_remainder / every_slot_owned_for_one_to_seventeen_nodes | `src/router.rs:151,175` |
| 全范围上报 | node_slots_single_node_full_range / slots_single_node_reports_full_zero_to_16383_range | `src/topology.rs:138` / `command/cluster.rs:339` |
| SET 矩阵 | set_ex_px_nx_xx_keepttl_get_full_option_matrix 等 4 项 | `command/string/set_opts_tests.rs` |
| 选项解析 | conflicts_and_duplicates_are_syntax_errors / bad_expire_values_error 等 | `command/string_opts.rs` |
| RAFTGET bulk | get_value_containing_crlf_keeps_bulk_framing | `command/raft_cmd.rs:174` |
| 预刷 | pipelined_replies_flush_before_blpop_parks | `tests/list_e2e.rs` |
| may_block 覆盖 | may_block_covers_exactly_the_parking_commands | `resp/conn.rs:270` |
| 并发 join | concurrent_joins_keep_all_voters + 401 断言 | `tests/raft_cluster_e2e.rs:213,185` |
| Observer 串行化 | handler_observer_fails_over_and_back / failover_concurrent_with_operator_write_loses_nothing / failover_concurrent_observers_keep_both_swaps | `tests/ha_failover.rs` |
| BLOCK 0 / 超大值永久等待 | block_zero_and_oversized_block_wait_for_xadd | `tests/lite_e2e.rs` |
| 阻塞前预刷回归 | block_wakes_on_xadd_over_wire | `tests/lite_e2e.rs` |
| BZPOPMIN 预刷回归 | bzpopmin_wakes_on_zadd_over_wire（pipeline 重写 hunk） | `tests/zset_e2e.rs` |
| 进程级 bulk | raft get 断言 `$3` 帧 / poll_get_bulk | `tests/process_cluster_e2e.rs:220` / `process_failover_e2e.rs:41` |

- 全量回归：`cargo test --workspace` → 456 passed（363 lib + 93 e2e）
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → 零警告
- 行数：`command/string.rs` 310 + `string/{tests 29, set_opts_tests 272, test_util 267}` ≤ 400/800

## Impact Surface
- **BREAKING：slot 迁移**——空 tag key（`foo{}bar`）与 16384%N 余数槽的
  owner 变化，客户端可能收到新的 MOVED 重定向；升级需评估集群拓扑。
- **BREAKING：协议帧**——RAFTGET 值 bulk 化、QUIT 单 +OK、arity 错误文案。
- **BREAKING：控制面**——/join /depart 错 token 得 401（原假 ok），部署
  脚本需校验状态码而非 body。
- 不影响：存储物理布局、Raft 日志/快照格式、Lite Mode 语义。

## Related Docs
- [agents/rust](../../agents/rust/index.md)
- [rust/COMPAT.md](../../../rust/COMPAT.md)
- [go-archive.md](./go-archive.md) / [rust-review-fixes.md](./rust-review-fixes.md)
