# ES Front：Elasticsearch 兼容 HTTP 前置（同一检索内核）

## 定位
- **协议前置适配层**，与 `src/kafka/` 之于 Lite MQ、`sql/front` 之于 MySQL 协议同构：
  `src/es/` 只做 HTTP/JSON 编解码与 DSL 映射，存储与检索完全复用 FT.* 内核
  （`src/search/`）——一个索引 = 一个用户 key = 一个 slot，RESP 侧 FT.CREATE 建的
  索引在 ES 面可直接查写（进程级 e2e `tests/es/es_search_e2e.rs` 有互操作证明）。
- 接线：`es_bind`（空 = 关闭，backup 监听器天然不含）；`es_token` 非空时要求
  `Authorization: Bearer <token>`；`es_max_connections` 活连接帽（0 = 内置 4096，
  帽满静默断连 + 一行 stderr，镜像 kafka/rocksmq `ConnGuard`）。

## 端点面（HTTP/1.1，每连接一请求 `Connection: close`）
- 传输护栏（2026-11-10 hardening）：活连接帽见上；handler 执行 30s 查询超时
  （408 `timeout_exception`；head/body 读超时原本就有）；无 body 语义的方法
  （GET/DELETE/HEAD…）携带 `Content-Length > 0` 直接 400，不再静默吞 body。
  写请求也可能撞 30s 超时：latch 为 async RAII、写路径单批 fsync 且无后台线程，
  超时不会留下半批写；但恰在边界完成的写客户端视角仍可能收到 408（写 >30s
  属罕见）。
- 元数据：`GET /`（tagline）、`GET /_cluster/health`（green/yellow）、
  `GET /_cat/indices`。
- 索引：`PUT|GET|HEAD|DELETE /{index}`（mappings 往返；重复建 409；缺 404）。
- 文档：`PUT|POST /{index}/_doc[/{id}]`（201 created / 200 updated；`?op_type=create`
  409）、`GET|HEAD|DELETE /{index}/_doc/{id}`。
- 检索：`POST /{index}/_search`（见下）、`POST /{index}/_count`、
  `POST /{index}/_refresh`（no-op 成功回）。
- 批量：`POST /_bulk`、`POST /{index}/_bulk`（NDJSON；`index`/`create` 可省
  `_id` —— 服务端生成并在条目里回填（2026-11-10 修复：此前空 `_id` 恒报
  "invalid document id"，auto-id 分支为死代码）；`update` 动作按条报错不炸整批；
  单请求项数上限 100 000，超限整批 400）。

## 字段类型映射（权威）
| ES mapping 类型 | FT.* 内核类型 |
|---|---|
| text | TEXT（同一 jieba+拉丁分词器） |
| keyword | KEYWORD（原样字节，不分词不小写） |
| long / integer / double ... | NUMERIC（单一 f64） |
| dense_vector（必带 dims） | VECTOR DIM n |

## DSL 子集
- 查询：`match_all`、`match`（默认 `or` = 并集 + BM25 求和；`operator:and`）、
  `match_phrase`（v2 位置化 posting 的相邻链，短语分 = 成员词 BM25 之和；
  v1 无位置记录退化为 AND —— 已记偏差）、`term`/`terms`（`{"value":..}` 对象式
  亦收，boost 容忍忽略）、`range`（数值边界）、`prefix`（posting-key 前缀扫描 =
  词项析取，BM25 求和；CJK 前缀按切分后的 bigram 词项匹配）、`query_string`
  （仅认 `query`，default_field/fields 等容忍忽略 —— 偏差；文法与 FT.SEARCH
  共用同一棵解析树，两端同语法同记录；含 `field:[min max]` 数值区间，
  常量分 1.0）、`bool`
  （must/filter/should/must_not + `minimum_should_match`：非负整数或 0-100 百分比，
  负数/组合式 400；百分比四舍五入、下限 1；默认不变 —— 纯 should = 并集(msm 1)，
  有 must/filter 时 should 只加分，显式 msm>0 才升级为门槛）。
  停用词：`match` 分析后词项按索引停用词表过滤（FT.CREATE 建的索引才有非空
  表，ES 建索引恒为空；纯停用词 match 零命中）；phrase/prefix/query_string
  经共享求值器自动同样过滤；`term`/`terms`/`range` 精确值不过滤。
- knn：`field/query_vector/k/num_candidates/filter`（精扫候选集或 SPANN probe，
  得分 `1/(1+L2)`）。
- 修饰：`sort`（字段 / `_doc` / `_score`，缺值排最后；数值字段走列式 f64
  doc-values，未映射字段回退 `_source` JSON；`{"field":"desc"}` 简写亦收）、
  `from`/`size`（窗口 ≤10000）、`_source`（false / includes / excludes）。
- 未实现即 4xx/5xx 错误信封（`error.type/reason`），不静默降级。

## 偏差
权威偏差清单见 `COMPAT.md` "Elasticsearch-compatible frontend (Rust-only)" 节
（版本号恒 1、_refresh no-op、keyword 精确字节、跨节点 400 `routing_exception`
不自 PROXY、HTTP/1.1 无 keep-alive、chunked 501 等）。
