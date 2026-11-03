# 03 · 性能技法研究：SQLite（turso fork）→ rdb SQL 引擎

- 日期：2026-11-02 · 状态：proposed（供 04-perf-batches 拆批引用）
- 参考（只读）：`link_repos/turso/libsql-sqlite3/src/`（下称 "SQLite"，行号以当前 checkout 为准）
- 核对对象：`src/sql/exec/scan.rs`、`select.rs`、`relation.rs`、`plan/mod.rs`、`storage/row.rs`、`index/{keys,mod}.rs`、`store/{ops,rocksdb}.rs`、`bench/`
- 纪律：本文只做研究与方案，不改代码。

## 1. SQLite 技术清单

### 1.1 索引/前缀 seek

| 项 | 机制（一句话） | 源码定位 | 借鉴要点 |
|---|---|---|---|
| seek 指令族 | WHERE 范围谓词编译成游标 seek（`>`→`OP_SeekGE` 等）而非逐行过滤 | `wherecode.c` `sqlite3WhereCodeOneLoopStart`：`aMoveOp[]`（TK_GT/LE/LT/GE→`OP_SeekGT/SeekLE/SeekLT/SeekGE`，wherecode.c:1711）；组合表 `aStartOp[]`/`aEndOp[]`（start=SeekGE/SeekLE，end=IdxGE/GT/LE/LT，wherecode.c:1826） | "谓词→seek 边界"是一等公民；rdb 应把 sargable 谓词变成迭代器边界而非读后过滤 |
| 前缀比较 | seek 键只比索引记录的前 nField 列，比完即停 | `vdbe.c` `case OP_SeekGE/OP_SeekGT`（vdbe.c:4847）：`UnpackedRecord r` 栈上构造，`r.nField = pOp->p4.i`（vdbe.c:4955）、`r.default_rc` 按 opcode 取 ±1（vdbe.c:4963），调 `sqlite3BtreeIndexMoveto`（vdbe.c:4987）；比较核心 `vdbeaux.c` `sqlite3VdbeRecordCompareWithSkip`（vdbeaux.c:4734）：一边字段耗尽返回 `default_rc`（vdbeaux.c:4403） | 复合前缀（如 `a=? AND b>?`）一次 seek 到位；`default_rc` 决定"前缀相等算命中还是越界"，这是语义正确的关键 |
| b-tree 定位 | 页内二分 + RecordCompare 回调，不物化整键 | `btree.c` `sqlite3BtreeIndexMoveto`（btree.c:5960）、`btreeMoveto`（btree.c:860，`UnpackedRecord *pIdxKey`）、页内 cell 比较走 `xRecordCompare`（btree.c:5890 一带） | seek 是比较函数而非序列化键；rdb 的 `encode_key` 顺序保持编码天然支持同款"构造下界键 + 顺序比较" |

### 1.2 top-N（ORDER BY ... LIMIT n）

SQLite **没有堆式 top-N**，用的是"sorter 容量封顶 + 当场淘汰"：

- 机制：sorter（临时有序结构）里最多保留 LIMIT+OFFSET 条；每来一条候选，`OP_IfNotZero(iLimit)` 判断已满 → `OP_Last` 取当前最大 → `OP_IdxLE` 比较候选与最大值 → 更小则 `OP_Delete` 淘汰最大再插入。注释原文："there are never more than LIMIT+OFFSET items in the sorter"。
- 源码：`select.c` `pushOntoSorter`（select.c:697），剪枝块 select.c:799–816；配套 `OP_ResetSorter`（select.c:792）。溢出（结果集 > 内存）由 PMA 溢写处理：`vdbesort.c` `mxPmaSize`（vdbesort.c:319、1008）。
- 借鉴要点：① LIMIT 存在时排序内存占用从 O(N) 降到 O(limit+offset)；② "取当前最大 + 比较 + 淘汰"在 Rust 里用 `BinaryHeap<Reverse<..>>` 容量 k 即可等价实现，且比 SQLite 方案更简单（堆顶即最大，无需 b-tree Last）。

### 1.3 join：嵌套循环 + 自动索引 + Bloom filter

SQLite join 本体只有嵌套循环（每个 FROM 项一个 WhereLevel 循环），性能靠两件配套：

1. **auto-index（自动临时索引）**：内表无索引但外层连接列有等值谓词时，查询内构建 transient 索引再走 seek。
   - 触发条件：`where.c` `whereLoopAddBtree`（where.c:3928–3937）：非 OR 子句、`SQLITE_AutoIndex` 开、无 `INDEXED BY`/`NOT INDEXED`、有 rowid、非相关子查询、非递归 CTE、非 RIGHT JOIN 右表。
   - 成本模型：`rSetup = rLogSize + rSize (+28 LogEst ≈ 常数×7)`，`nOut = 43`（≈20 行/次探测），wsFlags=`WHERE_AUTO_INDEX`（where.c:3940–3960）。
   - 构建：`where.c` `constructAutomaticIndex`（where.c:976），索引名 `"auto-index"`（where.c:1092）；**必须覆盖**（把查询需要的其余列补进索引，where.c:1128–1145），`OP_OpenAutoindex` + 填充循环。
2. **Bloom filter（3.38+）**：auto-index 上先过位图再 b-tree seek。
   - `vdbe.c` `OP_FilterAdd`（vdbe.c:9025）/`OP_Filter`（vdbe.key… vdbe.c:9061）：位图存在一个 MEM_Blob 寄存器里，`h %= n*8` 置位/探测；**只准假阳性跳过、不准漏**（false negative 只是多算，false positive 会错，注释明确）。
   - `where.c`：仅当 auto-index 键列含数值亲和时启用（文本列哈希全同，where.c:1116–1122）；分配 `OP_Blob 10000` 字节（where.c:1158）；总开关 `OptimizationEnabled(db, SQLITE_BloomFilter)`（where.c:1155）。

借鉴要点：rdb 无需照搬 auto-index（rdb 两侧 join 输入已是内存 `Vec`），等价物是 **hash join**（等值键建 HashMap，探测 O(1)），成本判断可参考 SQLite 的 "构建代价 vs 探测次数×选择率" 权衡；Bloom 在 hash join 后价值有限（列 P3）。

### 1.4 覆盖索引 / index-only scan

- 机制：查询所需列全在索引内时不打开表游标，`OP_Column` 直接读索引 cursor 的 payload。
- 源码：`where.c` 设 `WHERE_IDX_ONLY`（where.c:4040、4059，auto-index 恒覆盖 where.c:1062）；打开游标时跳过表 `OP_OpenRead`（`wherecode.c` 仅开索引 cursor）；`vdbe.c` `OP_Column`（vdbe.c:2975）对索引 cursor 走 payload 解码路径。
- 借鉴要点：rdb 索引条目 value 已含 pk_key（secondary）或 key 即 value（unique），"只取 pk / 只数数"的查询可不回表。

### 1.5 其他值得抄的

| 项 | 机制 | 源码定位 | 借鉴要点 |
|---|---|---|---|
| 表达式常量折叠（run-once） | 常量子表达式在 VDBE init 阶段求值一次入寄存器，重复引用直接复用；`pConstExpr` 列表按表达式结构去重 | `expr.c` `sqlite3ExprCodeRunJustOnce`（expr.c:5542，`OP_Once` 包裹 + `u.iConstExprReg` 复用）；触发点 `sqlite3ExprCodeTarget` 中 `sqlite3ExprIsConstantNotJoin`（expr.c:5113） | rdb 每行重 eval 整棵表达式树（`exec/expr.rs::eval`）；至少把纯常量子树（字面量运算、无行引用的函数）在 `execute_query` 入口折叠成 `Expr::Lit` |
| 寄存器复用 | 临时寄存器池分配/释放，避免反复扩张 | `expr.c` `sqlite3GetTempReg`（expr.c:7204）、`sqlite3ReleaseTempReg`（expr.c:7215）、`sqlite3GetTempRange`（expr.c:7227） | rdb 侧对应物：order-key/投影中间 `Vec<Value>` 复用缓冲；优先级低 |
| prepared stmt 计划缓存 | 编译产物（VDBE 程序）挂在 `sqlite3_stmt` 上复用，仅 schema cookie 变化时 `SQLITE_SCHEMA` 重编 | `prepare.c` `sqlite3Reprepare`（prepare.c:693）+ 重试循环（prepare.c:871） | rdb `front/shim.rs` 已按 stmt id 缓存 AST（shim.rs:243/259/296），但 COM_QUERY 路径每次全量 parse + `session_funcs::substitute` 重写（exec/mod.rs:157）；P2 可加 "SQL 文本→AST" LRU（schema epoch 失效） |

## 2. rdb 现状核对

### 2.1 SeqScan：确认"全库 walk"，且无上界、逐 key 解析

- 入口：`visible_rows` → `visible_rows_between`（scan.rs:210/222）→ 核心 `visible_versions_between`（scan.rs:246）。
- 遍历方式：`ops::for_each_from(store, b"0/", false, ...)`（scan.rs:259）——**单个前向迭代器从 `"0/"` 起直扫到 keyspace 末尾**，无上界、无提前终止；每个 key 过 `row::parse_version_key`（row.rs:168）解析后按 `lo<=slot<=hi && table_id==schema.id && ts<=read_ts && !is_prepared` 数值过滤（scan.rs:260–273）。
- 即：扫一张 1 万行的表也要走完整个 DB 的 key（含 RESP 数据、0x21/0x22 索引条目、0x23 segment meta），并对每个 key 做字符串→数字解析。成本 O(全库 key 数)。
- 物理 key（row.rs:7–11、134–141）：`version_key = slot_prefix(slot) ++ 0x20 ++ table_id(u32 BE) ++ pk_key ++ (!ts).to_be_bytes()`；`slot_prefix(slot)` = 十进制 ASCII + `"/"`（rocksdb.rs:300）；`slot_of = crc16(table_id BE ++ pk_key) % 16384`（row.rs:68）。
- scan.rs:264–266 注释如实记录："Slot keys are decimal strings, so byte order is NOT slot order（`"1000/" < "999/"`）: no prefix seek is possible"——该注释只对"单个连续前缀"成立；**同一 (slot, table_id) 的行版本是连续区间**，`slot_table_prefix(slot, table_id)`（row.rs:144）就是现成的区间下界（目前仅测试使用）。一张表 = 16384 个互不相交的连续区间。
- 快照合并：per-pk 取最新版用全局 `BTreeMap<Vec<u8>, (u64, Vec<u8>)>`（scan.rs:271）——即便不需要 pk 序也要付 BTreeMap 代价。

### 2.2 排序：确认全物化 + 全排序，无 top-N

`select.rs::execute_query`（select.rs:164）：把全部（组/行）先物化成 `pairs: Vec<(order_vals, projected)>`（select.rs:214–230），`sort_pairs` 全量 `sort_by`（select.rs:603，调用点 232），然后才 `skip(offset).take(limit)`（select.rs:236–246）。`ORDER BY ... LIMIT 1` 在 100 万行表上 = 物化 100 万对 `Vec<Value>` + 全排序。UPDATE/DELETE 复用的 `order_rows`（select.rs:644）同病。

### 2.3 join：确认纯嵌套循环

`scan.rs::join_sources`（scan.rs:411）：两侧先各自**全物化**（`materialize`，scan.rs:333），然后对 `(l, r)` 全组合逐对 eval ON 条件（cross join 无条件全保留）；`USING` 逐列解析成 offset 对比较。无 hash join、无 auto-index、无 bloom、无谓词下推进内表扫描。

### 2.4 索引与 planner：确认 ">1000 pk 弃索引"，且 **pk 点查无快速路径**

- 索引 key 布局（index/keys.rs:13–17）：secondary `"{slot}/" 0x21 table_id col_pos(u32 BE) key_encode(value) pk_key -> b""`；unique `0x22 ... -> pk_key`；`index_slot = crc16(table_id ++ col_pos) % 16384`（keys.rs:28）——**一个索引整体落在单个 slot 的连续区间**，eq/range 走 `for_each_from` + `starts_with` 前缀停（index/mod.rs:160–168、178 起）。布局本身是好的。
- planner（plan/mod.rs:57）：只认 `col=lit` / `col IN(lits)` / `col BETWEEN`（plan/mod.rs:100–130），先把谓词**全量解析成 pk 列表**（`resolve`，BTreeSet 去重排序，plan/mod.rs:197），`pks.len() > MAX_INDEX_PKS(=1000)`（plan/mod.rs:25）或解析失败 → `Path::SeqScan`（plan/mod.rs:80–84）→ 回到 2.1 的全库 walk。执行侧 `rows_by_pks`（scan.rs:308）对每个 pk 一次独立 seek（`visible_row_at_pk`，index/mod.rs:237）。
- **新发现（brief 未列）**：`schema.indexes` 只含显式 CREATE INDEX / 表内 inline 索引（ddl.rs:63、248），pk 本身无隐式索引；planner 只查列上的索引定义。因此 **`WHERE pk = ?` 在没有给 pk 建索引的表上同样退化为全库 walk**（无 `version_prefix` 点查路径；该路径只有 IndexLookup 回表和 write.rs 在用，write.rs:187）。这对 oltp 点查是致命项，应并入 P1。

### 2.5 执行/解析管线

`exec/mod.rs::execute`（exec/mod.rs:147）每次执行都跑 `session_funcs::substitute` + 完整执行管线；表达式求值是每行重入的树遍历（`exec/expr.rs::eval`），无常量折叠。prepared statement（shim.rs:243）缓存的是 AST，重复 execute 免 parse 但不免逐行树遍历。

## 3. 差距矩阵（核心产出）

| # | 优化项 | SQLite 借鉴点（定位） | rdb 改造点（文件/函数） | 预期收益 | 优先级 |
|---|---|---|---|---|---|
| 1 | 表前缀 seek（SeqScan → 表区间扫描） | `OP_SeekGE` + `UnpackedRecord` 前缀比较（vdbe.c:4847/4955，vdbeaux.c:4734）；谓词变 seek 边界（wherecode.c:1711） | `scan.rs::visible_versions_between`（scan.rs:246）：`for_each_from(b"0/")` 改为按 `row::slot_table_prefix(slot, table_id)`（row.rs:144）逐 slot 区间扫描；详见 §4 | 扫描成本 O(全库 key)→O(表 key + 16384 次 seek)；多表 DB 中小表查询从"秒级"回到"表大小级"；集群 band 版本同步受益 | **P1** |
| 2 | pk 谓词点查 | rowid 表 cursor 整数直达（vdbe.c:4847 表分支 `sqlite3BtreeTableMoveto`） | `plan/mod.rs::plan`：识别"谓词列 ∈ schema.pk"→ 新 `Path::PkLookup`，encode 后走 `index::visible_row_at_pk`（index/mod.rs:237）；IN/BETWEEN 同理多键 | `WHERE pk=?` 从全库 walk → 1 次 seek；oltp 点查数量级提升 | **P1**（与 #1 同批，复用同一编码） |
| 3 | top-N 排序 | `pushOntoSorter` LIMIT 剪枝（select.c:799–816）：容量封顶 + 淘汰当前最大 | `select.rs::execute_query`：有 LIMIT 时用容量 `offset+limit` 的 `BinaryHeap<Reverse<(order_vals, row)>>` 取 top-N，替代 pairs 全物化 + `sort_pairs` 全排序（select.rs:214–246）；`order_rows`（select.rs:644）同改 | 内存 O(N)→O(k)，CPU O(N·logN)→O(N·logk)；KNN/分页类查询受益最直接 | **P1** |
| 4 | hash join（等值） | SQLite 无 hash join，等价物为 auto-index（where.c:3928 成本门限 + constructAutomaticIndex:976） | `scan.rs::join_sources`（scan.rs:411）：ON/USING 拆合取式，识别完全等值合取 → 小侧建 `HashMap<Vec<Value>, Vec<row>>`（NULL 不入表），大侧探测；无等值或含 OR 时回退嵌套循环；OUTER 在探测未命中时 null-extend | 等值 join 从 O(N×M) 比较降到 O(N+M)；大表 join 从不可用变可用 | **P1** |
| 5 | 覆盖索引 / index-only | `WHERE_IDX_ONLY`（where.c:4040）+ `OP_Column` 读索引 cursor（vdbe.c:2975） | `plan/mod.rs`：查询列 ⊆ {索引列, pk} 时直接遍历索引条目返回（`index::lookup_range` 已会走条目，index/mod.rs:178），跳过 `rows_by_pks` 回表；`SELECT pk ... WHERE idx_col OP ?` 与 `COUNT` 先行 | 免 per-pk seek 回表；索引扫描型查询 CPU/IO 减半量级 | P2 |
| 6 | >1000 pk 阈值修复（IndexLookup 可扩展） | 无对应限制：SQLite 按成本而非硬阈值切换路径（where.c:3940 成本模型） | `plan/mod.rs::resolve`（plan/mod.rs:197）：pk 集改为流式（不先 BTreeSet 全物化）；超阈值时回退目标从 SeqScan 改为 #1 的表区间扫描；`rows_by_pks`（scan.rs:308）按 slot 分桶合并 seek | 低选择度索引（低基数列、大 IN）不再从"索引"掉回"全库 walk"；阈值语义从悬崖变坡道 | P2（依赖 #1） |
| 7 | 表达式常量折叠 | `sqlite3ExprCodeRunJustOnce`（expr.c:5542）+ `pConstExpr` 去重（expr.c:5542 起） | `exec/expr.rs`：新增纯函数 `fold_constants(Expr) -> Expr`（无列引用子树求值一次坍缩为 `Expr::Lit`），在 `execute_query`/`materialize` 入口调用 | 每行省掉重复的常量子树求值；对 WHERE 里日期/字符串函数最有效 | P2 |
| 8 | COM_QUERY 解析缓存 | stmt 缓存 + 仅 schema 变化重编（prepare.c:693、871） | `exec/mod.rs::execute` 或 front 层：SQL 文本 → AST 的 LRU（以 catalog epoch 失效），shim.rs prepared 已有 AST 缓存可复用结构 | 短查询热点路径省 parse；配合 #7 收益叠加 | P2 |
| 9 | LIKE 前缀走索引 | `aMoveOp` 范围映射（wherecode.c:1711）：`LIKE 'abc%'` → `[abc, abd)` 区间 | `plan/mod.rs::sargable`（plan/mod.rs:100）加 `Expr::Like` 前缀字面量分支 → `lookup_range` | 前缀匹配不再全扫 | P3 |
| 10 | Bloom filter | `OP_Filter/FilterAdd`（vdbe.c:9025/9061）、仅数值列（where.c:1116） | 暂无明确落点（#4 hash join 后内表探测已 O(1)）；若未来做 #2/#5 的集群版（先问远端"存在性"再 gather）可引入 | 边际 | P3 观望 |

优先级依据 brief 预期：表前缀 seek > top-N > hash join 为 P1 三件套；#2 与 #1 共享 key 编码且工作量小，并入 P1 批次；其余 P2/P3。

## 4. P1-1 表前缀 seek 具体方案

### 4.1 物理 key 形状（已读 storage 源码确认）

```
key   = slot_prefix(slot) ++ 0x20 ++ table_id(u32 BE) ++ pk_key ++ (!ts)(u64 BE)
slot  = crc16(table_id BE ++ pk_key) % 16384          (row.rs:68)
prefix= slot_table_prefix(slot, table_id)             (row.rs:144，现成)
      = decimal(slot) ++ '/' ++ 0x20 ++ table_id BE
```

推论：**一张表在单个 slot 内的全部行版本构成一个连续 keyspace 区间**（区间内先按 pk_key 再按倒序 ts 排列）；表全集 = 16384 个互不相交区间；同一 pk 的所有版本必在同区间且相邻（GC 的 pk-group 假设，gc.rs 模块注释）。

### 4.2 scan 改法

- 对 `slot in lo..=hi`（本机单表即 0..=16383）：`from = slot_table_prefix(slot, table_id)`，`ops::for_each_from(store, &from, false, ...)`，回调里 `if !key.starts_with(&from) { return false; }` 即停——与 `index::lookup_pks`（index/mod.rs:160–168）完全同款模式，无需新原语。
- **上界界定**：不设显式上界，靠 `starts_with` 失配提前停（迭代器有序，失配后不可能再命中）。若想 pin RocksDB 上界省扫描，注意 `iterate_upper_bound` 是**独占**的（ops.rs:267–277 的 `for_each_down_from` 注释已实测踩坑记录；后继键技巧 `upper = prefix ++ 0x00`），收益有限不推荐首版做。
- **免全局 BTreeMap**：区间内同一 pk 的版本天然相邻且新→旧，遇到第一条 `ts<=read_ts && !is_prepared` 即为可见版（scan.rs:271 的 or_insert 语义可局部化），pk 切换即放弃旧组 → 可边扫边产出，峰值内存从"整表 raw 版本"降到"当前 pk 组"。
- **pk 全序契约**：`visible_rows*` 文档承诺 "ordered by pk bytes"（scan.rs:206–209），而 pk→slot 是哈希，逐 slot 扫描打乱全局 pk 序。两个选项：
  - a) **k 路归并**（推荐）：16384 个区间的游标进 `BinaryHeap`（按 pk_key 序归并），保持对外契约不变，堆内存 O(16384)；
  - b) 放宽契约：下游 `execute_query` 无 ORDER BY 时仍会全排序、`order_rows` 也重排，但 GROUP BY 输出序（first appearance）、DISTINCT、既有测试依赖确定序——b 需要逐处审计，首版取 a。
- **迭代器成本注意**：store 未配置 `prefix_extractor`/prefix bloom（rocksdb.rs:41–58 仅整键 bloom 10bit、`block_based=false`），所以每个 slot 区间是一次普通 seek（memtable+各级 SST 二分），无前缀过滤加速；16384 次 seek 对小表可能反不如全库 walk——**加大小自适应**：先探测（或用索引计数/统计）估计表规模，低于阈值回退旧 `for_each_from(b"0/")` 路径。同时 `for_each_from` 每次新建 `DBIterator`（ops.rs:235），16384 次迭代器构造是主要固定开销，基准测试须覆盖"小表 + 大库"场景验证阈值。
- **错误语义**：沿用 `for_each_from` 的错误契约（迭代错误显式 `Err`，不得 `map_while(Result::ok)` 静默截断，ops.rs:226–234 注释）。
- **集群 band**：`visible_rows_between(lo, hi)` 直接 `for slot in lo..=hi`，scan.rs:264–266 的"十进制 slot 不可前缀 seek"注释随之作废（band 不再需要数值过滤，逐 slot 即天然分片）；`band_rows`（scan.rs:291）签名不变。
- 落点文件：`src/sql/exec/scan.rs`（`visible_versions_between` 重写 + 自适应阈值常量）、`src/sql/storage/row.rs`（`slot_table_prefix` 转正式 API，挪出测试专用）；`slot_table_prefix` 已存在，无需改 rocksdb 层。

### 4.3 验收口径

- 单表扫描输出与旧实现逐字节一致（同 read_ts 下 pk 全序、同可见版本）；
- 基准：N 表共 M 行的库中，单表 SELECT 扫描耗时随 N 表数增长基本不变（旧实现线性涨）；
- `scan_tests.rs`、`row_pk_tests.rs`、dist gather 相关 e2e 全绿。

## 5. bench 建议（供另一实现任务照做）

### 5.1 现状（已读 bench/Cargo.toml 与 bench/src）

- `rdb-bench`：依赖极简（仅 tokio），RESP 压测器；一连接=一 tokio task，pipeline 批次为延迟样本；`cli.rs::Workload`（cli.rs:4 起）：`ping/set/get/mixed/xadd/xreadgroup/xack/kafka-prod/kafka-fetch`；kafka 客户端是**手写 wire 协议**（kafka_wire.rs，模块注释 "hand-rolled Produce v2 / Fetch v4 client"）；无任何 mysql workload，无 mysql 依赖。

### 5.2 mysql workload 设计

- 传输：新增 `bench/src/mysql.rs`（会话握手 + COM_QUERY / COM_STMT_PREPARE / COM_STMT_EXECUTE 最小客户端），沿用 kafka_wire 手写协议先例、坚持零重依赖；`Workload` 增加 `mysql-*` 家族与 `--mysql-db`、`--rows`（预填充行数）、`--key-range` 参数。预填充（CREATE TABLE + 批量 INSERT）在压测开始前完成一次。
- 指标沿用 stats.rs：QPS + per-batch RTT 的 p50/p95/p99；同一 workload 与既有 RESP workload 可比。
- 四组（每组固定 pipeline=1 与 8 两种档）：

| workload | 查询形状 | 验证的优化项 |
|---|---|---|
| `mysql-point` | `SELECT c1,c2,c3 FROM t WHERE pk=?`（pk 随机）；对照组：给 pk 建 secondary index 后同查询 | #2 pk 点查（现状：无索引即全库 walk——正是要修的悬崖）；基准意义最大 |
| `mysql-range` | `SELECT pk,c1 FROM t WHERE c_indexed BETWEEN ? AND ?`（跨度 ~1% 行）+ `... LIMIT 20` | #1/#5/#6：索引范围 + 回表 vs 覆盖 |
| `mysql-join` | `SELECT a.pk,b.pk FROM a JOIN b ON a.k=b.k`（a 1k 行 × b 10k 行，k 基数 1k） | #4 hash join（现状嵌套循环 10^7 次比较） |
| `mysql-knn-scan` | `SELECT pk FROM t ORDER BY ABS(pk-?) LIMIT 10`（全表扫描 + top-N；rdb 尚无向量类型，用距离表达式模拟 KNN 形状） | #3 top-N + #1（扫描边界） |

- 建议每组同时记录服务端 `rdb_command_latency`（若 SQL 面已有 metrics hook）以便归因；数据集：1 表大库（验证 #1 自适应阈值）与 N 表中库（验证隔离性）两个 profile。

## 6. 不抄清单与风险

- **auto-index 照搬**：rdb join 输入已在内存，transient 持久索引无意义，取其"成本门限"思想即可（#4 的启发式）。
- **sorter PMA 溢写**（vdbesort.c）：rdb top-N 落地后内存已 O(k)，溢写属过度设计，P3 以下不排。
- **default_rc/前缀比较语义**：#1 落地时 rdb 用"下界键 + starts_with"而非 UnpackedRecord 回调，语义等价但少了"部分前缀命中"能力——若未来 #5 需要 `a=? AND b>?` 复合索引 seek 再引入比较函数式接口。
- 行数纪律：§4 涉及改动集中在 scan.rs（现 590 行），重写 `visible_versions_between` 预计净增 ~80 行，仍在 800 行红线内；若含 k 路归并则把归并器拆到新子模块（如 `exec/scan_merge.rs`，<200 行）。
