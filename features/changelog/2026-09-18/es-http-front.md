# ES HTTP 前置：内核字段扩展 + HTTP/JSON DSL 面 + 接线与进程级 e2e

Commit: HEAD（本批为 src/es/ 全量 + 进程接线；检索内核字段扩展与前置分层于
A/B/C 阶段先行合入，本条收口 D 阶段）

## 变更
- **内核（A 阶段）**：FT.* 索引元数据扩出 keyword/numeric/vector 字段类型与
  numval 编码（`src/search/ft_cmd.rs`、`index_codec/`），RESP FT.* 行为不变。
- **前置（B/C 阶段）**：`src/es/` —— 手写 HTTP/1.1 传输（每连接一请求、
  可选 Bearer）、路由表、mappings JSON↔内核 schema、文档写路径（单次 fsync、
  索引 key latch、非本节点 slot 直接 400 `routing_exception`）、`_search` DSL
  解析/求值/执行（match/term/terms/range/bool/knn/sort/from+size/_source）、
  `_bulk` NDJSON、`_count`/`_refresh`/`_cat`；38 个模块单测。
- **接线（D 阶段，本批）**：`es_bind`/`es_token` 配置键（`src/conf.rs`）+
  `do_main` 监听块（`src/main.rs`，镜像 mysql/kafka 前置模式）；`config/*.yaml`
  四份样例追加注释化默认关闭键；`COMPAT.md` 新增
  "Elasticsearch-compatible frontend (Rust-only)" 偏差节。
- **e2e**：`tests/common/mod.rs` 增 `ProcNode.es` + `spawn_node_es`（沿用
  spawn_node_sql 的"写完基础 yaml 再追加键"模式）；新增 `tests/es_e2e.rs`
  （生命周期/文档 CRUD/_bulk/错误路径）与 `tests/es_search_e2e.rs`
  （DSL 矩阵 + **RESP FT.\* ↔ ES HTTP 共用同一存储层的互操作证明**）。

## 验证
`cargo build` 绿；`cargo test --lib "es::"` 38 过；`--test es_e2e` 3 过；
`--test es_search_e2e` 2 过；`--test search_e2e` 4 过（无回归）。
