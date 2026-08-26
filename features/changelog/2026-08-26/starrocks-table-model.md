# StarRocks 表模型 DDL 兼容（PRIMARY KEY / DUPLICATE KEY / DISTRIBUTED BY）

## 背景
MySQL 兼容之外补齐 StarRocks 建表头兼容：`CREATE TABLE ... PRIMARY KEY(...) /
DUPLICATE KEY(...) ... DISTRIBUTED BY HASH(col) BUCKETS n` 直接可用。目标不是引入
新存储引擎，而是把两种表模型映射到现有引擎上：PK 模型 = 行存 upsert 表，
DUP 模型 = 列存 append-only 表；DISTRIBUTED BY 仅作为 schema 元数据记录
（物理放置仍是 Redis 式 crc16 slot 分片，见 COMPAT.md 偏差）。

## 实现（Wave A：解析 + schema）
- **预解析器** `sql/parse/starrocks.rs`（新增，≤400 行）：sqlparser 0.62 无 StarRocks
  文法，模型子句在 MySQL 解析前从原始文本中用 token 扫描摘出（处理注释/引号/转义/
  反引号），改写为 MySQL 可解析文本并随 AST 传递（`Statement::CreateTable.starrocks`）。
- **大声拒绝**（MySQL 1235，非静默丢弃）：PARTITION BY / PROPERTIES / ORDER BY /
  UNIQUE KEY(...) / DISTRIBUTED BY RANDOM；PK 模型 × ENGINE=columnar、DUP 模型 ×
  ENGINE=row/innodb 同样拒绝。多列 `PRIMARY KEY(a,b)` 沿用单列主键的既有文案
 （复合主键留给 Phase 4）。
- **schema**（`storage/schema.rs`）：新增 `KeyModel{MySql(默认),PrimaryKey,Duplicate}` 与
  `Distribution{columns,buckets}`；`TableSchema.key_model/distribution` 均
  `#[serde(default)]`——旧 catalog JSON 照常加载为 MySql/无。注意：节点间 catalog 形状
  因此变化，存在 StarRocks DDL 后**滚动升级不安全**，需同批升级（COMPAT.md 已记）。
- **DDL 校验**（`exec/ddl.rs build_schema`）：模型→引擎矩阵判定（PK 强制行存、DUP 强制
  列存）、BUCKETS≥1（Parse 1064）、分布列必须存在（BadField 1054）；DUP 表若无 MySQL
  主键，取 DUP KEY 首列做 schema pk 且**保持声明 NULL 性**（仅元数据，列存不去重）。

## 实现（Wave B：PK 模型 INSERT=UPSERT）
- 自动提交路径（`exec/write.rs insert`）：写批盖戳前用
  `index::visible_row_at_pk(store, schema, pk, now)` 回收每个 pk 的旧可见行，索引维护
  收到的是 replace（唯一索引项随值迁移）而非 blind insert——否则重插既会误报唯一冲突、
  又会泄漏陈旧索引项（write_tests 用"旧值迁移 + 二次回插"双向断言锁死该回归）。
- 显式事务路径零改动：staging 本就按 `(table_id, pk)` 覆盖写（last-write-wins），
  COMMIT 时由快照推导 replace（tx::session::commit_index_ops 既有逻辑）。
- UPDATE / DELETE 在行存上语义不变。

## 实现（Wave C：验证 + 文档）
- 单测：starrocks_tests 12 个（字节级透传、子句抽取、改写、字面量陷阱、各类拒绝）、
  ddl_starrocks_tests 5 个（模型落盘、矩阵、可空性、分布校验、catalog 往返）、
  write_tests 2 个（自动提交 upsert+唯一项迁移、事务 staging 合并提交）。
- e2e：`tests/starrocks_model_e2e.rs` —— 单机 PK upsert / DUP 同键追加 / 全部拒绝矩阵；
  3 节点集群 DDL 复制 + 每 node 分块插入 + 跨 owner 的整表 re-INSERT replace。
- 文档：COMPAT.md 新增 "StarRocks table-model DDL"；agents/rust/sql.md 模块地图更新。

## 已知限制（COMPAT.md 已记录）
1. **集群跨协调者的 replace 新近度**：2PC 只携带新版本（旧侧回收是协调者本地读），
   行正确性靠 MVCC ts 保证（每 pk 恰一行胜出），但"新值何时在所有 reader 上遮蔽旧值"
   受全局时序收敛影响——与既有跨节点 UPDATE 的 M2-era 注意事项同类。跨 owner replace
   的强一致次序与本地 unique 项迁移列为后续工作（复合主键 Phase 4 一并）。
2. DISTRIBUTED BY 不产生真实桶/再均衡；ROLLING 升级窗口见上文 catalog 兼容门。

## 验证
- cargo test --workspace 全绿（含新增 19 个单测 + 1 个双用例 e2e 编译入套件）。
- 四道闸：fmt / clippy -D warnings / test --workspace / release build 全过。
