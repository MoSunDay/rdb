# Wukong-FT Batch 2：停用词/BM25 旋钮 + 数值范围/SORTBY/RETURN + 互操作矩阵（2026-11-10 落地）

计划：[plans/2026-11-09-wukong-ft/](../../plans/2026-11-09-wukong-ft/00-overview.md)
（WP3+WP4+WP6；Batch 1 见 [wukong-ft-batch1.md](./wukong-ft-batch1.md)）。至此计划全量落地。

## WP3 索引级停用词 + BM25 k1/b 可调

- `IndexMeta`（`src/search/index_codec/mod.rs`）追加 `stopwords/k1/b`，编码
  append-only：老 meta 解码得默认值（空表/1.2/0.75），新编码尾部带
  `n_stopwords ++ 条目 ++ k1/b f64LE`；手写 `Default`（f64 派生默认 0.0 不合语义）。
- `FT.CREATE <idx> [STOPWORDS n w...] [K1 f] [B f] SCHEMA ...`：选项位于 SCHEMA
  前、任意组合顺序；未知选项/词数不匹配/坏 f64 报错。FT.INFO 增报
  stopwords/k1/b（reply 14→20 项）。
- 索引侧在 `doc_record_of` 对 TEXT 字段过滤停用词，**位置在过滤后紧凑**
  （"the quick the fox" 去 "the" 后 quick/fox 相邻 → 短语 `"quick fox"` 命中；
  RediSearch 保留空洞 —— 偏差入 COMPAT）。KEYWORD 不滤。默认停用词表为空
  （RediSearch 内置 174 词英文表 —— 偏差入 COMPAT）；ES 建的索引恒为默认值。
- 查询侧在 `eval_term`/`eval_phrase` 内过滤（FT.SEARCH 与 ES
  match_phrase/prefix/query_string 经共享求值器自动覆盖；纯停用词条/短语零命中）；
  ES `match` 对分析后词项过滤；`term`/`terms`/`range` 精确值不过滤。
- `term_score` 增加 k1/b 尾参（`bm25::K1/B` 常量保留为默认值），调用方
  eval_tree ×3、es/eval ×2 全部改传 `ctx.meta.k1/b`。

## WP4 `@num:[min max]` + SORTBY + RETURN

- `QueryNode::NumRange{field,gte,lte}`（闭区间，`*`/`±inf` 开界；仅 scoped
  语法、两方言同语法；无 scope 的 `[1 5]` 仍是普通词；畸形区间
  `ERR bad numeric range`）。lex 对 scoped `[...]` 吞空白扫到 `]`。
- `eval_tree.rs` 新增 eval_numrange（常量分 1.0，与 ES NUMERIC range 对齐；
  非 NUMERIC 字段静默零命中 —— 偏差入 COMPAT），并落 canonical
  `pub fn scan_numvals(store,prefix,index,field,keep)`，`es/eval.rs` 与
  `es/exec.rs` 两份逐字重复的扫描实现删除改调它。
- `FT.SEARCH ... [SORTBY <f> [ASC|DESC]] [RETURN <n> <f...>]`：SORTBY 仅
  NUMERIC 字段（非数值字段报错）、与 KNN 双向互斥、全量命中先按（值,
  docid）排、缺值两方向都垫底、再 LIMIT，WITHSCORES 照报相关性分；RETURN
  投影按列表序保留存在键（缺键跳过、`RETURN 0` 输出 `{}`、重序列化键序可能
  与原文不同 —— 偏差入 COMPAT）。

## WP6 互操作矩阵 + 台账

- 新 e2e `tests/es_bulk_interop_e2e.rs`（es_search_e2e.rs 已近 800 行上限，
  按工程规则拆新文件）：ES `_bulk` ↔ FT.SEARCH 短语/前缀、四路混写
  （FT.ADD/ES PUT/_bulk/FT.ADD 交替 + 双向覆盖同 docid）计数与内容一致、
  WP4 跨前端（RESP `@price:[..] SORTBY ASC` / ES `query_string price:[..]`）、
  TTL 家族整删后 RESP `unknown index` 与 ES `404 index_not_found`/文档 404
  视图一致。
- 台账：COMPAT.md（FT 节命令行/停用词/旋钮/数值范围/SORTBY/RETURN 六处；
  ES 节停用词+共享数值范围一条；全文 782 行逼近 800 上限，后续 FT 契约
  增量建议评估拆分独立 COMPAT 文件）、features/es-front.md、本 changelog、
  plans 状态 → 已落地。

## 测试

- 单测 `--lib` 1414 全绿（新增：meta 尾部 round-trip/老格式默认、停用词
  求值短路、query_tree 数值区间 8 断言组）。
- e2e：search_e2e 8（+`ft_stopwords_and_bm25_knobs`、
  `ft_numeric_range_sortby_return`）、es_search_e2e 4（+停用词跨前端）、
  es_bulk_interop_e2e 1（新）。
- clippy `-D warnings`：src/search、src/es 零告警（src/sql/* 残留属并行
  会话 WIP，非本计划范围）。
