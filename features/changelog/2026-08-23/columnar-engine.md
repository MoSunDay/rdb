Commit: (working-tree, 随本提交入库)

# 列存表引擎（CREATE TABLE ... ENGINE=columnar）

## 背景
SQL 数据面（`src/sql/`）原仅有行存（MVCC 行版本 + RocksDB）。本次按五个里程碑
落地追加式列存引擎：段文件格式（M1）→ 写路径（M2）→ 读路径（M3）→ DROP 清理
与 e2e（M4）→ 后台清扫（M5）。零新增依赖（CRC-32/编码自写，footer 用 serde_json）。

## 变更
### 段文件与元数据（`src/sql/columnar/`）
- 段文件 `<store_path>/<bind>/columnar/t<table>-s<seg>.col`：
  MAGIC | 列页（PLAIN/字符串 DICT，比率 0.7 门控，≤8192 值/页，页级 zonemap）
  | footer JSON | footer_len | crc32（`format.rs`/`encode.rs`/`decode.rs`）。
- 段元数据为 RocksDB 新 kind `0x23`，键 `[0x23] ++ table_id BE ++ segment_id BE`
  （无 slot 前缀：控制面数据不参与槽路由，重建为单次前缀扫描），值为
  `SegmentMeta` JSON（state=prepared/live、commit_ts、页索引）。
- `Registry`：内存段索引，进程级缓存按 `(store_path, bind)` 键控，首用自
  RocksDB 重建；2PC decide(commit) 翻 prepared→live 后入册。

### 写路径（append-only，segment-per-commit）
- `CREATE TABLE ... ENGINE=columnar`（sqlparser ENGINE 选项）；仅允许 INSERT，
  UPDATE/DELETE/索引报 ER_NOT_SUPPORTED_YET。
- 会话内暂存 `Txn.appends`，COMMIT 时每表冲刷为一个不可变段：`.tmp`→fsync→
  rename，段元数据与行写同一原子 WriteBatch 发布；autocommit INSERT 即单段提交。
- 2PC：参与者 PREPARE 批含 Prepared 段（文件先于标记落盘），DECIDE(commit)
  翻 live；abort 删元数据+文件。段只落在事务关闭节点，不走线协议。
- 限额 `columnar_flush_rows`（默认 65536）/`columnar_flush_bytes`（默认 64 MiB），
  按语句与事务-表累计双口径检查，超限报错不落盘（无溢出转储）。

### 读路径
- 本地扫描：`commit_ts ≤ read_ts` 的 live 段按 (commit_ts, segment_id) 序解码，
  打开事务叠合自暂存追加；prepared 段永不入册即永不可见。
- 集群读：段按提交位置分散，读向**所有**节点扇出（新 `ScanColumnar` RPC），
  任一节点不可达整查报错；EXPLAIN 显示 `Gather(columnar, nodes=N)`。

### 清理与运维
- DROP TABLE：raft 目录删除后单批清 0x23 元数据、清注册表、删文件。
- 后台清扫（`gc.rs`，30s 轮询，主监听器侧）：以库内元数据为准回收悬空段
  （表已删/不可解码/无在疑标记的陈旧 prepared）与孤儿文件（`.tmp` 与 1h 以上
  无引用 `.col`），并幂等自愈注册表。

## 兼容性 / 已知边界
- 旧 raft 目录 JSON 无 engine 字段 → `#[serde(default)]` 解码为行存。
- 列存读可见性沿用 ts 块授权的本地知识语义：新提交可能在其他节点的
  autocommit 读点推进后才有可见性（与行存同语义，COMPAT.md 已注记）。
- 列存数据不参与 slot 迁移（段留在提交节点）。
- 测试：+41（848 全绿），含 `tests/columnar_e2e.rs`（单机 + 3 节点集群）。
