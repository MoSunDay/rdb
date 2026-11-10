# Wukong-FT P3 出池：FT.SEARCH HIGHLIGHT（2026-11-10 落地）

P3 观望池条目 `highlight` 的触发条件（"positions 就绪后随时可加"）自 Batch 1
起满足，按池纪律出池收编。计划：[plans/2026-11-09-wukong-ft/](../../../plans/2026-11-09-wukong-ft/00-overview.md)。

## 实现

- 新模块 `src/search/highlight.rs`（223 行 + 198 行单测，纯函数）：
  - `token_spans`：`tokenize` 的 span 镜像（runs 拆分复用 tokenize.rs 的
    pub(crate) 内部件；拉丁极大字母数字子串、Han 按 jieba cut token 长度重建
    偏移，单字/相邻 bigram flush 语义逐条对齐）。**等价性金标准单测**：对 13
    个样本断言 span 切片小写化 == tokenize 输出，保证高亮的 token 恰是建过
    posting 的 token。
  - `highlight_terms`：查询树收集 Term/Phrase（Exact）/Prefix（词干），
    And/Or 并集，Not/NumRange 跳过，停用词滤除。
  - `highlight_doc`：仅 fields 内 string JSON 值，token 命中（相等/前缀）用
    TAGS 包裹（默认 `<b>`/`</b>`），从后往前 splice；非 string/缺字段/非
    object/无命中原样字节返回。
- `ft_query.rs`：`HIGHLIGHT [FIELDS n f...] [TAGS open close]` 解析（子尾可
  重复后者覆盖），与 NOCONTENT 双向互斥报错；与 KNN/SORTBY/RETURN/LIMIT/
  WITHSCORES 共存。
- `ft_search.rs`：meta 后展开默认 TEXT 字段集 + 收集高亮词；共享 reply 循环
  在 RETURN 投影之后接线（KNN/SORTBY 路径同循环自动生效）。

## 契约与偏差（COMPAT.ft.md 详录）

TEXT 字段/string 值才包裹；FIELDS 缺省全部 TEXT 字段、未知字段静默跳过；
大小写不敏感（经分词器）；SUMMARIZE 不实现；ES 前端 `highlight` 请求键按
既有未知键规则容忍忽略；重序列化键序 caveat 同 RETURN。

## 台账

- COMPAT.md FT 节整体迁出为 **COMPAT.ft.md**（2026-11-10，沿 COMPAT.sql.md
  先例；主文件 782→725 行释放余量），HIGHLIGHT 契约入新文件；ES 节对
  dialect-v2 的交叉引用改为指向 COMPAT.ft.md。
- plans/2026-11-09-wukong-ft/00-overview.md P3 池：highlight 出池（余
  `_msearch`/aggs/scroll/wildcard/fuzzy 继续观望）；plans/index.md 摘要同步。

## 测试

- `--lib` 1426 全绿（+12 highlight 单测：等价性 ×3、terms 收集 ×2、包裹 ×7）。
- e2e：新 `tests/ft_highlight_e2e.rs` 1 用例 9 组断言（默认/自定义标签/
  FIELDS 过滤/前缀/停用词/NOCONTENT 双序报错/RETURN 共存/分页/match-all
  原样含字面标签）；search_e2e 8、es_search_e2e 4 无回归。
