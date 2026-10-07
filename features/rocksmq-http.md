# RocksMQ HTTP：极简 HTTP 消息 API（P4）

## 定位
- RocksMQ 风格的**极简 HTTP/1.1 前置**（`src/rocksmq/`），与 Kafka front 同构：
  协议适配层，存储与引擎完全复用 Lite MQ（`src/lite/`），Redis Streams 动词面
  仍是第一公民。
- 手写 HTTP（`rcache/http.rs` / `es/http.rs` 一派，零新增 crate）：keep-alive
  默认（`Connection: close` 或 HTTP/1.0 则关），head 上限 32 KiB（431），body
  上限 4 MiB（413），chunked 请求体 501，`Expect: 100-continue` 先应答再读 body。
- 接线：`rocksmq_bind`（空=关闭，backup 监听器不含）；并发连接上限
  `rocksmq_max_connections`（0=内建 4096，见下文连接上限节）。鉴权与 es/s3
  前置同姿态：
  `rocksmq_token`（默认空 = 不鉴权；非空 = 全路由要求 `Authorization: Bearer
  <token>`，否则 401，见下文鉴权节）。
- 复用路径：HTTP 层构造 argv 走 `command::dispatch`（XADD/XREADGROUP/XGROUP/
  XPENDING/XACK/XREVRANGE），解析回包 RESP 字节（`src/rocksmq/respv.rs`）。因此
  自动获得 slot 前缀计算、panic 兜底、backup 只读门与 `rdb_command_latency`
  直方图（label=xadd/xreadgroup/...），**未新增** `rdb_rocksmq_api_latency`。
  （直调 `src/lite/` pub fn 需重复前缀计算且绕开兜底与打点，代码量不减，故弃。）

## 四接口契约

| 接口 | 语义 | 成功响应 |
|---|---|---|
| `POST /produce?channel=NAME[&delay_ms=MS]`，body=负载 raw bytes | XADD 等价，单字段对 `("v", body)`；`delay_ms>0` 走 XADD `DELAY` 暂存 | `200` body=消息 id 文本 `<ms>-<seq>` |
| `POST /consume?channel=NAME[&group=G][&n=K][&wait_ms=MS]` | 有 group：XREADGROUP `>`；无 group：尾部拉取；均可长轮询 | `200` JSON `{"msgs":[{"id":"..","body":"<base64>"}]}` |
| `POST /ack?channel=NAME&group=G&id=<ms>-<seq>` | XACK（幂等：id 不在 PEL 也 200） | `200` body=`ok` |
| `POST /pending?channel=NAME&group=G` | XPENDING 概要只读透传（总 pending 数/最小最大 id/按消费者分布） | `200` JSON（见下） |

`/pending` 响应（空 PEL 时 `min_id`/`max_id` 为 `null`、`consumers` 为 `[]`，
与 RESP XPENDING 概要对空 PEL 的回答一致；group 模式下 HTTP 面是单一逻辑消费者
`http`，consumers 通常只有一项）：

```json
{"channel":"NAME","group":"G","pending":3,
 "min_id":"1760000000000-0","max_id":"1760000000005-1",
 "consumers":[{"name":"http","pending":3}]}
```

错误：缺/空参数、非法名、空 body、`n`/`wait_ms`/`delay_ms` 越界 → 400；未知
路径 → 404；已知路径非 POST → 405（带 `Allow: POST`）；ack/pending 组不存在 →
404（先以 XPENDING 概要探组，因 XACK/XPENDING 对未知组的回包无法直接区分）。
查询值做 URL-decode（`+` 与 `%XX`）；下文三个 P3 新路由遵循同一错误表
（非 POST 405、未知路径 404、token 门禁一致）。

## 批量与回放（P3：`/produce_batch`、`/ack_batch`、`/range`）

| 接口 | 语义 | 成功响应 |
|---|---|---|
| `POST /produce_batch`，body=JSON 数组 | 每元素一次 `/produce` | `200` JSON：逐元素结果数组（请求序） |
| `POST /ack_batch`，body=JSON 数组 | 每元素一次 `/ack` | 同上 |
| `POST /range?channel=NAME[&begin=B][&end=E][&limit=K]`（`topic=` 为别名） | 只读回放 XRANGE | `200` JSON `{"msgs":[{"id":"..","body":"<base64>"}]}` |

请求元素 = 单路由入参的 JSON 重组；逐项结果 = 单路由回复原文（`status` +
`body`）的 JSON 落位：

```json
// /produce_batch 元素（"delay_ms" 可为 number 或 string，缺省/null = 不带；
//  "body" 为标准 base64——单路由 body 是 raw bytes，JSON 内以 base64 保真，
//  与 /consume 回包同一字母表，消费→重产 byte-exact）
{"channel":"NAME","delay_ms":0,"body":"aGVsbG8="}
// /ack_batch 元素（= /ack 的三个查询参数）
{"channel":"NAME","group":"G","id":"1760000000000-0"}
// 逐项结果（两批量路由同形）
{"status":200,"body":"1760000000000-0"}
{"status":400,"body":"invalid channel name"}
```

- 实现路径（`src/rocksmq/batch.rs`）：每元素重组单路由 `Query`
  （`query.rs::Query::of`）后**调用既有单项处理器**（`api::produce` /
  `api::ack`）——逐项语义与单路由字节一致（校验顺序、400/404 文案、200 body
  出自同一份代码，非复刻）。元素相互独立：一项失败不中断其余，失败就地可见。
- 元素按请求序顺序执行（批量 produce 的 id 因此单调递增）；缺 `body` 字段 =
  空 body，由单路由处理器自身回 400（与单路由一致）。
- 整请求 400 仅三种：body 非合法 JSON / 非数组 / 超过 `MAX_BATCH`=100 个元素
  （与 `/consume` 的 `n` 同一常量）；空数组合法，回 `[]`。元素非对象、字段
  类型错、base64 非法均为**逐项** 400。
- 批量路由照常过 token 门禁（路由层单点注入，新路由默认进门禁）。

### `/range`（只读回放）
- 参数：`channel`（别名 `topic`）、`begin`（默认 `-`）、`end`（默认 `+`）、
  `limit`（默认 100，1..=100）。边界语法 = Lite XRANGE 约定
  （`src/lite/model.rs::parse_bound`）：`-`/`+` 哨兵、前缀 `(` 排他、
  `<ms>-<seq>` 含端点；非法边界 HTTP 层先行 400。
- 复用路径：`run` → `command::dispatch(XRANGE ...)` → `src/lite/append.rs::xrange`
  （`src/command/readonly.rs` 只读命令表内），entry 前缀扫描
  （`model::entry_base` + `ops::for_each_from`），按 id 升序。
- **只读保证**：无 PEL 登记、无可见性/重投副作用、不建组不记进度（XRANGE 不
  触碰组状态）。回放后无组 channel 的 `/pending` 依旧 404；组消费照旧可取全部
  未确认消息（e2e 断言）。回包解码/形状复用 `consume_wait.rs` 的 `msg_of` /
  `msgs_json`，与 `/consume` 回包同构。
- 延迟消息在到期交换前不是 entry，`/range` 不可见（与一切读路径一致）；
  不存在的 channel → `{"msgs":[]}`。

### 连接上限（`rocksmq_max_connections`）
- 配置项（W0 加入，`src/conf.rs`）：rocksmq HTTP 前置并发 TCP 连接上限；
  `0`（默认）= 内建 4096（`src/rocksmq/guard.rs::DEFAULT_MAX_CONNS`，与 kafka
  front `src/kafka/conn.rs::DEFAULT_MAX_CONNS` 同值）；负值一律回落 4096。
- 实现：进程级 `AtomicI64` 在场计数 + RAII `ConnGuard`（accept 起计，连接
  关闭即释放；任何提前返回——对端关闭/畸形头/超限/空闲超时——都归还槽位），
  镜像 kafka front 的同名模式；单位测试覆盖计数算术与拒纳回滚。
- 拒绝行为与 kafka 逐字一致：达到上限时新 TCP 连接**静默关闭**（处理任务
  直接返回、不写任何 HTTP 字节，socket 随之 drop），仅 stderr 记一行
  `[rocksmq] connection refused: at cap <N>`（kafka 先例：
  `[kafka] connection refused: at cap <N>`）。

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
- `n` 默认 1，上限 100；`body` 为 base64（条目 `v` 字段）；不带 `wait_ms` 时
  非阻塞，空结果回 `{"msgs":[]}`。

### `wait_ms` 长轮询
- `wait_ms` 是 `/consume` 新增可选参数（缺省/`0` = 现状，回复字节不变）：
  缓冲区有消息即回（不引入额外等待）；无消息则 park 至到期回 `200` 空列表
  （**非无限阻塞**，不是错误）。非数字/负数 → 400；>60000 clamp 到 60000
  （低于常见网关 75s/120s 空闲超时；生产取值建议 ≤ 上游超时）。
- 等待不在 HTTP 层重写：group 模式映射为 XREADGROUP `>` 带 `BLOCK <wait_ms>`，
  无 group 尾拉模式先 XREVRANGE（有数据即回），空则 `XREAD BLOCK ... $` park——
  两种模式共用 Lite 的 `park_wait` 多流等待循环（单 waiter 注册在被通知 meta
  key 上、分片 park、醒后复验关 lost-notify 窗口），XADD 提交与延迟消息到期
  交换（due exchange 的 notify）都能唤醒。
- **空通道 + 组不存在** 的长轮询：组随流而建，流不存在时组也无法建——此时
  HTTP 面 park 在流的 meta key 上（`XREAD ... $`），首个 produce 落地后再按
  常规"建组于头 + XREADGROUP"路径交付（PEL 登记完整，与非空通道首消一致）。

### `delay_ms` 透传（produce）
- `POST /produce?channel=NAME&delay_ms=MS`：HTTP 层纯透传 XADD 的 `DELAY` 前置
  选项；`0`/缺省 = 不带该选项对（argv 与现状逐字节一致）。非数字/负数/超过
  `MAX_DELAY_MS`（365 天）→ 400。延迟语义（0x1D 暂存行、到期前一切读路径
  不可见、到期交换分配**新 id**、produce 回复 id 只是预约凭证）全部由 Lite
  引擎定义，见 [mq-lite.md](./mq-lite.md) 延迟消息节。

### 鉴权（`rocksmq_token`）
- 空（默认）= 全开放，既有客户端零变化；非空 = **所有** rocksmq HTTP 路由
  要求 `Authorization: Bearer <token>`（scheme 大小写不敏感，token 精确匹配，
  非 constant-time——与 es/s3 前置同款简单比对），否则 `401` 固定文案
  `unauthorized`（不回显任何请求/配置片段）。
- 门禁在路由层单点注入（新路由默认进门禁）；公开路径豁免清单为模块内常量，
  初始为空（无探活/健康检查路径）。测试用 FAKE token（`tests/` 内嵌）。

## 互操作
- 与 RESP：channel 即 Lite 流名，RESP `XADD/XREADGROUP/XACK/XPENDING/XRANGE`
  直接操作同一流（e2e 双向验证）。
- 与 Kafka front（P1 定版的 1 对规则）：HTTP produce 写 `("v", body)` 与 Kafka
  无 key 无 header 的 record 字段对**字节一致**，两个前置互相可读对方消息。
- 无 `v` 字段的手写 RESP 条目经 HTTP consume 时 body 为空串（不报错）。

## 偏差清单（vs 真 RocksMQ）
- 无 group 拉取不保证不重不漏（真 RocksMQ 每消费者有游标）。
- 单字段对固定为 `v`（真 RocksMQ 消息体无字段结构）；无 RocksMQ 的 topic 创建/
  删除/seek 接口——对应能力走 RESP `XGROUP CREATE/DESTROY/SETID`。
- 消息 id 是 Lite 到达时钟 `<ms>-<seq>`，非 RocksMQ 的 offset 整数。
- 4 MiB body 上限、100 条批量与 60s 长轮询上限、365 天 `delay_ms` 上限均为
  本文档化常量。
- `delay_ms` produce 的回复 id 是 XADD 时刻的预约 id，到期交换后实际 entry 拥有
  新 id（Lite 延迟语义；拉模式客户端不应把该 id 当作可 ack 的凭证预存）。
- 组名 ≤256 字节（Lite 本身收 raw bytes，HTTP 面收紧）。

## 测试
- `tests/rocksmq_http_e2e.rs`（进程级，真二进制 + rocksmq_bind + RESP 互证）：
  产/组消/确认全流程（含幂等 ack、PEL 清空、组 404）、RESP XADD ↔ HTTP consume、
  HTTP produce ↔ RESP XRANGE 同流互证、裸名/全名等价、无组尾部拉取无进度、
  400/404/405/413、keep-alive 两连发与 `Connection: close` 关闭。
- `tests/rocksmq_wait_pending_e2e.rs`（进程级，同上真二进制，`lite.delay_sweep_ms`
  武装）：`wait_ms` 超时回空且默认/0 行为不变、有消息即回；plain produce 与
  **延迟到期交换**两种唤醒（延迟唤醒断言新 id）；`/pending` 摘要（计数/min/max/
  消费者分布、ack 后递减、空 PEL 形状、400/404/405）；`delay_ms` 到期前不可消/
  到期后可消、`0` 即时、非法 400；`rocksmq_token` 401/通过矩阵（FAKE token）。
- `tests/rocksmq_batch_range_e2e.rs`（进程级，fixture 取自 `tests/common/mq.rs`）：
  批量 produce 有序 id；混合批量（一项非法）其余成功且逐项错误可见；批量 ack
  子集后 pending 相应收缩；`/range` 界内回放 id 升序 + `limit` 截断；`/range`
  只读（随后组消费仍见全部未确认消息、pending 不增长）；三新路由 token 门禁
  401/通过；`rocksmq_max_connections: 1` 时第二条并发连接被静默拒绝。
- 单测：`src/rocksmq/{mod,api,query,respv,auth,consume_wait,pending,batch,range,
  guard}.rs` 内嵌（头部解析/keep-alive、channel 映射、`n`/`wait_ms`/`delay_ms`
  边界与 clamp、base64 向量与解码、XPENDING 摘要→JSON、Bearer 矩阵、query
  解码、RESP 解析、批量元素/上限形状、XRANGE 边界语法与 limit、ConnGuard 计数
  算术与拒纳回滚）。
