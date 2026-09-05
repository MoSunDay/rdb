Commit: (working-tree, pre-initial-commit)

# KV 命令面补齐与多视角 e2e 测试加固

## 背景
对照 Redis 客户端可用的命令面，补齐 Rust 实现缺失的 string 族（算术/读写）、
位族、COMMAND/服务器元命令与散点缺口（HMSET/ZREVRANGE/SINTERCARD/LMPOP/XREVRANGE），
并以五层视角（handler、wire、MOVED 路由、MULTI、真实进程 kill -9）铺设 e2e，
上线前暴露问题。

## 变更
### 新命令（注册表 188 条，`COMMANDS` 静态表与注册表双向同步测试锁定）
- `src/command/string_incr.rs`：INCR/DECR/INCRBY/DECRBY/INCRBYFLOAT——共享核，
  TTL 保持，i64 溢出拒绝，f64 最短回程格式，NaN/Inf 拒绝。
- `src/command/string_rw.rs`：APPEND/STRLEN/GETSET/SETNX/SETEX/PSETEX/GETDEL/
  SETRANGE/GETRANGE——GETSET 清 TTL；SETRANGE 置空删键；SETNX 的 NX 否决先于
  类型检查（Redis parity：hash 键上回 `:0` 而非 WRONGTYPE）。
- `src/command/bitops.rs` + `bitops/bits.rs`：SETBIT/GETBIT/BITCOUNT/BITPOS/BITOP，
  MSB-first 位编号（对齐 Redis 7.2.5 实测）；BITOP 目标键族清理 + TTL 清除。
- `src/command/cmd_meta.rs`：COMMAND 元数据静态表；`server_cmd.rs`：
  COMMAND/COUNT/INFO/DOCS/GETKEYS、INFO 分节、DBSIZE、ECHO、SELECT、FLUSHDB
  （分块删除、保控制面记录）。
- 散点：HMSET、ZREVRANGE、SINTERCARD、LMPOP（`list_mpop.rs`，pop 核抽到
  `list_ops.rs`）、XREVRANGE（`lite/range_rev.rs` + `store/ops.rs` 倒序迭代器）。

### 修复（e2e 暴露的真 bug）
- **argv[2] 路由键**：LMPOP/SINTERCARD（numkeys 前置）与 BITOP（op 词前置）此前
  按 argv[1] 求 slot，物理前缀写错位。新增 `router::routing_key_index()`
  （lmpop/sintercard/bitop → 2），`command/mod.rs` 派发与 `resp/conn.rs`
  MULTI 队列同用该表；真二进制 smoke 验证修复。
- **SETEX 秒数 0**：未校验即写零 TTL——补 `ERR invalid expire time` 拒绝。
- **PING 带参**：可选 message 回显（Redis parity）。

### e2e（六文件，全部精确字节断言）
- `tests/string_more_e2e.rs`：算术/溢出/值错误/TTL 语义矩阵/12 命令 WRONGTYPE
  矩阵/SETRANGE-GETRANGE 边界/惰性过期重启计数/32 任务×25 并发 INCR（latch 原子性）。
- `tests/bits_e2e.rs`（wire）：MSB-first 编号表、跨字节稀疏置位、BITPOS 四规则、
  BITOP 混合长度/TTL/族覆盖/CROSSSLOT、MULTI 队列时 CROSSSLOT→EXECABORT、50 连发 pipeline。
- `tests/server_surface_e2e.rs`（wire）：COMMAND 全子命令形状、INFO 分节与惰性
  expires 递减、DBSIZE 跨家族计数、FLUSHDB 2100 键跨分块页、SELECT/ECHO 帧精确。
- `tests/routing_newcmds_e2e.rs`（wire，注入双节点拓扑）：新命令 MOVED 回归——
  LMPOP/SINTERCARD/BITOP 以 argv[2] 为路由键的陷阱用例（argv[1] 数字 token 落本
  地槽也不得本地服务）；无键命令不重定向。
- `tests/gaps_e2e.rs`（wire）：五散点命令全语义 + 全错路径矩阵（numkeys/LIMIT/
  exclusive id/COUNT clamp/全部清空 `*-1` 等）+ MULTI/EXEC。
- `tests/kv_newcmds_proc_e2e.rs`（进程级）：真实二进制热身矩阵（全部新命令）+
  MULTI/EXEC + kill -9 respawn 持久化（INCR 计数、位图、流倒序、TTL 绝对期限、
  LMPOP 已弹出）+ wire FLUSHDB。

## 验证
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo test --workspace`（52 测试目标全绿，lib 864）；真二进制 smoke 40+ 检查 PASS。
- 未发现新回归；上述两处修复均有回归用例锁定。

## 兼容
- 见 [COMPAT.md](../../../COMPAT.md)「Command-surface expansion」：SETNX NX 否决
  先于类型检查、路由键 argv[2] 规则、MSB-first 位编号。

## 第二轮整改（评审 TODO 清单）

### 文件拆分（repo 行数上限合规）
- `src/command/cmd_meta.rs`（497L）→ `cmd_meta/{mod.rs, table.rs, tests.rs}`，
  公共 API 路径不变；tests.rs 额外收纳本轮新增路由一致性测试。
- `src/command/bitops/tests.rs`（407L）→ `bitops/tests/{mod.rs, set_get_count.rs,
  pos_op.rs}`（共享 helper 留在 mod.rs `use super::*`）。

### 路由/队列键交叉校验（TODO 4）
- `cmd_meta/tests.rs` 新增 `routing_consistency_tests`：对 COMMANDS 表逐行断言
  Shape::None ⇒ `router::is_whitelisted`；否则按 shape 采样 argv，路由键位置
  （`router::routing_key_index`）必属 `keyspec::keys_of` 输出——静态表不再能
  与运行时路由/事务入队语义漂移。

### FLUSHDB × lite 流孤儿复活修复（TODO 6，真 bug）
- 现象：FLUSHDB 清掉流/组记录后，200ms 脏偏移缓存回刷会把孤儿 group 记录
  写回（`xinfo groups` 在 wipe 后仍列出 g1）。
- 修复：`src/lite/offset.rs` 新增 `pub fn clear_all`，`flushdb.rs::flushdb`（与 `keyspace_role.rs` 的全键空间记录分类同拆自 `server_cmd.rs`）
  分块擦除前先清缓存；`tests/flushdb_lite_e2e.rs`（进程内，137L）锁定：显式
  回刷轮后无复活、陈旧组读 `-NOGROUP`、自动 id 不回退、重建可用、二次 wipe 干净。
  变异验证：关掉修复该 e2e 以 *1（复活）失败，开启即绿。
- COMPAT.md FLUSHDB 条目补充偏移缓存清除与 NOGROUP 对齐说明。

### 长稳（kill -9 respawn 契约）
- 新增 `scrtips/e2e_scenarios/soak_kill9.sh`（单 bootstrap 节点、rdb-bench
  mixed 8×16 双 90s 窗口 + 串行 acked-INCR 写手；SIGKILL 落在健康 bench 窗口
  跑完之后、仅串行 acked-INCR 写手仍在途时，同配置原地 respawn）：
  counter ∈ {N, N+1} 且单调续写，两窗口 bench 零客户端错误（180s 全程
  1,031,744 ops，≈103.2 万），/metrics 载荷下可答。
- env.sh 修复仅一处：`RDB_E2E_KEEP_WORKDIR` 别名此前被忽略（`_e2e_spawn`
  本身未动）；初始 spawn 的 3 次换端口带重试（EADDRINUSE 竞态）落在
  `scrtips/e2e_scenarios/soak_kill9.sh`（首启 spawn 循环 ~43–57 行，同文件
  respawn 循环随后补齐同样的重试）。

## 最终门禁（第二轮）
- `cargo fmt --all -- --check` OK；`cargo clippy --workspace --all-targets
  -D warnings` OK；`cargo test --workspace --no-fail-fast` **53 目标 0 失败**
  （run 1 仅 1 例 EADDRINUSE 环境抖动，run 2 全绿）。
- `cargo build --release`（rdb + rdb-bench）后 `run_all.sh` **4/4 PASS**。
- 180s soak+kill -9 PASS（见上）。

## 待用户决策
- **TODO 3 starrocks 抖动**：复现证据指向共享机器端口冲突类（非配置端口被
  bind 失败 + 200ms respawn 窗口内 early-exit），与 starrocks 场景 "rdb exited
  before RESP ready" 同类；二轮全绿。无产品缺陷证据，倾向登记为已知环境抖动。
- **TODO 5 生产客户端命令清单**：188 命令面 vs 真实用量对账，需业务侧输入。

## 第三轮整改（R3，lite 偏移缓存孤儿清扫与 FLUSHDB 竞态收口，2026-09-05）

### P0 lite 偏移缓存孤儿复活（closed）
- 现象：命令流之外删除 lite 流记录的家族清理路径（XIDLE active-expire 回收、
  读路径惰性 idle 清理、流键 DEL/EXPIRE）让内存 group-offset 缓存留脏；200ms
  回刷器（`drop_superseded` 只复查缓存自身 map，不看盘）把孤儿 group 记录写回
  已删家族，同名 `XGROUP CREATE` 永远 BUSYGROUP。
- 修复设计（`src/lite/mod.rs`、`src/lite/model.rs`、`src/ds/expire.rs`、
  `src/command/keys_core.rs`）：`Runtime::stream_reaped(prefix, stream)` 立即
  失效该流缓存偏移并入队延迟 latched 清扫（`reaps` 队列）；`lite::reap_stream`
  取流 latch（与回刷轮同推导：parent-slot prefix 下 `meta_key`），清缓存态并
  range-delete group 记录窗口（kind 0x0E），带陈旧守卫——仅当 meta 记录仍缺失
  或仍过期才删，绝不盲删，家族删除与清扫之间客户端重建流亦受保护；
  `lite::drain_reaps` 挂在每个 200ms 后台 tick 与每次 active-expire 采样轮后；
  `read_meta` 内惰性 idle 清理触发同一失效（读路径 `read_group` 设计上安全：
  仅在 offset-cache MISS 时运行，无脏态）；DEL/EXPIRE 流家族路径按设计防护调用
  `stream_reaped`（当前对流不可达——lite 记录位于 parent 派生 slot prefix 之下，
  但属廉价保险）。
- **FLUSHDB 竞态窗口（closed）**：FLUSHDB 先取脏流 latch 守卫（等出已过校验的
  在途回刷轮），执行 `clear_all` + 分块擦除，擦除后再做第二次 `clear_all`
  （与擦除并发的 XGROUP CREATE/XACK 会重标缓存；这些标记描述已擦除的流，不得
  回刷），最后放守卫。latch 排序与回刷轮一致——无 ABBA。
- 回归：新增 `tests/lite_reap_e2e.rs`（惰性 idle 清理失效缓存、清扫删除已写回
  的孤儿 group 记录、同名 XGROUP CREATE 成功）与 `src/ds/expire.rs` 单测
  `sampler_reap_of_stream_invalidates_offset_cache`；soak_kill9.sh 加固
  （pre-kill 写手存活 + ≥10 acked-INCR 门、post-respawn `GET bench_0` 持久化
  读回、respawn 换端口带重试循环）。
