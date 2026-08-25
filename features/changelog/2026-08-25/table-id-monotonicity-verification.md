Commit: (working-tree, 随本提交入库)

# 表 id 单调化定向验证：补齐 OLTP/OLAP 审计修复的证据缺口

## 背景
上线前 review 指出：`b8491d4` 的四项审计修复中，**DROP 表 id 单调不复用**
（孤儿数据别名风险）一项只有实现、没有定向测试——既有用例覆盖
CREATE/DROP 语义往返，但从未断言"drop 后 id 不回退"。本提交补上该证据
缺口，并用变异测试证明用例真能抓住回归。

## 变更
### 定向测试（不改产品代码）
- **`src/sql/exec/ddl.rs`**：新增 `table_ids_stay_monotone_across_drop_recreate`
  ——走真实 CREATE/DROP 路径：id 1 → drop → 同名重建得 **2**（不复用）→
  3 → drop → 4。同时记录一条已验证的语义细节：同名重建会以新 schema
  覆盖同名 key 下的旧墓碑（`dropped_ids == [3]` 而非 `[1,3]`）——安全，
  因为更大的 live id 已约束后续分配。
- **`src/sql/storage/catalog.rs`**：新增两例单测——
  `next_table_id_is_monotone_over_stub_kv_tombstones`（stub kv 视图：
  max(live, dropped)+1；旧 `""` 墓碑与非 catalog key 不参与；空 catalog → 1）、
  `dropped_ids_reads_the_fsm_live_kv_view`（FSM `live_kv` 即重启/真实节点
  读路径：墓碑可见、live schema 照常列出）。
- **变异验证**（一次性、未入库）：把 `alloc_table_id` 回退为修复前的
  live-max-only 行为，DDL 用例立即失败（`left: 1, right: 2`，即 id 复用
  复现）；还原后全绿——证明用例对回归敏感。

## 测试覆盖
| 功能 | 测试名 | 文件 |
|------|--------|------|
| DROP→重建 id 单调不复用（同名重建场景） | `table_ids_stay_monotone_across_drop_recreate` | `src/sql/exec/ddl.rs` |
| 墓碑 id 参与 id 分配（stub kv 视图） | `next_table_id_is_monotone_over_stub_kv_tombstones` | `src/sql/storage/catalog.rs` |
| 墓碑 id 参与 id 分配（FSM live_kv 视图） | `dropped_ids_reads_the_fsm_live_kv_view` | `src/sql/storage/catalog.rs` |
| 变异敏感度（回退 live-max 复现 id 复用） | 手工变异验证，未入库 | `src/sql/exec/ddl.rs` |

- 全量回归：`cargo test --workspace` → **872 passed / 0 failed**
  （lib 695 + main 4 + e2e 173）
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → 零告警
- 行数：ddl.rs 547 ≤ 800、catalog.rs 274 ≤ 800

## Impact Surface
- 纯测试补证，**不改任何产品行为**；不影响协议/存储格式/性能。
- 补齐 `b8491d4` 审计修复的证据链：表 id 单调化现有定向回归测试守卫。

## Related Docs
- [features/changelog/2026-08-24/oltp-olap-completeness-and-engine-gating.md](../2026-08-24/oltp-olap-completeness-and-engine-gating.md)
- [SQL 数据面](../sql-dataplane.md)
