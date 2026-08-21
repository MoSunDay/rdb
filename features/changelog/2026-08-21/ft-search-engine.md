Commit: ddd1bf3a92e1f01e4d5e68bbf979623adfd5c9f6（补记，代码先于此文档入库）

# FT.* 搜索内核——BM25 全文检索 + SQ8/SPANN 向量 ANN（Rust 独有）

## 背景
在 RESP2 + RocksDB typed-record 数据面上建搜索引擎（Go 归档实现无此能力）：
P1 全文检索（jieba/Unicode/bigram 分词 + BM25）、P2 向量检索（SQ8 量化 +
SPANN 式磁盘 ANN）。约束：纯 Redis `FT.*` 命令面、不做 ES 兼容层；
跨节点 = 客户端 fan-out（索引落在其键的 slot 上，非本节点回 MOVED）。

## 变更
### 搜索引擎（`rust/src/search/`，新增模块族）
- **`ft_cmd.rs`**（351 行）：FT.CREATE（SCHEMA，至多一个 VECTOR 字段）、
  FT.ADD（JSON 文档 add/replace，单次 fsync）、FT.DEL、FT.DROP/DROPINDEX、
  FT.BUILD（K/ITERS/SEED 重训重分区）、FT.INFO
- **`ft_search.rs`** + **`ft_query.rs`**：查询执行——BM25 文本（terms AND，
  Lucene idf，K1=1.2 B=0.75，docid 升序破平）+ KNN 探测重排（score=1/(1+L2)）；
  `@field:term` 前置于 KNN 做文本预过滤（精确向量，无 SQ8 误差）
- **`tokenize.rs`**：拉丁字母数字小写化；Han 段 jieba（内嵌词典 + HMM），
  词典未命中回退重叠 bigram；索引/查询同源分词
- **`index_codec/`**：search 记录族编解码（`mod.rs` 344 行 + `posting.rs`）
- **`ft_index.rs`**：增删的批构建；**`ft_build.rs`**：k-means 重训
- **`ann/`**：k-means、分区、KNN（未训练时退化为精确暴力）
- **`quant.rs`**：SQ8 逐维 min/scale 标定（字段全局）
- **`vecmath.rs`**：cosine 等向量数学——VSIM 复用同一实现
  （`rust/src/command/vectorset_sim.rs:20` 改为转发，行为不变）

### 数据面接线
- **`rust/src/ds/codec.rs`**：kinds `0x13`–`0x18`（meta/doc/posting/termstat/
  ann_centroid/ann_posting）+ `SEARCH_FAMILY` 单族（索引键 TTL 一并清整族）；
  `META_KINDS` 收录 `KIND_SEARCH_META`；codec_tests 的 unassigned 上界移至 0x19
- **`rust/src/ds/mod.rs`**：`TYPE` 对六个 kind 应答 `search`
- **`rust/src/command/mod.rs`**：8 个 FT.* 派发（索引键 = argv[1]，走默认
  slot 路由/keyspec）
- **`rust/Cargo.toml`**：`jieba-rs = "0.10"`（内嵌词典）

## 测试覆盖
| 功能 | 测试名 | 文件 |
|------|--------|------|
| 文本 BM25 排序/中文命中/替换/删除/DROP | `ft_text_lifecycle_and_ranking` | rust/tests/search_e2e.rs |
| KNN 暴力→SPANN 构建→预过滤（recall@k = 1.0） | `ft_knn_bruteforce_build_and_prefilter` | rust/tests/search_e2e.rs |
| 索引键 TTL 整族清理（docs+postings+termstat+ann） | `ft_ttl_expires_whole_family` | rust/tests/search_e2e.rs |
| 分词/BM25/编解码/量化/kmeans 单测 23 个 | `search::*::tests` | rust/src/search/ |

- 全量回归：`cargo test --workspace --no-fail-fast`（隔离 worktree @ ddd1bf3）→
  31 套件全绿、0 失败
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → exit 0；
  `cargo fmt --check` → exit 0
- 行数：最大 `ft_cmd.rs` 351 ≤ 400，全部新文件达标

## Impact Surface
- 新增可感知命令面：8 个 `FT.*`（Rust 独有，Go 归档无对应）
- VSIM 行为不变（cosine 实现移位复用）
- 不影响：既有七数据家族命令、RESP 兼容语义、集群路由（FT.* 走 argv[1] 键路由）
- kind 空间占用 `0x13`–`0x18`：外部依赖原始 kind 字节面的工具需知悉

## Related Docs
- [rust/COMPAT.md「Full-text + vector search」](../../../rust/COMPAT.md)
- [agents/rust](../../../agents/rust/index.md)
