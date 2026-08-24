Commit: (working-tree, 随本提交入库)

# OLTP/OLAP 完备性修复与事务引擎门禁

## 背景
上线前 OLTP/OLAP 完备性审计发现三类语义漏洞：① 事务可混写行存与列存
（与「单事务只支持一种引擎类型」的产品约束不符）；② `SELECT ... FOR
UPDATE / FOR SHARE` 被静默降级为普通读，破坏 snapshot-isolation 语义
承诺；③ 会话级 `SET`（autocommit/isolation/NAMES 等）被当作 no-op
容忍，用户无从得知会话语义未生效。另有两处一致性问题：DROP 后表 id
可能被新表复用（孤儿数据别名风险）；非 DROP 执行节点上的行孤儿无
回收路径（列存已有 M5 GC，行存缺同构覆盖）。

## 变更
- **单事务单引擎门禁**（`src/sql/tx/session.rs`）：`Txn` 增 `mode` 字段，
  `check_engine` 拒绝跨引擎 staging，报 MySQL 1235 + "a transaction cannot
  mix row-store and columnar writes"；拒后事务仍可提交自身引擎侧已 staged
  写入（与 staged 语义一致）。事务内混合读取不受限。
- **锁读显式拒绝**（`src/sql/parse/query.rs`/`ast.rs`/`exec/select.rs`）：
  `SELECT ... FOR UPDATE / FOR SHARE` 返回 1235（不再是静默降级）；SET
  语句在 translate 层按白名单分诊：cosmetic 项（sql_mode/wait_timeout…）
  容忍忽略，语义项（autocommit/isolation/NAMES/TIME_ZONE/ROLE…）显式
  拒绝，消息由 `{set}` Display 构造（修复 "SET SET ..." 重复前缀）。
- **表 id 单调化**（`src/sql/storage/catalog.rs`、`src/sql/exec/ddl.rs`）：
  DROP 墓碑由 TableSchema JSON 改为裸十进制表 id（`CatalogMutation::Drop`
  携带 schema）；`lookup_raft` 三态：空串/可解析十进制 → `None`，合法
  JSON → `Some`，其余 → `Err`；`next_table_id` = max(live, dropped)+1，
  id 永不复用。
- **行孤儿 GC**（`src/sql/storage/gc.rs`）：每节点 GC 轮次由
  `catalog::dropped_ids` 派生墓碑 id 集，`fold_version`/`sweep` 对 dropped
  组整组删除（含 watermark 以上与 0x02 prepared），并同步清该表
  0x21/0x22 索引键（行键 0x20 全排在索引键前，不干扰版本 fold 游标）；
  与列存 M5 GC 同构，列存段未改。
- **2PC vote 批内防重**（`src/sql/dist/participant.rs`）：vacated/claimed
  状态在批内不重复计数。
- **COMPAT.md**：Access/Transactions/GC/列存 DROP 段同步上述行为；删
  "USE/SET no-op" 过时措辞。

## 测试覆盖
| 功能 | 测试名 | 文件 |
|------|--------|------|
| 混写事务提交前拒绝 + 拒后行侧仍可提交 | `mixed_engine_txn_is_rejected_before_commit` | `src/sql/tx/session_tests.rs` |
| 行/列各自独立事务正常持久化 | `row_and_columnar_txns_each_persist_their_own_engine` | `src/sql/tx/session_tests.rs` |
| SET 拒绝消息与容忍项 | 既有 session/translate 用例扩展 | `src/sql/tx/session_tests.rs` 等 |
| 锁读 FOR UPDATE/FOR SHARE 拒绝 | 既有 parse 用例扩展 | `src/sql/parse/` |
| dropped 行超 watermark 全删（含 live anchor） | `dropped_*` 4 例 | `src/sql/storage/tests_gc.rs` |
| dropped 表索引键被清 / 不影响他表 | `dropped_*` 4 例 | `src/sql/storage/tests_gc.rs` |
| 坏 catalog 条目保留段 meta（三态 Err 分支） | `unreadable_catalog_entry_keeps_metas` | `src/sql/columnar/tests_gc.rs` |
| 2PC 并发乱序 | 既有 gather 用例扩展 | `src/sql/dist/gather_tests.rs` |

- 全量回归：`cargo test --workspace` → **855 passed / 0 failed**
  （lib 692 + main 4 + e2e 159；仓库实际总数，非原预估 864）
- clippy：`cargo clippy --all-targets` → 0 告警；`cargo fmt --check` 干净
- 行数：session.rs <800、gc.rs <800、catalog.rs <800，其余新改文件 ≤400

## Impact Surface
- 行为变化（有意）：混写事务/锁读/语义 SET 从"静默容忍"改为显式 1235
  拒绝；DROP 表 id 永不复用；dropped 表行孤儿 ≤30s 被每节点 GC 回收。
- 兼容：数据格式无变化（墓碑值编码即表 id 十进制，与旧 "" 墓碑等价
  兼容）；协议/配置/接口无变化；非 leader 节点由 GC 而非 DROP 命令清理，
  语义与列存 M5 对齐。
- 已知遗留（审计记录、非阻塞）：2PC 协调者错误吞没（`dist/twopc.rs`）、
  无谓词下推/zonemap、双向消息无超时、SeqScan 无前缀 seek、列存段全表
  物化、列存指标盲区，另行排期。
