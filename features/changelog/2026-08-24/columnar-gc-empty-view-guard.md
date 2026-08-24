Commit: (working-tree, 随本提交入库)

# 列存 GC 空视图守卫：重启后不再误删段元数据

## 背景
上线前深度审计发现：`sql::columnar::gc` 的清扫把 `lookup_raft` 的 `Ok(None)`
与 `Err` 一律当作"表已删除"判垃圾。节点重启后、raft catalog 视图尚未加载
（FSM 恢复慢 / 拓扑同步未到，`RaftState.kv` 每 3s 刷新）时视图为空，而磁盘
段元数据早于重启存在——首轮清扫（启动后 30s）会把**所有** 0x23 元数据当
垃圾批量删除：数据事实源丢失，孤儿年龄门槛到期后段文件随之被删，属条件性
数据丢失路径。必须在上线路径上堵住。

## 变更
- **`src/sql/columnar/gc.rs`**（`sweep_core`）：
  - 新增 `view_empty = catalog::list_tables_raft(raft).is_empty()` 守卫：
    视图为空时本轮保留全部可解码段元数据（下轮视图加载后重试），并在
    空视图且存在段元数据时打印 `[columnar-gc]` 提示（`gc.rs:83-90`
    注释、`gc.rs:147-152` 提示行）；
  - 表存在性判定改 `match`：`Ok(None)` 仅在视图非空时判垃圾；`Err`
    （catalog 条目不可读）按"未知而非已删"保留并打日志（`gc.rs:104-118`）；
  - 模块文档补充空视图/不可读条目语义。
- **`src/sql/storage/catalog.rs`**：新增 `list_tables_raft(&RwLock<RaftState>)`
  （GC 在 blocking pool 无 `Shared`），`list_tables` 变薄包装（`catalog.rs:88-114`）。
- **`COMPAT.md`**：列存节修正"planned M5"过时措辞为实装行为（含空视图
  守卫注记）；补记列存×slot 迁移交互（段不随槽迁移、无槽节点拒绝列存
  INSERT）；运行时验证测试数 123→862。

## 测试覆盖
| 功能 | 测试名 | 文件 |
|------|--------|------|
| 空视图保留全部段元数据（游离文件仍清） | `empty_catalog_view_keeps_every_meta` | `src/sql/columnar/tests_gc.rs` |
| 不可读 catalog 条目保留段元数据（Err 分支） | `unreadable_catalog_entry_keeps_metas` | `src/sql/columnar/tests_gc.rs` |
| DROP 清理仍生效（见证表保证视图非空） | `dropped_table_metas_and_files_swept` | `src/sql/columnar/tests_gc.rs` |

- 全量回归：`cargo test --workspace` → 864 passed / 0 failed（含新增 2 例）
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → 零告警
- 行数：`gc.rs` 239 <800；`tests_gc.rs` 284 ≤400；`catalog.rs` 151 <800

## Impact Surface
- 行为变化：重启后短暂窗口内 GC 不再回收任何段元数据（宁可滞漏、不可误删）；
  视图加载后下一轮恢复正常判定。全程仅新增日志，无协议/配置/接口变化。
- 已知遗留（审计记录、非阻塞）：全表扫描全量物化的内存上限、2PC 协调者
  二阶段错误吞没（`dist/twopc.rs`）、列存指标盲区（无 Prometheus 计数），
  另行排期。
