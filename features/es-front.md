# ES Front：Elasticsearch 兼容 HTTP 前置（同一检索内核）

## 定位
- **协议前置适配层**，与 `src/kafka/` 之于 Lite MQ、`sql/front` 之于 MySQL 协议同构：
  `src/es/` 只做 HTTP/JSON 编解码与 DSL 映射，存储与检索完全复用 FT.* 内核
  （`src/search/`）——一个索引 = 一个用户 key = 一个 slot，RESP 侧 FT.CREATE 建的
  索引在 ES 面可直接查写（进程级 e2e `tests/es_search_e2e.rs` 有互操作证明）。
- 接线：`es_bind`（空 = 关闭，backup 监听器天然不含）；`es_token` 非空时要求
  `Authorization: Bearer <token>`。

## 端点面（HTTP/1.1，每连接一请求 `Connection: close`）
- 元数据：`GET /`（tagline）、`GET /_cluster/health`（green/yellow）、
  `GET /_cat/indices`。
- 索引：`PUT|GET|HEAD|DELETE /{index}`（mappings 往返；重复建 409；缺 404）。
- 文档：`PUT|POST /{index}/_doc[/{id}]`（201 created / 200 updated；`?op_type=create`
  409）、`GET|HEAD|DELETE /{index}/_doc/{id}`。
- 检索：`POST /{index}/_search`（见下）、`POST /{index}/_count`、
  `POST /{index}/_refresh`（no-op 成功回）。
- 批量：`POST /_bulk`、`POST /{index}/_bulk`（NDJSON；`update` 动作按条报错不炸整批）。

## 字段类型映射（权威）
| ES mapping 类型 | FT.* 内核类型 |
|---|---|
| text | TEXT（同一 jieba+拉丁分词器） |
| keyword | KEYWORD（原样字节，不分词不小写） |
| long / integer / double ... | NUMERIC（单一 f64） |
| dense_vector（必带 dims） | VECTOR DIM n |

## DSL 子集
- 查询：`match_all`、`match`（默认 `or` = 并集 + BM25 求和；`operator:and`）、
  `term`/`terms`、`range`（数值边界）、`bool`（must/filter/should/must_not）。
- knn：`field/query_vector/k/num_candidates/filter`（精扫候选集或 SPANN probe，
  得分 `1/(1+L2)`）。
- 修饰：`sort`（字段 / `_doc` / `_score`，缺值排最后）、`from`/`size`（窗口 ≤10000）、
  `_source`（false / includes / excludes）。
- 未实现即 4xx/5xx 错误信封（`error.type/reason`），不静默降级。

## 偏差
权威偏差清单见 `COMPAT.md` "Elasticsearch-compatible frontend (Rust-only)" 节
（版本号恒 1、_refresh no-op、keyword 精确字节、跨节点 400 `routing_exception`
不自 PROXY、HTTP/1.1 无 keep-alive、chunked 501 等）。
