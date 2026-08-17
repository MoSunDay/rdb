# 数据结构 P2：List/ZSet 全命令集与阻塞命令族

## 背景
P0/P1（[2026-08-18/rust-import-ds-p0-p1.md](../2026-08-18/rust-import-ds-p0-p1.md)）落地 ds/ 物理编码基座与 keys/Hash/Set 后，P2 按七类结构计划推进 List 与 ZSet 全命令集，并启用 `ds/wait.rs` WaitHub 实现阻塞命令族；阻塞唤醒复用 lite 验证过的 register→锁内快路径→24h 分片 park 循环。

## 变更
### ds 层（`rust/rdb/src/ds/`）
- **`list_ds.rs`**（测试 `list_ds_tests.rs`）：两端队列编码——`KIND_LIST_L` 用 8B BE 补码索引（物理升序=逻辑左段顺序），`KIND_LIST_R` 用正索引；meta 存 `l_count/l_next/r_count/r_next` 四 varint。不变量：每侧存活索引稠密无洞，LREM/LTRIM 删除内部元素后由命令层 compact 重排。
- **`zset_ds.rs`**（测试 `zset_ds_tests.rs`）：双记录编码——`KIND_ZSET_MEMBER`（member→8B 可排序分值，f64 符号翻转技巧，±inf 安全）+ `KIND_ZSET_SCORE`（可排序分值++member，物理升序=分值序、同分按成员字典序）；`for_each_scored` 有界前向迭代、`count_before` 供 ZRANK。
- **`wait.rs`**：新增 `register_shared`——同一 waiter 挂多个 key 队列，支撑 BLPOP/BZPOPMIN 多 key 阻塞。

### List 命令（20 个，`rust/rdb/src/command/list_*.rs`）
- `list_cmd.rs`：LPUSH/RPUSH(±X)/LLEN/LRANGE/LINDEX/LSET；`list_ops.rs`：LPOP/RPOP（单值与 count 形态）；`list_rewrite.rs`：LREM/LTRIM（dense 重排）；`list_move.rs`：LINSERT/LPOS(RANK/COUNT/MAXLEN)/LMOVE/RPOPLPUSH。
- 阻塞 `list_block.rs`：BLPOP/BRPOP（多 key、FIFO 唤醒、超时 `*-1`）、BLMOVE/BRPOPLPUSH（超时 nil bulk）；推送类命令提交后 `wait::notify` meta 根键。

### ZSet 命令（27 个，`rust/rdb/src/command/zset_*.rs`）
- `zset_cmd.rs`：ZADD(NX/XX/GT/LT/CH/INCR)/ZINCRBY；`zset_read.rs`：ZCARD/ZSCORE/ZMSCORE/ZCOUNT/ZRANK/ZREVRANK(±WITHSCORE)/ZRANDMEMBER；`zset_pop.rs`：ZREM/ZPOPMIN/ZPOPMAX；`zset_range.rs`：ZRANGE(BYSCORE/BYLEX/REV/LIMIT/WITHSCORES) 及方向变体/ZLEXCOUNT；`zset_util.rs` 共享解析与提交。
- `zset_remops.rs`：ZREMRANGEBYRANK/SCORE/LEX；`zset_scan.rs`：ZSCAN(±WITHSCORES)；`zsetops_cmd.rs`：ZUNIONSTORE/ZINTERSTORE/ZDIFFSTORE（WEIGHTS/AGGREGATE）；`zset_block.rs`：BZPOPMIN/BZPOPMAX（`*3` 应答、超时 `*-1`）。

### 测试
- 单测（经注册表 arm 分发，arity/WRONGTYPE/边界）：list 20、zset 31、ds 纯数学 9、wait 并发 1（register_shared）。
- e2e：`tests/list_e2e.rs` 8 用例——生命周期、LREM compaction 正确性、TTL 惰性清理、CROSSSLOT、BLPOP 丢失唤醒回归（`blpop_got_does_not_swallow_next_notify`）、真实进程跨连接唤醒；`tests/zset_e2e.rs` 8 用例——生命周期、边界、代数、TTL、ZSCAN 游标、BZPOPMIN 跨连接唤醒。

## 验证
- 全量回归：`cargo test --workspace` → 317 passed / 0 failed（lib 259 + main 2 + 集成 45 + bench 11）
- `cargo clippy --workspace --all-targets -- -D warnings` → 零警告；`cargo build --workspace` 干净；`cargo fmt --check` 干净
- 行数：新增文件全部 ≤400（最大 `list_move.rs` 400、`list_cmd.rs` 399）

## Impact Surface
- 客户端可感知：新增 List/ZSet 47 命令与阻塞命令族；阻塞仅停驻本连接任务，其管道中先入的应答会被扣留至唤醒返回（与 lite BLOCK 相同的既有行为）。
- 复用 P0 编码基座：LIST/ZSET 预留 kind/family 即刻启用，keys 族/EXPIRE/TTL/DEL/TYPE/主动采样对新类型零改动生效。
- 不影响：Go 实现、Raft 控制面与 HTTP API、monitor、bench、RESP 协议层。

## Related Docs
- [agents/rust](../../agents/rust/index.md)、[features/kv-storage](../../features/kv-storage/index.md)
- [2026-08-18/rust-import-ds-p0-p1.md](../2026-08-18/rust-import-ds-p0-p1.md)
