# 2026-11-10 · FT.*/ES 前置上线确认 P0-P2 修复批

## 背景

上线确认报告（read-only pass）判定：RESP FT.* 面条件性可上线，ES 面因
`_bulk` 空 `_id` 恒失败（高危）修复前不可上线。本批落报告中可代码化的
P0/P1/P2 项；容量评估与 reader-first 滚动演练为运维动作，不在本批。

## 修复清单

- **P0 `_bulk` 空 `_id`（`src/es/bulk.rs`）**：`!valid_name(&op.id, ..)` 对
  空 id 恒真，`{"index":{}}` 无 `_id` 的条目恒报 "invalid document id"，
  auto_id 分支为死代码。改为「空 id 放行到生成分支；在场 id 才做合法性校验」
  （`contains('/')` 冗余检查一并移除，`valid_name` 本就拒绝 `/` 与超长）。
  e2e `es_bulk_auto_id_items`：三种形态（裸 `index`/显式 `_index` 无 `_id`/
  `create` 带 `_id`）混批，断言回填 id 可 GET、可检索。
- **P0→P2 `_bulk` 项数帽**：`MAX_BULK_OPS = 100_000`（每项独立 latch+fsync，
  64 MiB body 帽只限字节不限项数），超限整批 400；纯函数 `check_cap` 单测覆盖。
- **P1 ES HTTP 传输护栏（`src/es/http.rs` + 新 `src/es/guard.rs`）**：
  活连接帽 `es_max_connections`（0=内置 4096，`ConnGuard` 镜像 kafka/rocksmq，
  帽满静默断连 + stderr 一行）；handler 执行 30s 查询超时（408
  `timeout_exception`；此前只有 head/body 读超时，查询可无限占用任务）；
  无 body 语义方法带 `Content-Length > 0` 从「静默吞 body」改为 400。
  `config/conf.yaml` 与 `features/es-front.md` 同步登记。
- **P1 KNN 预过滤吞错（`src/search/ft_search.rs`）**：`matched_docids` 的
  `unwrap_or_default()` 把 store/解码错误折叠成空预过滤集 —— 坏 posting =
  静默 0 命中。改为 `Result` 透传进既有 `append_error` 路径（与
  `text_search` 一致）。
- **P1 KEYWORD 大小写对齐（共享查询文法）**：索引侧 KEYWORD 按原样字节建
  posting、ES `term`/`match` 亦精确字节，但共享文法（FT.SEARCH 与 ES
  `query_string`）一律 tokenize 小写化 —— 混大小写 keyword 值两端静默零命中。
  `QueryNode::Term/Phrase/Prefix` 增携带 as-typed `raw` 字节，求值侧对
  KEYWORD scope 用 `raw` 精确查（短语 = 整值精确；前缀 = 原样 stem 扫描；
  停用词跳过改为 TEXT-only，keyword 值 "the" 合法）。停用词/TEXT 路径行为
  不变（小写 keyword 上 raw == tokenized）。e2e
  `keyword_exact_case_both_fronts` 锁双前端精确语义；COMPAT.ft.md 文法条目
  同步登记。

## 验证

`cargo test --lib` 1431 全绿（+5 新增）；`cargo test --test es`（含新
`es_bulk_auto_id_items`、`keyword_exact_case_both_fronts`）12 通过；
`cargo test --test search` 10 通过；`cargo clippy --all-targets -- -D warnings`
零告警；改动文件 rustfmt clean。注：tests/ 已按 2026-11-10 tests-reorg
收编为目录 crate，原 6 个 e2e 二进制现聚合为 `es`/`search` 两 target。

## 遗留（运维，非代码）

- 容量评估：全量物化查询（match_all/_count/sort、FT `*`/NOT 补集）内存
  O(索引规模)，放行前按目标 doc 数/dim/高 df 词分布定语料上限。
- 混版本滚动：posting v2 懒迁移要求 **reader 先于 writer 升级**
  （COMPAT.ft.md 既有登记）。
- 真实 Elasticsearch/RediSearch 客户端对拍仍缺（现全部为自家双前端 e2e）。

## 收尾（同日 review 闭环）

- **eval_phrase 全停用词 KEYWORD 边界（`src/search/eval_tree.rs`）**：停用词
  过滤后的空 `terms` 短路 `return` 原在字段循环之前，tokenized 词项全为
  停用词的短语（如 `@kw:"the of"` 对 keyword 值 `the of`）永远到不了 KEYWORD
  raw 精确分支。改为：短路降为 TEXT 路径的 per-field `continue`，KEYWORD
  分支置于循环首位 —— TEXT 语义不变（每个字段都跳过）。新增 store-backed
  单测 `all_stopword_phrase_matches_keyword_raw_bytes`
  （`src/search/eval_tree_tests.rs`，put_meta + build_add_batch 同步装置：
  命中 `raw` 一致 docid、错 `raw` 零命中）。
- **408 写语义登记**：`features/es-front.md` 传输护栏 + COMPAT.md ES 节
  Transport 条目各补一句 —— 写请求也可能撞 30s 超时；latch async RAII +
  单批 fsync 无后台线程，超时不留半批写；边界完成的写客户端视角仍可能
  收 408（写 >30s 属罕见）。
- 落库为三笔提交：`fix(es)` `_bulk` auto-id + 项数帽（P0）；`feat(es)` HTTP
  传输护栏（连接帽/30s 408/body-less 400）；`fix(search)` KEYWORD 精确字节
  文法对齐 + KNN 预过滤透传 + 停用词/keyword 短语边界（含本次收尾与文档）。
  验证同上文门禁：`--lib` 1432 全绿（+1）、`--test es` 12、`--test search`
  10、clippy `-D warnings` 零告警、rustfmt clean。
