# Wukong-FT Batch 1：位置化 posting + 双前端查询树 + ES DSL 追平（2026-11-10 落地）

计划：[plans/2026-11-09-wukong-ft/](../../plans/2026-11-09-wukong-ft/00-overview.md)
（WP1+WP2+WP5；Batch 2 的 WP3/WP4/WP6 未启动）。

## WP1 位置化 posting（kind 0x1A v2）

- `src/ds/codec.rs`：`KIND_SEARCH_POSTING2 = 0x1A` 入 `SEARCH_FAMILY`
  （上界 0x13→0x1A，classify/TTL 家族随动）；`codec_tests` 补 0x1A Typed /
  0x1B raw 未分配断言。
- `src/search/index_codec/posting.rs`：`PostEntry.positions`（严格递增 token 序号）；
  v2 编码 = (docid, tf) + 每个 doc 一段 delta-LEB128（`pos[i]-pos[i-1]-1`）；
  `upsert_posting` 替换语义。
- 读路径双读（先 0x1A 后 0x15），写路径恒写 v2 并顺手删除 v1 孪生
  （touch 即迁移）；`ft_cmd` 的 add/del 批、`es/write.rs` 全量走新签名。

## WP2 查询树（QueryNode / eval_tree，两协议同一 AST）

- `src/search/query_tree.rs`：`QueryNode::{Term, Phrase, Prefix, And, Or, Not}` +
  `parse_query_tree`（方言只差 scope 语法：FT `@field:` vs ES 裸 `field:`（须含
  字母，`12:30` 仍是普通词）；引号短语、尾 `*` 前缀（多 token 词干退化为 AND、
  星号丢弃 —— 记偏差）、`|`/`OR` 并、`NOT`/前导 `-` 补（大小写敏感关键词）、
  `*` 单独 = 全量）。
- `src/search/eval_tree.rs`：树上求值（BM25/位置链 `verify_positions`/
  posting-key 前缀扫描/集合运算）；`ft_query`/`ft_search` 切换到该树。
- **行为修正**：空白分隔多子句由 v1 的事实**并集**修正为契约的严格 **AND**
  （COMPAT 原文即写 AND；KNN prefilter 早已是交集）。e2e 固化
  （`ft_dialect_v2_operators`）。

## WP5 ES DSL 对接

- `src/es/dsl.rs`：`match_phrase` / `prefix` / `query_string`（仅认 `query`，
  其余容忍忽略）/ `bool.minimum_should_match`（非负整数或 0-100%，负数与 Lucene
  组合式 400）/ `term {"value":..}` 对象形式。
- `src/es/eval.rs`：短语/前缀/query_string 经共享 `eval_tree` 求值；msm：纯
  should = 命中数门槛（默认 1 即并集，分数为命中 should 之和），有 must/filter
  时显式 msm>0 才把 should 升级为门槛（不满足者剔除，命中者仍加分）；
  百分比四舍五入、下限 1。
- `src/es/exec.rs`：数值字段 sort 走 0x19 列式 doc-values（未映射字段回退
  `_source` JSON）；`sort` 收 `{"field":"desc"}` 简写（`opts.rs`）。

## 测试与台账

- 单测：`codec`（54）、`dsl`（13）、`query_tree`（6）、`eval_tree`（4）全绿；
  e2e：`search_e2e`（6，新增 `ft_dialect_v2_operators`）、`es_search_e2e`
  （3，`dsl_matrix` 扩 v2 断言 + `dsl_v2_phrase_prefix_qs_msm` +
  `resp_es_interop_one_kernel` 扩 FT↔ES 短语/前缀互操作）。
- 台账：COMPAT.md（FT 家族 0x13–0x1A、方言 v2 与严格 AND 修正说明；ES 前端
  新子句与偏差条目）、features/es-front.md DSL 子集同步、
  plans/2026-11-09-wukong-ft/ 状态 → 部分落地。
