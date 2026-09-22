# RocksMQ HTTP：极简 HTTP 消息 API（P4）

## 定位
- RocksMQ 风格的**极简 HTTP/1.1 前置**（`src/rocksmq/`），与 Kafka front 同构：
  协议适配层，存储与引擎完全复用 Lite MQ（`src/lite/`），Redis Streams 动词面
  仍是第一公民。
- 手写 HTTP（`rcache/http.rs` / `es/http.rs` 一派，零新增 crate）：keep-alive
  默认（`Connection: close` 或 HTTP/1.0 则关），head 上限 32 KiB（431），body
  上限 4 MiB（413），chunked 请求体 501，`Expect: 100-continue` 先应答再读 body。
- 接线：`rocksmq_bind`（空=关闭，backup 监听器不含）。**无鉴权**（与 Kafka front
  同姿态）：不可信网络下绑定回环/受控端口。
- 复用路径：HTTP 层构造 argv 走 `command::dispatch`（XADD/XREADGROUP/XGROUP/
  XPENDING/XACK/XREVRANGE），解析回包 RESP 字节（`src/rocksmq/respv.rs`）。因此
  自动获得 slot 前缀计算、panic 兜底、backup 只读门与 `rdb_command_latency`
  直方图（label=xadd/xreadgroup/...），**未新增** `rdb_rocksmq_api_latency`。
  （直调 `src/lite/` pub fn 需重复前缀计算且绕开兜底与打点，代码量不减，故弃。）

## 三接口契约

| 接口 | 语义 | 成功响应 |
|---|---|---|
| `POST /produce?channel=NAME`，body=负载 raw bytes | XADD 等价，单字段对 `("v", body)` | `200` body=消息 id 文本 `<ms>-<seq>` |
| `POST /consume?channel=NAME[&group=G][&n=K]` | 有 group：XREADGROUP `>`；无 group：尾部拉取 | `200` JSON `{"msgs":[{"id":"..","body":"<base64>"}]}` |
| `POST /ack?channel=NAME&group=G&id=<ms>-<seq>` | XACK（幂等：id 不在 PEL 也 200） | `200` body=`ok` |

错误：缺/空参数、非法名、空 body、`n` 越界 → 400；未知路径 → 404；已知路径非
POST → 405（带 `Allow: POST`）；ack 组不存在 → 404（先以 XPENDING 概要探组，
因 XACK 对未知组也回 `:0` 无法区分）。查询值做 URL-decode（`+` 与 `%XX`）。

## channel 映射
- 裸名 `NAME` → `NAME/q0`（RESP XADD 自动队列与 Kafka front partition 0 同一流）；
- 含 `/` 视为完整 `parent/child`，按 `lite::parse_topic_name` 校验
  （`[A-Za-z0-9._-]{1,64}`，至多一个 `/`），非法名 400。

## 消费语义
- **有 group**：XREADGROUP `>`，consumer 名固定 `http`；**组不存在自动建组**
  （真 RocksMQ 无建组接口，首次消费即建）：`XGROUP CREATE <stream> <g> 0-0`，
  即组从头消费（Kafka `auto.offset.reset=earliest`）；流不存在 → 空数组且不建
  任何东西；游标/PEL/断点续投全部继承 Lite XREADGROUP 语义（at-least-once）。
- **无 group**：独立拉取——XREVRANGE 取尾部 `n` 条再正序返回，**不记进度**：
  重复拉到同批、期间新消息挤掉旧消息都属正常（文档化偏差）。
- `n` 默认 1，上限 100；`body` 为 base64（条目 `v` 字段）；非阻塞，空结果回
  `{"msgs":[]}`。

## 互操作
- 与 RESP：channel 即 Lite 流名，RESP `XADD/XREADGROUP/XACK/XPENDING/XRANGE`
  直接操作同一流（e2e 双向验证）。
- 与 Kafka front（P1 定版的 1 对规则）：HTTP produce 写 `("v", body)` 与 Kafka
  无 key 无 header 的 record 字段对**字节一致**，两个前置互相可读对方消息。
- 无 `v` 字段的手写 RESP 条目经 HTTP consume 时 body 为空串（不报错）。

## 偏差清单（vs 真 RocksMQ）
- 无 group 拉取不保证不重不漏（真 RocksMQ 每消费者有游标）。
- 无阻塞/长轮询 consume（真 RocksMQ 无组模式即时返回；带组阻塞版留待偏差清单
  之外再做）。
- 单字段对固定为 `v`（真 RocksMQ 消息体无字段结构）；无 RocksMQ 的 topic 创建/
  删除/seek 接口——对应能力走 RESP `XGROUP CREATE/DESTROY/SETID`。
- 消息 id 是 Lite 到达时钟 `<ms>-<seq>`，非 RocksMQ 的 offset 整数。
- 4 MiB body 上限、100 条批量上限为本文档化常量。
- 组名 ≤256 字节（Lite 本身收 raw bytes，HTTP 面收紧）。

## 测试
- `tests/rocksmq_http_e2e.rs`（进程级，真二进制 + rocksmq_bind + RESP 互证）：
  产/组消/确认全流程（含幂等 ack、PEL 清空、组 404）、RESP XADD ↔ HTTP consume、
  HTTP produce ↔ RESP XRANGE 同流互证、裸名/全名等价、无组尾部拉取无进度、
  400/404/405/413、keep-alive 两连发与 `Connection: close` 关闭。
- 单测：`src/rocksmq/{mod,api,query,respv}.rs` 内嵌（头部解析/keep-alive、
  channel 映射、base64 向量、`n` 边界、query 解码、RESP 解析）。
