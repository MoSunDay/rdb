Commit: (working-tree, 随本提交入库)

# 上线评审 P0/P1 修复与正确性加固（非破坏性）

## 背景
上线评审 22 条目中的 P0/P1 缺陷修复，外加评审外发现的配套正确性问题
（CROSSSLOT 守卫缺口、多元素阻塞唤醒、水位回退竞态等）。全部为
崩溃/死锁/数据损坏修复或向 Redis 语义收敛的**非破坏**变更；
破坏性语义对齐另见同日 [rust-redis-semantics-breaking.md](./rust-redis-semantics-breaking.md)。

## 变更

### 崩溃与死锁（P0）
- **`rust/rdb/src/utils.rs`**：`glob_match`（:39）字符类恒消耗一字节，负向类
  `[^x]` 匹配空串时 `&s[1..]` 越界 panic——SSCAN 空成员 + `MATCH [^x]*` 曾崩。
- **`rust/rdb/src/command/set_cmd.rs`**：`smove`（:311）补 CROSSSLOT 守卫、
  src==dst 直答（自死锁）、双 latch 按字节序加锁防 ABBA；`spop`（:242）
  拒绝 count≤0（负数经 `as u64` 回绕弹空整个集合）；`sadd`/`srem` 同命令
  重复成员去重。
- **`rust/rdb/src/command/keys_core.rs`**：`rename_key`（:287）src==dst 直答 +
  双 latch 字节序；`delete_records`（:140）RawString 快路径补 latch，防并发
  EXPIRE 在已报告删除后留下迁移记录。

### 数据正确性（P1）
- **`rust/rdb/src/command/zset_cmd.rs`**：新增 `effective_score`（:22），同一
  ZADD 内重复 member 读批内 pending 分数，修复重复对索引损坏；同分重加不计
  added/changed（CH 语义）；缺失成员 INCR 直接落 delta 保住 -0 符号。
- **`rust/rdb/src/command/zset_util.rs` + `zset_read.rs` + `zset_range.rs`**：
  `seek_from_sortable`（zset_util :83）含 0.0 下界一律从 -0.0 起 seek——±0.0
  排序位不同曾静默漏成员（ZRANGEBYSCORE/ZCOUNT/ZRANGE REV）；REV BYSCORE
  参数交换、REV+LIMIT 按回复序裁剪、LIMIT 缺参报 syntax error。
- **`rust/rdb/src/command/hash_scan.rs`**：仅 "0" 重启游标（空字段 "" 落页边界
  曾翻页死循环）；`hrandfield` 无 count 带 WITHVALUES 报错；负 count 用
  `unsigned_abs()`（i64::MIN 溢出）。
- **`rust/rdb/src/lite/offset.rs` + `lite/mod.rs:113`**：`drop_superseded`
  （offset :179）落盘前复核快照，旧 ACK 批晚到不再把 committed 水位拉回
  （崩溃后重投已 ACK 消息）或复活已删组。
- **`rust/rdb/src/rcache/store.rs`**：`get_cf_json`（:122）三态映射——JSON
  损坏从静默 None 改为 Err，吞错曾导致同 term 重复投票（Raft 安全违规）。
- **`rust/rdb/src/rcache/store_snapshot.rs`**：`save_snapshot`（:65）固定
  数据→meta→删旧顺序，原顺序崩溃后旧 meta 指向已删文件、load 永久 crash-loop。
- **`rust/rdb/src/lite/entries.rs`**：`count_reap`（:69）CAS-weak 且 0 为下限，
  双读者不再把 `streams_live` gauge 减成负数。

### 阻塞与唤醒
- **`rust/rdb/src/ds/wait.rs`**：新增 `notify_n`（:101）单次持锁唤醒至多 n 个
  waiter；`signal_if_armed` 防二次唤醒、stale 队列项跳过。
- **`command/list_cmd.rs` / `list_block.rs` / `zsetops_cmd.rs` / `zset_cmd.rs`**：
  多元素提交按元素数唤醒（LPUSH N 元素曾只醒 1 个 waiter）；BLMOVE/BRPOPLPUSH
  dst 预检 WRONGTYPE + `restore_popped` 防元素丢失；ZINTERSTORE 等落盘后按
  结果成员数唤醒。
- **`rust/rdb/src/lite/read.rs`**：XREAD BLOCK 0 = 永久等待（Redis 语义）；
  分片可续期 park，超大 BLOCK 值不再被钳到单一封顶 deadline。

### 集群与运维
- **`rust/rdb/src/ds/setops.rs` + `command/keys.rs`**：`require_same_slot`
  （setops :40）统一 CROSSSLOT 守卫接入 EXISTS/DEL/UNLINK/RENAME 等——此前
  只按首个 key 定 slot，跨槽 key 会读/删错槽数据；`read_members` 先 resolve，
  异类型报 WRONGTYPE 而非静默空集。
- **`rust/rdb/src/rcache/join.rs`**：出站 join URL 对 peerAddress/token
  `percent_encode`（token 含 `&`/`+` 曾拆坏查询串被假 "ok" 静默放行）。
- **`rust/rdb/src/store/ops.rs`**：`delete_range_paged`（:69）每页 ≤1000 key
  独立批 + 游标续扫，替代整 keyspace 单一无限大 fsync 批。
- **`rust/rdb/src/ds/expire.rs`**：`sample_once`（:145）返回游标跨轮续扫 +
  自适应加轮，高 slot 到期键不再饥饿。

## 测试覆盖
| 功能 | 测试名 | 文件 |
|------|--------|------|
| glob 负向类 panic | sscan_negated_class_match_over_empty_member | `command/set_tests.rs` |
| SPOP 负数回绕 | spop_negative_count_errors_and_keeps_set | `command/set_tests.rs` |
| SMOVE 死锁/语义 | smove_semantics_and_wrongtype / concurrent_reversed_smove_never_deadlocks | `command/set_tests.rs` / `tests/blocking_concurrency_e2e.rs` |
| 多元素唤醒 | multi_element_lpush_wakes_all_parked_waiters / zadd_multiple_members_wake_all_parked_bzpopmin | `tests/blocking_concurrency_e2e.rs` |
| ZADD CH / BYLEX | zadd_ch_unchanged_and_bylex_withscores_errors | `tests/zset_e2e.rs` |
| -0 符号保持 | zincrby_negative_zero_on_new_member_keeps_sign / zadd_incr_negative_zero_keeps_sign | `command/zset_tests.rs` |
| 水位回退竞态 | drop_superseded（单测） | `lite/offset.rs:280` |
| HRANDFIELD i64::MIN | pick_many 负 count（单测） | `command/hash_scan.rs:284` |
| CROSSSLOT/槽覆盖 | type_exists_and_del_roundtrip（{x} tag 化） | `command/keys/tests.rs` |
| 过期游标续扫 | 既有 ds 用例适配 sample_once 新签名 | `command/json_arr_tests.rs` / `tests/ds_e2e.rs` |

- 全量回归：`cargo test --workspace` → 456 passed（363 lib + 93 e2e）
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → 零警告
- membership mux 并发修复与 /join 401 同文件（`rcache/http.rs`）交织；装配链
  `main.rs`（`config_missing_error` :37、mux 接线 :255,:304）与 `rcache/ha.rs`
  （ObserverMux 串行化）因编译依赖（新 API `membership_mux`/5 参 `serve_on`/
  `spawn_leader_probe` 新签名）须与 http.rs 同提交以保证中间提交可编译——
  三文件及 `tests/ha_failover.rs`、`tests/lite_e2e.rs`（`park_reader` 依赖
  conn.rs 预刷行为）、`tests/zset_e2e.rs` 的 `bzpopmin_wakes_on_zadd_over_wire`
  预刷重写 hunk 均随 breaking 提交入库（该文件的 `zadd_ch` 等新回归留本提交），见
  [rust-redis-semantics-breaking.md](./rust-redis-semantics-breaking.md)。

## Impact Surface
- 全部为缺陷修复：崩溃路径恢复为正常错误回复、错误数据修正为 Redis 语义。
- 不改变任何命令的成功回复格式；跨槽错误从「读错数据」变为 CROSSSLOT 错误。
- 不影响：存储物理布局、Raft 日志格式、Lite Mode 水位记录格式。

## Related Docs
- [agents/rust](../../agents/rust/index.md)
- [rust/COMPAT.md](../../../rust/COMPAT.md)
- [2026-08-18/rust-import-ds-p0-p1.md](../2026-08-18/rust-import-ds-p0-p1.md)
