# c22ff37 之后遗留未决项收口（终态清零）

Commit: 本批次（26e79d0..最终提交）

## 背景

上线审计交付（c22ff37）后仍存 7 项未决（A-G）。本条目逐项闭环：完成并留证，或显式落盘关闭理由。

## 变更

1. **推送收口（A/T1）**：`c22ff37` 起推送至 origin/main；后续收口提交随批合入。
   连续快速推送曾触发 ci.yml `cancel-in-progress` 取消前一 run（预期语义，非缺陷），
   故本批提交攒批后单次推送。
2. **soak workflow 韧性（C/T2）**：`soak.yml` 三处微调——job timeout 60→90min
   （冷缓存 release build + 900s soak + 全量 e2e 可能超 60min，job 级超时不该成为
   杀死仍在推进的 soak 的因素）；工件上传 `if: failure() || cancelled()`（超时即
   cancel，恰是最需要诊断工件的时刻）；修正头注释"180s soak inside run_all.sh"
   的过时描述（run_all.sh 只 glob 4 个 `scenario_*.sh`，soak 由本 workflow 显式
   跑 `soak_kill9.sh`）。
3. **nightly 首跑（B/T3）**：workflow 已具备 `workflow_dispatch`，但本机无 GitHub
   API 凭证（仅 SSH push），无法手动触发；首跑落在 cron `17 3 * * *`。三项假设
   中：60→90min 预算已由 timeout 提升覆盖；p99 阈值以本地 soak 复核（见下）；
   runner 环境假设（ubuntu-latest 预装 python3/curl）以首跑日志为准。
4. **tokio test-util 迁移（E/T4）**：生产 `[dependencies]` 剥离 `"test-util"`，
   改入 `[dev-dependencies]`（特性统一使 test 构建仍含）。证据：
   `cargo tree -e normal,features -i tokio` 零 test-util；`state.rs` 三个
   `#[tokio::test(start_paused = true)]` 全绿。
5. **expire.rs 拆分（D/T5）**：802 行（超 800 红线）按职责拆入 `src/ds/expire/`：
   `mod.rs`（公共谓词/时间/batch 维护 + 再导出）、`lazy.rs`（惰性访问路径 + 脱离
   worker 的 revalidate 清除）、`active.rs`（旋转游标采样 + 自适应循环）、
   `tests/`（共享夹具 + lazy/active 分置）。纯搬移 + 可见性调整（`SCAN_LIMIT`/
   `revalidate_expire` 提为 `pub(super)`），外部 `expire::*` 调用面零改动；
   全部新文件 ≤253 行（红线 400）。
6. **clippy 1.98 收敛（CI 红根因，计划外发现）**：main 的 CI 已连续多轮红在
   clippy——CI 用最新 stable（1.98.1），本地 1.96.1 无新 lint。10 处机械等价
   修复：9× `chunks_exact(N)`→`as_chunks::<N>().0`（`store/rocksdb.rs`×2、
   `command/string.rs`、`command/vectorset_cmd.rs`、`ds/vectorset_ds.rs`、
   `search/ft_query.rs`、`search/index_codec/posting.rs`×2、
   `search/index_codec/mod.rs`），1× `Some(x).filter(|_| p)`→
   `(p).then_some(x)`（`sql/storage/row.rs` 解码守卫）。
7. **高负载 e2e stall 定论落盘（F/T6，关闭不改码）**：`sql_dist_read_e2e`/
   `auto_increment_e2e`/`migrate_task_list` 三次 raft-join stall 均发生于本机
   外部租户高负载（load 184）窗口，wall-clock 失真放大 join 超时；隔离/低负载
   重跑三次全绿；`tests/common` 等待已宽裕（RESP ready 90s、leader 120s、sql
   变体 30-60s）；CI runner 独占无此形态。结论：环境噪音，非代码缺陷，不再
   追加等待或重试逻辑（宽裕等待 + 重跑即现行缓解）。

## 验证

- 本地（隔离 worktree，避免外部租户对 `src/sql/storage` 的并发 WIP 污染）：
  `cargo fmt --check` / `cargo +1.98.1 clippy --workspace --all-targets -- -D warnings` /
  `cargo test --workspace` / `cargo build --release --workspace` 全绿；
  `find src -name '*.rs'` 无 >800 行文件。
- CI：最终批次单次推送触发 run 须绿（clippy 1.98 修复后）。
- nightly soak：等 cron 首跑（`17 3 * * *`），绿 = 收口终态；本地 900s soak
  复核 p99 阈值裕量。

## 运维注记

本机存在 `/root/opencode` 外部租户进程，会在工作区留下未提交 WIP（DECIMAL 支持等）
且阶段性破坏本地编译。本批所有验证在 `.verify-wt` 隔离 worktree（共享
`CARGO_TARGET_DIR`）完成；主工作区仅做文件级选择性提交，未触碰其 WIP。

## H. CI test-step hang — root cause fixed (post-closeout)

The first post-push CI run's `cargo test --workspace` step never
finished (last green baseline: 7 min; this run burned the whole
360-min job cap). Reproduced locally under the CI-shaped constraint
(`taskset -c 0-3` + `--test-threads=4`): `sql_dist_read_e2e::
for_update_vetoed_and_explain_degrades_on_indexed_table` hangs
forever with the leader node wedged — raft listener backlog full
(peers stuck in SYN-SENT), an established raft conn with 2.6 MB
unread, every RaftState accessor starved.

Root cause (gdb stacks): `exec::ddl::catalog_apply`/`catalog_txn`
(and `exec::sequence`'s AUTO_INCREMENT bump) held the
`shared.raft.write()` guard across `handle.block_on(txn.put(..))`,
i.e. across the raft commit await. `sql::tx::global::
fetch_serialized` additionally blocked on `raft.read()` while
holding `fetch_mux` — an ABBA inversion. With the guard never
released, the leader's raft/HTTP serve paths and ts refill all
starve on the same RwLock and the cluster freezes. Never seen
locally before because the race window needs the slow 4-core CI
timing.

Fix (queue-then-await, never a guard across an await):
- `sql/storage/catalog.rs`: `CatalogTxn` gains sync
  `queue_put/queue_drop/queue_put_kv` returning a `QueuedApply`
  (key, value, ticket) + `record`; the async put/drop/put_kv are
  gone.
- `sql/exec/ddl.rs`: both guard windows only `begin` + `decide` +
  `queue`; commits are awaited after the guard drops. Whole-DDL
  serialization (decide → queue → commit) moves to a process
  `DDL_MUX` tokio Mutex so `concurrent_create_tables_stay_isolated`
  (exactly one CREATE wins, loser gets 1050) keeps its ordering via
  the FSM `live_kv` view.
- `sql/exec/sequence.rs`: AUTO_INCREMENT bump queues under the
  guard, awaits the commit after it.
- `sql/tx/global.rs`: `fetch_serialized` snapshots `is_leader`
  BEFORE taking `fetch_mux`, removing the lock-order inversion.

Validation: hang test 8x green (~5 s each), race test 4x green,
full workspace suite under `taskset -c 0-3 --test-threads=4` green
(1151 tests; hung indefinitely before), fmt + clippy 1.98
`-D warnings` green.

## I. Post-fix red → green: follower-lag assert + CI telemetry (4c4a19a)

After the hang fix landed (8065e6e) the CI test step finished in 8
minutes but red — logs are 403 for anonymous readers, so the failing
test was unknown. Reproduced locally in a 3x full-suite loop on 4
pinned cores: `drill_py_scenario_again_three_real_processes` failed
1-in-3 with a nil bulk reply for `raft get rk1` on one node.

Root cause (test bug, not product): the drill asserted `raft get rk1`
on EVERY node immediately after `raft set` returned +OK. +OK means
quorum commit; a lagging follower can answer before applying the
entry. Same file already polls for exactly this in the depkey /
rejoinkey loops — the rk1 check was the odd one out.

Fix: poll with a 10 s deadline (process_cluster_e2e.rs), matching the
file's own convention. Post-fix: 10/10 drill-binary stress runs and
3/3 full-suite runs green locally; CI run 35160609179 test step green
(9m45s, first green test step since fca9112).

Also in this push window (933ba4d): ci.yml test step now (a) emits
failing test names as `::error` annotations — readable via the
anonymous check-runs API, since log downloads stay 403 — and (b) has
`timeout-minutes: 60` so any future hang fails fast instead of
burning the 360-min job cap.

Outstanding: the B-side failover flake (one failure in a 2x10
concurrent-binary stress loop, heavier than CI load, not reproducible
4/4 serial) — revisit only if CI annotations name it.

Closeout: run 35164744900 on fc5de4c (main) is the first full-green
CI since fca9112 (2026-08-20) — fmt/clippy/test/release/guards all
success, test step ~9m45s across three consecutive runs.
