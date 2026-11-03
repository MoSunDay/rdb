# 02 — libsql 向量内部机制与 rdb SQL 向量索引映射（Phase 4 门禁结论）

- 状态：proposed（Phase 4 / ANN 索引动工前门禁文档）
- 日期：2026-11-02
- 关联：`00-turso-map.md`（架构地图）；SQL 语法/类型面归 `01`（待写）；落地目标为 V2
  阶段 `CREATE/DROP VECTOR INDEX` + 近似 KNN
- 参考源：`link_repos/turso/libsql-sqlite3/src/`（pin `d8bc862d`，只读），核心文件
  `vector.c` / `vectorInt.h` / `vectorfloat32.c` / `vectorIndex.c` / `vectorIndexInt.h` /
  `vectordiskann.c` / `vectorvtab.c`；rdb 侧 `src/search/`（`quant.rs` / `vecmath.rs` /
  `ann/` / `index_codec/`）

## 0. 结论速览（TL;DR）

1. libsql 的向量索引在本 pin 下**没有 chunk/centroid 化 shadow 表**：历史 chunk 方案已整体
   移除，唯一实现是 DiskANN（`vectordiskann.c`），shadow 表为 `<idx>_shadow`（每图节点一
   行，`data` BLOB 是定长邻接块），全局参数在 `libsql_vector_meta_shadow`。
2. libsql 的索引维护是**同事务同步**的：DML 的 `OP_IdxInsert/OP_IdxDelete` 在 VDBE 内联调
   `diskAnnInsert/diskAnnDelete`，shadow 写是普通 SQLite 表/blob 写，随外层事务原子提交/
   回滚——**不存在最终一致窗口，也没有后台构建线程**。
3. rdb 映射结论：复用 `src/search` 的 SQ8 + SPANN（k-means centroid + 分区 posting +
   SQ8 粗排 + 原始向量精确重排），SQL 侧新增独立 kind 族（不混入 `SEARCH_FAMILY`），
   写路径**同步小批量**（与行写同一 RocksDB batch，同 commit_ts），可见性靠
   **snapshot_ts 回表裁决**，不做 per-entry ts。
4. 推荐可见性方案（一句话）：**向量索引条目由 DML 提交批同步派生（与行版本同 batch、同
   commit_ts），KNN 查询对候选一律按 read_ts 回表取可见行版本并丢弃不可见项；旧快照下
   因条目已被后续 UPDATE 迁移而产生的召回损失被显式接受（召回可暂时低于精确扫描），
   且保留无 centroid 表/候选不足 k 时回退精确扫描的兜底。**

## 1. libsql 编码内部（`vector.c` + `vectorfloat32.c`）

### 1.1 F32_BLOB 内存/磁盘布局

- **无头部、无显式维度字段**。磁盘格式（`vectorInt.h` 顶部注释 + `vectorParseMeta`）：
  f32 blob = 裸的小端 IEEE-754 `f32` 数组 `[data[0] as f32] [data[1] as f32] ...`，
  **维度 = blob 长度 / 4**，由长度反推。
- 类型判别靠"奇偶长度 + 尾字节"约定：偶数长度 blob 默认按 f32 解析（`nBlobSize % 2 == 0`
  分支）；奇数长度 blob 的最后一字节是 `type` 标签（f64=2 / float1bit=3 / float8=4 /
  f16 / fb16），f32 显式带标签时要求去掉标签后长度 `% 4 == 0`，否则报
  `vector: float32 vector blob length must be divisible by 4 ...`。
- 序列化对称：`vectorSerializeToBlob`（`serializeF32` 逐字节小端写出）+
  `vectorSerializeMeta`（f32 不写任何尾字节；`vectorF32DeserializeFromBlob` 用
  `deserializeF32` 逐元素还原）。内存表示 `Vector { type, dims, data, flags }`，
  `dims` 为 `u32`，data 紧跟结构体之后单次分配（`vectorAlloc`）。

### 1.2 合法性校验

- 维度上限：`MAX_VECTOR_SZ = 65536`（`vectorInt.h`）。文本解析超限报
  `vector: max size exceeded`；blob 反推超限报 `vector: max size exceeded: %d > %d`
  （`detectBlobVectorParameters`）。
- 维度 0：`'[]'` 是**合法输入**（`vectorParseSqliteText` 的 empty-vector 分支），
  `dims = 0`，序列化为 `zeroblob(0)`（`vectorSerializeWithMeta` 的 `nBlobSize == 0`
  分支）。但**索引列不接受 0 维**：`vectorIdxParseColumnType` 要求
  `dimensions > 0`（`vector column must have non-zero dimension for index`），且
  `diskAnnInsert` 逐条校验 `dims != pIndex->nVectorDims` 即报
  `vector index(insert): dimensions are different`（类型不一致同理报
  `vector type differs from column type`）——即**错维度向量会使整条 DML 失败**。
- NaN/Inf：**全链路无校验**（`grep isnan` 仅命中 f16↔f32 转换的位处理）。blob 手写 NaN
  分量可入库、可进索引，距离计算直接传播 NaN（SQLite 惯例将 NaN double 表现为 NULL，
  libsql 不在 C 层特判）。

### 1.3 距离函数（`vectorfloat32.c`，f32 单遍累积）

```c
float vectorF32DistanceCos(...) {
  for(i = 0; i < dims; i++){ dot += e1[i]*e2[i]; norm1 += e1[i]*e1[i]; norm2 += e2[i]*e2[i]; }
  return 1.0 - (dot / sqrt(norm1 * norm2));
}
float vectorF32DistanceL2(...) { /* sqrt(sum((e1[i]-e2[i])^2)) */ }
```

- cosine 返回的是**距离** `1 - cos`（不是相似度），dot 与两个 norm 在同一循环累积。
- **零向量无特判**：`norm1 * norm2 == 0` 时 `0/0 = NaN`，函数返回 `1.0 - NaN = NaN`。
  对照：rdb `vecmath::cosine` 对零向量返回 `0.0`（`VSIM` 的 `(cos+1)/2 = 0.5` 语义）并
  `clamp(-1.0, 1.0)`；这是必须进 COMPAT 的语义差异。
- SQL 函数 `vector_distance_cos(X, Y)` / `vector_distance_l2(X, Y)`
  （`vectorDistanceFunc`）：两参必须同类型（`vectors must have the same type`）、同维度
  （`vectors must have the same length`）；L2 不支持 float1bit（1bit 走
  `vector1BitDistanceHamming`，挂在 cos 分发路径上）。
- 顺带（量化语义差异素材）：libsql `vector8` 是 **per-vector** 标量量化
  （`vectorConvertToF8`：`shift = min`、`alpha = (max-min)/255`、`clip(...)`），量化参数
  随每个 blob 自带；rdb SQ8 是 **per-field、per-dimension** 的全局 calibration
  （见 §3）。

## 2. libsql 向量索引（`vectorIndex.c` 为主）

### 2.0 事实修正：chunk 表不存在，DiskANN 是唯一实现

本 pin 代码中 `grep` 不到任何 `__libsql_vector_index_chunks` / `chunk_vector` /
`centroid` 字样；`docs/` 下也无 `vector-index.md`。旧 chunk/centroid 方案已被移除，
当前唯一索引类型是 `VECTOR_INDEX_TYPE_DISKANN = 1`（`vectorIndexInt.h`，参数
`'type=diskann'`）。下文以代码为准描述 DiskANN 方案；任务书中 chunk/shadow 表名按此修正。

### 2.1 存储模型

- **全局元数据表** `libsql_vector_meta_shadow(name TEXT PRIMARY KEY, metadata BLOB)
  WITHOUT ROWID`（`VECTOR_INDEX_GLOBAL_META_TABLE`，`initVectorIndexMetaTable`）。
  `metadata` 是二进制参数包（`VectorIdxParams`，参数 ID 表见 `vectorIndexInt.h`
  105–141：type/dims/metric/block_size/format/pruning_alpha/insertL/searchL/
  max_neighbors/compress_neighbors）。
- **每索引一张 shadow 表** `<index_name>_shadow`（`diskAnnCreateIndex`）：
  - rowid 表：`CREATE TABLE ... _shadow (rowid INTEGER PRIMARY KEY, index_key <列类型>,
    data BLOB, UNIQUE (index_key))`；
  - INTEGER PK / WITHOUT ROWID 表：`(index_key ..., data BLOB, PRIMARY KEY (index_key))`；
  - 另建 `<idx>_shadow_idx`（仅 key 列）加速随机行选择（`OFFSET ... OFFSET RANDOM()`
    技巧要跳叶子页，注释里说明了动机）。
- **每行 = 图的一个节点**，`data` 是定长 block（`nBlockSize`，下限 256B，上限
  `DISKANN_MAX_BLOCK_SZ = 128MB`）。V3 布局：
  `[node meta: u64 rowid + u64 pad][node vector][edge vectors ...][edge metas: u64 rowid +
  f32 distance ...]`（V1/V2 的 meta/edge 布局见 `nodeMetadataSize`/`VECTOR_FORMAT_*`
  注释）；**最大出边数不是参数而是倒推值**：
  `(block_size - node_overhead) / edge_overhead`（`nodeEdgesMaxCount`）。
- 节点向量可全精度；`compress_neighbors` 参数可把**边向量**换成低精度类型
  （1bit/f8/f16 等），`VectorPair { pNode, pEdge }` 双表示承载"精确/压缩"两套距离。
- **没有 centroid、没有 chunk、没有每 chunk 最大向量数**——分区概念不存在，结构就是
  一张单调图。

### 2.2 构建路径（CREATE / INSERT / 事务性）

- DDL 语法：`CREATE INDEX idx ON t(libsql_vector_idx(embedding, 'type=diskann',
  'metric=cosine', ...))`。marker 函数 `libsql_vector_idx` 本身是 no-op 直通
  （`sqlite3_result_value(context, argv[0])`），只作解析锚点；`vectorIndexCreate` 校验：
  恰好一个列表达式、禁止 COLLATE、列类型必须是 `FLOAT32(N)` 形态、单列主键
  （`unsupported for tables without ROWID and composite primary key`）。
- **CREATE 立即构建**：`build.c:4345` 钩子在普通索引列信息齐备后调用
  `vectorIndexCreate`，返回码驱动 SQLite 原生 **refill**——对既有行逐行重放
  `OP_IdxInsert` 进索引（与普通 `CREATE INDEX` 回填同机制）；例外：载入 dump 时 meta 表
  已有该索引参数 → 返回 `CREATE_OK_SKIP_REFILL`，跳过回填避免二次填充。
  **没有任何后台/异步构建。**
- 插入（每条 DML 同步）：`vdbe.c` 在 `OP_IdxInsert` 里识别 vector cursor
  （`vectorIndexInsert` → `diskAnnInsert`）：
  1. NULL 向量直接跳过（`vectorInRowAlloc` 对 `SQLITE_NULL` 返回空指针，insert 返回 OK）；
  2. `diskAnnSelectRandomShadowRow`（`LIMIT 1 OFFSET ABS(RANDOM()) % MAX(COUNT,1)`）
     选随机入口节点；空表则本行即首节点；
  3. 以 `insertL`（默认 70）候选集从入口做贪心搜索；
  4. 写入新 shadow 行（`data` 先 zeroblob 占位）并 `nodeBinInit` 初始化；
  5. 双向连边：visited 节点作为新节点候选邻居（容量满替换最差，`diskAnnReplaceEdgeIdx`），
     新节点同时插进各 visited 节点的邻接表；
  6. `pruningAlpha`（默认 1.2）剪边：`nodeToEdge > alpha * hintToEdge` 则删旧边。
- 事务性：所有 shadow 读写都是同连接上的普通 SQL / `sqlite3_blob` 增量 I/O
  （`BlobSpot` 复用 `sqlite3_blob_reopen`），**随外层事务原子提交或回滚**；索引重建/refill
  前用 `OP_OpenVectorIdx` 的 `OPFLAG_FORDELETE`（`build.c:3847`，REINDEX/回填路径）先
  `vectorIndexClear` 清空旧 shadow。一致性模型 = SQLite 本身，**不是最终一致**。

### 2.3 查询路径

- 查询**不走 MATCH**，也不被 planner 自动选用（merge `2f15a0e06`
  "hide-vector-index-for-sqlite-planner"）：唯一入口是 eponymous vtab
  `vector_top_k`（`vectorvtab.c`，隐藏列 `idx/vector/k`，输出列 `id`）：
  `SELECT rowid FROM vector_top_k('idx_name', vector(?), 10)`（索引名可带
  `schema.` 前缀；`k` 接受整数或无损浮点，负数报错；查询向量维度必须等于索引维度）。
- `diskAnnSearch`：随机入口 → `searchL`（默认 200）候选集的 beam search；候选节点被
  弹出访问时从其 block 读出节点向量。**重排是逐点内联的**：当
  `pNodeQuery != pEdgeQuery`（边向量被压缩）时，边上算的是压缩距离，节点访问时
  **用 block 内全精度向量重算精确距离**（代码注释 "distance ... is approximate and we
  must recalculate it"）——没有独立 rerank pass，也没有二次全库扫描。
- 结果确定性：等距候选按 rowid 升序 tie-break（`topCandidateInsertIdx`），使跨构建的
  输出顺序可复现。**输出只有 id（rowid 或复合键），不返回距离**。
- 对照 rdb：libsql 是"图导航 + 访问点精算"；rdb `ann::knn` 是"nprobe 分区粗排 shortlist
  （`max(4k, k+16)`）+ 原始 f64 精确重排"两阶段——rdb 的 finalist 语义更接近精确 top-k。

### 2.4 一致性取舍

- 可见性：索引更新与行写在同一事务提交，**读者看到的是强一致的图**（无滞后）；
  近似性只来自图结构与贪心插入（FreshDiskANN 类方案的固有 recall 上限）。
- DELETE：`diskAnnDelete` 找到节点行 → 遍历其出边、从每个邻居的邻接表里摘掉指向它的
  边 → 删节点行。图只存 forward 边，**允许残留 zombie edges**（指向已删节点），搜索遇
  `DISKANN_ROW_NOT_FOUND` 优雅跳过（代码 TODO 承认不修补图，因 READONLY/WRITABLE 两种
  blob 模式下修补有风险）。
- UPDATE = SQLite 标准路径 `OP_IdxDelete` + `OP_IdxInsert`（先删旧向量节点再插新）。

### 2.5 DiskANN 定位（一句话）

DiskANN 不是实验开关而是**当前唯一在维护的 vector index 实现**（`type` 参数只有
`diskann` 一个取值，默认构建包含，`SQLITE_OMIT_VECTOR` 可整体裁剪），但对 planner 隐藏、
只能经 `vector_top_k` 显式使用；`vectorvtab.c` 是旧 `vector_idx` vtab 的兼容薄包装。

## 3. rdb `src/search` 可复用面（公开 API 清单）

### 3.1 API（签名摘录，全部纯函数 + 显式 `Store`/`WriteBatch` 参数）

| 模块 | API | 说明 |
| --- | --- | --- |
| `vecmath.rs` | `dot/l2/norm/cosine(a,b: &[f64]) -> f64` | 唯一距离实现；零向量 cosine=0.0、clamp ±1 |
| `quant.rs` | `Calibration { min: Vec<f32>, scale: Vec<f32> }`；`fit(&[Vec<f64>], dim)` | per-field、per-dim 的 SQ8 标定，退化轴 scale 下限 1e-9 |
| | `encode(&cal, &[f64]) -> Vec<u8>` / `decode` | 量化 clamp [0,255]；dequant = `min + q*scale` |
| | `l2_dequant(&cal, q, query) -> f64` | 不解码直接算 SQ8 L2（粗排热点） |
| | `roundtrip_error(&cal, v)` | 量化误差评估（测试/自检） |
| `ann/kmeans.rs` | `train(&[Vec<f64>], dim, k, iters, seed) -> Vec<Vec<f32>>` | k-means++ 播种 + Lloyd，同 seed 确定性 |
| | `nearest(&[Vec<f64>], v) -> (usize, f64)` | L2 最近 centroid（粗排第一步） |
| `ann/mod.rs` | `SqEntry { docid: Vec<u8>, q: Vec<u8> }` | 量化 posting 条目（~1B/dim） |
| | `build_batch(&Store, prefix, index, &IndexMeta, k, iters, seed) -> Result<WriteBatch, String>` | 全量重训 + 重分区 + doc record 盖 centroid id，单批 fsync |
| | `knn(&Store, prefix, index, field, dim, query, k, nprobe) -> Result<Vec<(Vec<u8>, f64)>, String>` | 无表→`brute_force`（私有）兜底；SQ8 粗排 + 精确重排；NaN 用 `total_cmp` 定序并丢非有限分 |
| `ann/partition.rs` | `assign(...)` / `partition_add(batch, store, ..., cid, docid, vector)` / `partition_remove(...)` | 增量成员维护：整分区读-改-写 upsert/delete |
| | `docid_of_key(physical, prefix_len)` | doc record key → docid |
| `index_codec` | `CentroidTable { dim, centroids, min, scale, members }`；`encode/decode_centroids` | centroid 表 = k 个 f32LE 质心 + SQ8 标定 + 成员计数（members 为构建期快照） |
| | `centroid_key/ann_posting_key/ann_posting_range`、`doc_key/doc_range`、`DocRecord` | 物理 key 族与 doc 记录（`vector` 字段存原始 f64LE，重排源） |
| `bm25.rs` | `TopK::new(k)` / `push` / `finish` | 通用 top-k 堆（粗排/重排共用） |

### 3.2 物理 key（现状）

`{slot}/` + kind + index + suffix（`ds::codec` 派生 key）：`KIND_ANN_CENTROID = 0x17`
（单记录：质心+标定）、`KIND_ANN_POSTING = 0x18`（每 centroid 一记录：`dim ++ entries[
docid ++ dim SQ8 bytes]`，docid 有序）。二者属于 `SEARCH_FAMILY (0x13..=0x19)`，
随 FT 索引 TTL 整族清除——**SQL 向量索引绝不能复用这两个 kind**（见 §4.1）。

## 4. 映射方案（核心结论）

### 4.1 索引数据物理布局（建议）

- 沿用 SQL 二级索引的定位规则：`index_slot(table_id, col_pos)`（crc16 散列到 16384 槽，
  `sql/index/keys.rs`）保证同索引条目聚簇在一个 `{slot}/` 前缀内。
- 新 kind 族（避开 `SEARCH_FAMILY` 与既有 0x20–0x23）：
  - `KIND_SQL_VEC_META`（如 0x24）：索引参数记录（metric、dim、k、nprobe、构建水位、
    格式版本），编码可复用 `VectorIdxParams` 风格的 ID→值参数包；
  - `KIND_SQL_VEC_CENTROID`（如 0x25）：**直接复用 `encode_centroids(CentroidTable)`**
    的 value 编码（质心 f32LE + SQ8 标定 + members）；
  - `KIND_SQL_VEC_POSTING`（如 0x26）：每 centroid 一条，value 复用
    `partition::encode_partition` 形态，但把 `docid` 换成 **pk_key**（行主键编码字节，
    与 `row::version_prefix` 对齐），SQ8 bytes 不变。
- **可见性物理载体**：条目**不携带 per-entry commit_ts**（与现有 0x21/0x22 二级索引同
  模型：entry 是派生数据，无版本链）。分区 value 头部可加一个
  `last_commit_ts`（ informational，用于诊断与未来段跳过优化），V2 不参与过滤。
- catalog（`IndexDef`）扩展 vector 索引项（metric/dim/nprobe），DDL 原子性沿用现有
  "表 id 分配 + catalog 变更同一 raft 决议" 的先例；DROP 走 catalog tombstone +
  `sweep_start` 式 slot 范围删除 + GC 兜底（对齐 M4/M5 模式）。

### 4.2 写路径（取舍结论：同步小批量）

- **决策：同步**。DML 提交批内派生向量索引 ops，模式完全照抄
  `sql/index/maintain::batch_ops`：对每个行 `Transition{old, new}` 计算
  `partition_remove(old_cid, pk)` + `partition_add(new_cid, pk, new_vector)`，与行版本写
  进**同一个 RocksDB WriteBatch**（同 commit_ts 一次原子可见）。
- `partition_add/remove` 是整分区读-改-写：并发写同分区靠既有 slot latch 串行化
  （分区 key 落在 `index_slot` 单槽内）；2PC 场景下索引 ops 以 `WireOp`（键值对形状与
  `IndexOp` 同构，`dist/proto.rs`）随 participant 计划分发，commit 应用、abort 删除——
  与现有 "commit flips 0x02→0x01 + applies secondary index entries; abort deletes"
  （`COMPAT.sql.md`）完全同轨。**注意：索引槽与行槽可能不同，2PC 写集必须同时覆盖两者
  （现有 secondary index 已如此，直接复用）。**
- 为什么不异步后台构建：(a) 同批原子性免费获得，无最终一致窗口、无 ts 对齐难题；
  (b) SPANN posting 维护是"量化 + 一次读改写"，远廉于 libsql 在事务内维护 DiskANN 双向
  边与剪枝（libsql 已证明更贵的同步维护可接受）；(c) 后台任务需与 MVCC GC/watermark
  竞争快照，复杂度不成比例。`build_guard.rs` 留作 V2.1 大表异步重建的基础设施，V2 不用。
- `CREATE VECTOR INDEX` 立即回填（对齐 libsql refill 语义与 rdb
  `ddl_alter::backfill_index` 先例）：分批扫全表可见行 → `kmeans::train` + `quant::fit`
  → 写 centroid 表 + 各分区，分批限内存，最后 catalog 登记同 raft 决议生效。
- 风险控制：分区 value 尺寸 ≈ `dim × |partition|` 字节，k 与分区容量需给默认上限并在
  表值膨胀时触发重训/段合并（V2 先整值读改写，段化增量 append + merge 列为后续优化）。
- DML 侧校验：维度不匹配/非有限分量（NaN/Inf）**在语句层拒绝**（libsql 的 dims 校验
  对齐；NaN libsql 缺失校验，rdb 补上——`quant::encode` 遇 NaN 会静默落 0 字节，必须前置）。

### 4.3 查询路径

1. 读 `CentroidTable`（无表 → 与 `ann::knn` 同语义：**精确扫描兜底**）；
2. `nearest`/`vecmath::l2` 选 `nprobe` 个分区，`quant::l2_dequant` 粗排进
   `TopK::new(max(4k, k+16))` shortlist；
3. **精确重排 + 可见性裁决合一**：对每个候选 pk 用
   `row::visible_value(read_ts)` 取可见行版本（覆盖 0x02 prepared 头自动跳过），
   从行 value 解出向量列做 `vecmath` 精确距离；不可见/已删/缺列的候选**直接丢弃**；
4. 剩余不足 k 且启用 fallback 时回退精确扫描（阈值由 01/执行器定）；
5. 返回 id + 精确距离（比 libsql 多给距离，方便 `ORDER BY distance` 语义）。

### 4.4 与 libsql 的差异清单（COMPAT 台账素材）

| # | 维度 | libsql（pin d8bc862d） | rdb V2 计划 |
| --- | --- | --- | --- |
| 1 | DDL 语法 | `CREATE INDEX ... ON t(libsql_vector_idx(col,'type=diskann',...))` marker 函数式 | `CREATE/DROP VECTOR INDEX`（语法细节归 01） |
| 2 | 算法 | DiskANN 单调图（随机入口 + beam + alpha 剪边） | SPANN（k-means 分区 + SQ8 粗排 + 精确重排） |
| 3 | 量化 | `vector8` per-vector min/max 标量量化；边向量可压缩 | SQ8 per-field per-dim calibration（`quant.rs`） |
| 4 | 重排 | 无独立阶段：节点访问时用 block 内全精度向量逐点精算 | 两阶段：shortlist 粗排 + 原始向量精确重排 |
| 5 | 查询接口 | `vector_top_k` vtab，planner 隐藏，不返回距离 | KNN 语法/函数进执行器，返回 id+距离（planner 可见性归 01） |
| 6 | 一致性 | 同事务同步维护 shadow 表（强一致图） | 同批派生 ops（同 batch/commit_ts 原子），回表裁决可见性 |
| 7 | DELETE | 摘边 + 容忍 zombie edges（搜索时跳过） | 条目即时消失；旧快照召回损失显式接受 |
| 8 | 零向量 cosine | `1 - dot/(|a||b|)` → NaN（SQL 表现 NULL） | `vecmath::cosine` → 0.0 + clamp ±1 |
| 9 | 数值精度 | f32 单遍累积 | f64（`Vec<f64>` 全链） |
| 10 | NaN 策略 | 不校验，入库传播 | DML 入口拒绝非有限分量 |
| 11 | 维度上限 | `MAX_VECTOR_SZ = 65536`，0 维 blob 合法但索引拒绝 | dim 由 DDL 声明，上限进 01（建议对齐 65536） |
| 12 | 元数据 | `libsql_vector_meta_shadow` + 参数包 | catalog `IndexDef` + `KIND_SQL_VEC_META` |

## 5. 风险结论：ANN vs MVCC/2PC 可见性语义（最终决策）

**推荐方案**（§0 第 4 条的展开）：

- 索引条目 = 派生数据，**不带 per-entry ts**，与行写同 RocksDB batch、同 commit_ts 提交；
  2PC 下随 participant 的 `WireOp` 列表 commit 应用 / abort 删除。
- KNN 查询**永不返回对快照不可见的行**（硬保证）：重排阶段逐候选 `visible_value(read_ts)`
  裁决，天然覆盖 prepared(0x02) 版本、tombstone、并发未决事务。
- 唯一放宽的是**召回率**：旧快照下，行向量被后续事务 UPDATE 过 → 旧条目已迁走 → 该快照
  的 ANN 结果可能漏掉该行（召回暂时低于精确扫描）。这与 ANN 固有的近似性同量级，
  可接受且可文档化（进 `COMPAT.sql.md` 偏差条目）。
- 兜底：无 centroid 表（未建/重建中）→ 精确扫描（复用 `ann::knn` 的既有语义）；
  重排后不足 k → 可选精确补扫开关。
- **否决的替代案**：(a) per-entry commit_ts + 查询过滤 `ts <= snapshot_ts`——每条目 +8B
  且仍需回表取原始向量，双重成本无额外正确性收益；(b) 异步后台构建（最终一致）——引入
  滞后窗口与后台任务/MVCC GC 的 ts 竞争，语义更难解释，而同步维护的成本已被 libsql
  （更贵的图维护）证明可承受。

主要残余风险与对策：分区读-改-写的并发依赖 slot latch 与 2PC 写集覆盖索引槽（§4.2）；
大表 CREATE 延迟靠分批回填限内存；分区膨胀靠容量上限 + 重训触发。

## 6. 佐证路径（撰写时快照）

- libsql：`link_repos/turso/libsql-sqlite3/src/{vector.c, vectorInt.h, vectorfloat32.c,
  vectorIndex.c, vectorIndexInt.h, vectordiskann.c, vectorvtab.c, build.c, vdbe.c}`
  （`build.c:4345` create 钩子；`vdbe.c:4265/6651/6745` OP_OpenVectorIdx/IdxInsert/
  IdxDelete 的 vector 分支；`vectorIndex.c:766` 旧 meta 表读取兼容）；
  `libsql-sqlite3/benchmark/README.md`（`vector_top_k` 用例与 recall 基准口径）。
- rdb：`src/search/{quant.rs, vecmath.rs, ann/mod.rs, ann/kmeans.rs, ann/partition.rs,
  index_codec/mod.rs, bm25.rs, ft_build.rs}`；`src/sql/index/{keys.rs, maintain.rs}`；
  `src/sql/storage/{row.rs, codec.rs}`；`src/sql/tx/ts.rs`；`src/sql/dist/{twopc.rs,
  participant.rs, proto.rs}`；`src/sql/columnar/{mod.rs, commit.rs}`；`COMPAT.sql.md`。
