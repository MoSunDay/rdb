# MQ 台账外盲点复核的缺陷清偿：DUMP/RESTORE/MIGRATE 丢延迟消息 + FLUSHDB 幽灵组

Commit：4c171b0（delay 折行）、5e36d5a（协调器逐出）
背景：2026-10-08 第二轮完备性复核（入池登记见同目录 `mq-p3-intake.md`）发现的
两条**缺陷级**项——已落地面与自身承诺不一致，按"不做 ≠ 不修 bug"口径走缺陷
流程清偿，不入池。

## 缺陷 1：DUMP/RESTORE/MIGRATE 丢 0x1D 延迟暂存行（4c171b0）
- 症状：带 outstanding 延迟消息的流经 DUMP→RESTORE 或 MIGRATE 搬运后，
  延迟消息**静默丢失**；删除路径（`expire::family_delete_entries`）折删 0x20
  账本 + 0x1D 暂存行，搬运路径（`dump_key`）只折 0x20——WP2 建立的族登记
  纪律漏了搬运这一侧，`features/mq-lite.md` P0 键族登记三连（XIDLE/RENAME/
  FLUSHDB）亦未覆盖 DUMP/MIGRATE。
- 修复：
  - `src/ds/dump.rs` `dump_key`：STREAM_FAMILY 分支镜像 `fold_delay_rows`
    的窗口扫描匹配逻辑但只收集不删除，Record body 同账本约定（去 slot 前缀）；
  - `src/ds/dump.rs` `restore_key`：0x1D 记录 due-major 非 data_key 形状，
    通用重定根无法解析——decode (due, stream, locked_id) 后以**目标流名**
    re-encode；同名恢复与 MIGRATE（同名换 slot）字节级恒等，改名恢复
    due/locked_id 保真、仅流名重定向；
  - `src/command/migrate/data.rs`：MIGRATE 源侧删除从 batch-only 的
    `clear_key_family` 改为镜像 `keys_core::delete_records`（Enveloped 臂传
    store 使 0x1D 折删生效）——运输侧现在携带延迟行，源侧折删删掉的是最后
    一份副本，不再泄漏也不复丢。
- e2e：新文件 `tests/lite_delay_migrate_e2e.rs`（3 用例：DUMP→RESTORE 同名 /
  改名 / MIGRATE 跨 slot；流名取父子 slot 碰撞对防串扰）。

## 缺陷 2：FLUSHDB 后 ListGroups 报幽灵组（5e36d5a）
- 症状：FLUSHDB 清库（0x20 账本行随数据同灭、lite offsets/owners 已清）但
  kafka 组协调器的内存 runtime 够不着——ListGroups（runtime ∪ ledger）继续
  应答已清库的组。
- 修复：
  - `src/kafka/coordinator/mod.rs`：进程级 `OnceLock<Weak<CoordRuntime>>`
    槽（`publish` / `evict_all_groups`；kafka front 未启用时 Weak 升级失败
    即 no-op）+ `clear_groups`（一次写锁 drain 全表，锁释放后逐组 notify，
    与 `sweep_once`/`remove_group` 同锁序；不动 notifies map 防搁浅旧 Arc
    等待者）；
  - `src/kafka/coordinator/join.rs`：`wait_state` 的 resolve 签名改
    `Fn(Option<&GroupState>)`——组条目整体消失时 parked 等待者立即回
    UNKNOWN_MEMBER_ID（原实现该分支根本不会被调用，`remove_group` 文档宣称
    的"等待者 re-check 后 fenced"至此成立）；deadline 兜底仍在；
  - `src/command/flushdb.rs`：lite 清理同批调 `evict_all_groups()`，不破坏
    顶部 latch 持有下的"组记录先落再被删"顺序论证。
- e2e：`tests/kafka_group_e2e.rs` 追加 `flushdb_evicts_runtime_groups_
  and_rejoin_rebuilds`（建组+commit → FLUSHDB → ListGroups 空、
  DescribeGroups=Dead → rejoin 重建 generation、commit/fetch 恢复）。

## 验证
- `cargo test`：`lite_delay_migrate_e2e` 3/3（含回退源改动后 3/3 失败的红绿
  验证）、`lite_delay_e2e` 9/9、`lite_delay_proc_e2e` 3/3、`migrate_e2e` 1/1、
  `kafka_group_e2e` 3/3（注释 evict 调用后新用例如预期失败）、
  `kafka_group_failover_e2e` 2/2；lib：`kafka::` 97、`ds::dump` 2、
  `command::migrate` 7、`ds::expire` 15 全绿；`cargo check --workspace` 干净。
- e2e 台账登记：`features/e2e-coverage.md`「MQ 缺陷修复（2026-10-08）」组两行。
