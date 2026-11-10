# Wukong-FT：位置化 posting 与双前端查询树

> 状态：landed（Batch 1 = WP1+WP2+WP5，Batch 2 = WP3/WP4+WP6，2026-11-10 全量落地；摘要 features/changelog/2026-11-09/wukong-ft-batch{1,2}.md）
> 日期：2026-11-09
> 关联：[01-wp1-postings.md](./01-wp1-postings.md)、[02-wp2-query-tree.md](./02-wp2-query-tree.md)、
> [05-wp5-es-dsl.md](./05-wp5-es-dsl.md)、[06-wp6-interop-matrix.md](./06-wp6-interop-matrix.md)

## 背景与差距矩阵

`src/es/`（ES HTTP/JSON 前端）与 `src/search/`（FT.* 内核）共用同一 search kernel
（一个索引 = 一个 user key = 一个 slot，`tests/es_search_e2e.rs:resp_es_interop_one_kernel`
已有互操作证明）。真正的差距在**查询表达力**：

| 能力 | 内核现状 | ES 前端现状 | 缺口 |
| --- | --- | --- | --- |
| 位置信息（短语） | posting 只有 (docid, tf) | `match_phrase` 不支持 | posting 需带 token 位置 |
| 前缀查询 | 无 | `prefix` 不支持 | posting key 前缀迭代 |
| 查询树（OR/NOT） | 词项列表，仅隐式 AND | `bool.minimum_should_match` 忽略 | 纯 enum 查询树 + 求值器 |
| query_string | 无 | 不支持 | 复用 FT 查询解析器产同一棵树 |
| ES sort 数值字段 | 逐 hit 读 `_source` | 同左 | 切 doc-values（kind 0x19） |
| stopwords / BM25 可调 | 无 | 无 | WP3 |
| `@num:[min max]` / SORTBY / RETURN | 无 | n/a | WP4 |

## 工作包

- **WP1** 位置化 posting：kind `0x1A` v2 codec（docid/tf/positions），读路径 v1/v2
  双读、写路径迁移式覆写；`SEARCH_FAMILY` 上界收编 0x1A；`codec_tests` 未分配断言跟进。
- **WP2** 查询树：`QueryNode::{Term, Phrase, Prefix, And, Or, Not}` 纯 enum +
  `eval_tree.rs` 求值器；`term*` 走 posting key 前缀迭代（`for_each_from` + 上界）。
- **WP5** ES 协议对接：`match_phrase`→`Phrase`、`prefix`→`Prefix`、
  `minimum_should_match` 真实现、`term {"value":..}` 对象形式、`query_string` v1 子集
  （`field:term`、引号短语、AND/OR/NOT、尾 `*`）直接复用 FT 查询解析器产出同一棵
  `QueryNode`——「一个解析器、两条线协议」。
- **WP3/WP4**（Batch 2）stopwords + BM25 k1/b；`@num:[min max]` + SORTBY + RETURN。
- **WP6**（Batch 2）互操作矩阵扩容 + COMPAT/features 台账回写。

## 批次

- **Batch 1**（已落地 2026-11-10）：WP1 + WP2 + WP5 + 对应单测/e2e —— 直接补齐
  两条前端的短语/前缀/布尔表达力。摘要见
  [features/changelog/2026-11-09/wukong-ft-batch1.md](../../features/changelog/2026-11-09/wukong-ft-batch1.md)。
  落地说明：FT 查询的空白分隔子句由 v1 的事实并集修正为文档契约的严格 AND
  （COMPAT 原文即写 AND，属行为修正而非契约变更）。
- **Batch 2**（已落地 2026-11-10）：WP3（索引级停用词 + BM25 k1/b 旋钮，位置
  紧凑偏差入 COMPAT）+ WP4（`@num:[min max]` 数值区间 + SORTBY/RETURN + canonical
  scan_numvals 去重）+ WP6（`tests/es_bulk_interop_e2e.rs` 互操作矩阵：_bulk/混写/
  跨端数值区间/TTL 双前端视图；台账回写）。摘要见
  [features/changelog/2026-11-09/wukong-ft-batch2.md](../../features/changelog/2026-11-09/wukong-ft-batch2.md)。
  计划全量落地。

## P3 观望池（显式不做，触发条件再拾起）

`highlight` 已于 2026-11-10 出池落地（触发条件"positions 就绪"自 Batch 1
满足；见 features/changelog/2026-11-09/wukong-ft-p3-highlight.md）；
继续观望：`_msearch`、aggs、scroll、wildcard/fuzzy。
**显式不做**：HNSW/cosine、服务端分布式 merge、外置分词词典。

## 约束

- 纯函数风格（无 class）、新文件 <400 行、迭代中文件 <800 行；phrase/prefix 求值
  拆新文件 `eval_tree.rs`，ES 侧增量并入 `dsl.rs`/`eval.rs`（余量充足）。
