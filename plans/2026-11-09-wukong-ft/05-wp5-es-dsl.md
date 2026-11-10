# WP5：ES 协议面对接（DSL 子集追平内核）

> 状态：landed（2026-11-10，Batch 1）
> 日期：2026-11-09

## 增量

- `match_phrase`（`dsl.rs`→`Plan::MatchPhrase`）：内核 `QueryNode::Phrase`；
  `slop`/`analyzer` 容忍忽略。
- `prefix`：`{"f":"red"}` 与 `{"f":{"value":"red"}}` 两形式 → `QueryNode::Prefix`
  （value 即前缀，不带 `*`）。
- `query_string`：`{"query": "..."}` → 复用 `ft_query` 的 QueryString 方言解析器，
  直接产出 `QueryNode` 树（与 RESP 同一 AST、同一份 RocksDB 记录）。
- `bool.minimum_should_match`：整数或 `N%`（按 should 子句数四舍五入）；
  缺省 = 无 must/filter 且有 should 时 1，否则 0（ES 7 语义）；显式 msm>0 时
  should 变必选，命中数不足的 doc 剔除。负数/组合式（`2<-25%`）→ 400。
- `term` 对象形式 `{"field":{"value":..}}`（boost/_name 容忍）。
- ES `sort` 数值字段切 doc-values：优先读 kind 0x19 numval 记录，miss 回落
  `_source` 路径取值。

## 偏差（回写 COMPAT）

- CJK 前缀：`redi*` 干净单 token 才是 Prefix；多 token（分词产物）退化为 AND 词项。
- 短语在 v1 posting（无位置）上降级为 AND；keyword 字段短语同为 AND。
- msm 百分比取整策略为四舍五入。

## 测试

`dsl_tests` 解析单测；`eval_tests` 视需要；e2e 矩阵扩：match_phrase/prefix/
query_string/msm（整数/百分比/与 must 并存）/term 对象/数值 sort；
FT.ADD ↔ ES 短语/前缀、ES _bulk ↔ FT.SEARCH 短语/前缀互操作。
