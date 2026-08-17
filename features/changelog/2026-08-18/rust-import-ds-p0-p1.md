Commit: (working-tree, 随本提交入库)

# Rust 重写首入库与数据结构 P0/P1（keys/TTL/Hash/Set）

## 背景
`rust/` 重写此前仅存在于工作树（见 [2026-08-17/rust-rewrite.md](../2026-08-17/rust-rewrite.md)），从未入库。本次随首提交入库，并按七类数据结构计划落地 P0/P1：P0 = 物理编码基座 + keys 命令族 + 统一 TTL；P1 = Hash 与 Set 全命令集。

## 变更
### 首入库（rust/ 全树 + .gitignore）
- **`rust/`**：cargo workspace 全树首提交（`rdb` + `bench`），含 lite 流命令模块（XADD/XLEN/XRANGE/XTRIM/XDEL/XIDLE/XREAD/XREADGROUP/XACK/XGROUP/XINFO/XPICK）与其 e2e。
- **`.gitignore`**：裸 `rdb` → `/rdb`（不再误伤 `rust/rdb/`），新增 `rust/target/`。

### P0：物理编码基座 + keys 命令族 + 统一 TTL
- **`rust/rdb/src/ds/codec.rs`**（测试外置 `codec_tests.rs`）：typed-key 物理编码 `<slot>/<kind:u8><u32 BE key_len><key><suffix>`；kind 0x00 raw string 无信封零开销，其余 value = LEB128 `expire_ms` 信封 + payload；`0xFD` 过期索引键；按 family 的删除范围。
- **`rust/rdb/src/ds/expire.rs`**：全类型统一 TTL——读路径惰性判定 + 后台主动采样 `spawn_active_expire`（`main.rs` 装配）。
- **`rust/rdb/src/ds/latch.rs`**：用户键分片读写锁（读改写串行化）；**`wait.rs`**：阻塞命令 WaitHub（P2 BLPOP 族备用）。
- **`rust/rdb/src/command/keys*.rs`**：TYPE/EXISTS/DEL/UNLINK/EXPIRE 族（NX/XX/GT/LT）/TTL/PTTL/PERSIST/SCAN/KEYS/RANDOMKEY/RENAME(NX)。
- **`rust/rdb/src/store/ops.rs`**：get/delete_range/batch_write（含 async 变体）/for_each_from/prefix 采集等存储操作。

### P1：Hash 与 Set 全命令集
- **`rust/rdb/src/ds/{hash_ds,set_ds,setops}.rs`** + **`rust/rdb/src/command/{hash_cmd,hash_scan,hash_incr,set_cmd,set_scan,setops_cmd}.rs`**：
  - Hash：HSET/HSETNX/HGET/HMGET/HDEL/HLEN/HEXISTS/HSTRLEN/HINCRBY/HINCRBYFLOAT/HGETALL/HKEYS/HVALS/HSCAN/HRANDFIELD；
  - Set：SADD/SCARD/SISMEMBER/SMISMEMBER/SMEMBERS/SMOVE/SPOP/SREM/SRANDMEMBER/SSCAN/SDIFF/SINTER/SUNION（±STORE）；
  - 多 key 命令 CROSSSLOT 校验 + hash tag 聚合。
- **`rust/rdb/src/command/mod.rs`**：注册表改 async（`Handler = for<'a> fn(&'a mut Ctx<'_>) -> HandlerFuture<'a>`）。

## 测试覆盖
| 功能 | 测试名 | 文件 |
|------|--------|------|
| 派生键/信封/过期索引 roundtrip | `data_key_roundtrip` 等 11 项 | `rust/rdb/src/ds/codec_tests.rs` |
| 主动采样清 family 与陈旧索引 | `sampler_purges_expired_families_and_stale_index` | `rust/rdb/tests/ds_e2e.rs` |
| family 删除范围 | `delete_range_wipes_family_records_sampler_sweeps_index` | `rust/rdb/tests/ds_e2e.rs` |
| EXPIRE 族/TTL 持久化 | `set_expire_ttl_persist_via_registry` | `rust/rdb/tests/expire_e2e.rs` |
| 惰性过期与 DEL 清理 | `pexpireat_lazy_expiry_and_del_cleanup` | `rust/rdb/tests/expire_e2e.rs` |
| MGET 信封读/RENAME 带 TTL/SCAN+KEYS | `mget_reads_enveloped_and_missing_keys` 等 | `rust/rdb/tests/expire_e2e.rs` |
| Hash 生命周期/TTL/HSCAN | `hash_lifecycle_through_registry` 等 | `rust/rdb/tests/hash_set_e2e.rs` |
| Set 生命周期/代数/SMOVE/CROSSSLOT | `set_algebra_smove_and_crossslot` 等 | `rust/rdb/tests/hash_set_e2e.rs` |
| keys/hash/set 单元（arity/WRONGTYPE/边界） | `keys::tests`、`hash_tests`、`set_tests` | `rust/rdb/src/command/` |

- 全量回归：`cargo test --workspace` → 239 passed / 0 failed（lib 198 + main 2 + 集成 28 + bench 11）
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → 零警告
- build：`cargo build --workspace` → 干净
- 行数：新增文件均 ≤400（最大 `keys_core.rs` 379；`hash_cmd.rs` 309、`codec.rs` 266，超限文件已拆分）

## Impact Surface
- 客户端可感知：新增 keys/Hash/Set 命令族与全类型 TTL；raw string 物理格式不变（`<slot>/`+key）。
- typed 值采用信封物理编码（相对 2026-08-17 工作树描述的格式；此前未发布，无部署影响，COMPAT.md 后续阶段补记）。
- 不影响：Go 实现（`internal/`）、Raft 控制面与 HTTP API、monitor 指标、bench CLI、RESP 协议层。

## Related Docs
- [agents/rust](../../agents/rust/index.md)
- [features/kv-storage](../../features/kv-storage/index.md)
- [2026-08-17/rust-rewrite.md](../2026-08-17/rust-rewrite.md)、[2026-08-17/rust-e2e-benchmark.md](../2026-08-17/rust-e2e-benchmark.md)
