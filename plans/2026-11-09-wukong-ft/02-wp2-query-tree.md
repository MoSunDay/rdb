# WP2：查询树 QueryNode + eval_tree

> 状态：landed（2026-11-10，Batch 1）
> 日期：2026-11-09

## 目标

把 `ft_query.rs` 的「词项列表」升级为纯 enum 查询树，一个解析器同时服务
RESP `FT.SEARCH` 与 ES `query_string`；求值器落在 `src/search/eval_tree.rs`。

## 设计

- `QueryNode::{Term, Phrase, Prefix, And, Or, Not}`（`ft_query.rs`）：
  - `Term { field: Option, term }`：posting 精确查找（field=None 遍历全部 TEXT 字段）；
  - `Phrase { field, terms }`：按位置链校验相邻（p, p+1, p+2 …）；任一 term 的
    positions 为空（v1 记录）时该 doc 降级为「全部 term 命中」；
  - `Prefix { field, prefix }`：posting key 前缀迭代（0x15 与 0x1A 两段范围，
    callback 解码 (field, term) 过滤前缀），逐 term BM25 求和；
  - `And`/`Or`：交集（分数求和）/并集（分数求和）；`Not`：doc 全集差集，常数 1 分。
- 方言：`Ft`（`@field:` 作用域、`-` 取反、`|` OR）与 `QueryString`（裸
  `field:term` 作用域、AND/OR/NOT 关键字、引号短语、尾 `*`）。两方言共用同一
  语法骨架；`And([])`/空树 = 恒不匹配（保持 v1 空查询 0 命中语义）。
- **行为修正（有意）**：v1 `text_search` 对多词查询实际是「并集」而非文档所写的
  AND；WP2 落地后 `FT.SEARCH` 多词为严格 AND（与 COMPAT 契约、KNN 预过滤一致）。
- `ft_search::text_search`/`matched_docids` 切到 `eval_node`。

## 测试

解析器单测（作用域/短语/前缀/OR/NOT/关键字/空查询）；位置链校验纯函数单测；
search e2e 增短语、前缀、`-` 排除、`|` 并集、严格 AND 断言。
