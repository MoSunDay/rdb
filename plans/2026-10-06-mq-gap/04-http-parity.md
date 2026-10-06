# WP4 — RocksMQ HTTP 面对齐：长轮询、pending 可见性、Bearer token

状态：proposed
日期：2026-10-06
工作包：WP4（随 Batch 2 执行）

## 背景

RocksMQ HTTP 面（`features/rocksmq-http.md`，`src/rocksmq/`）现为三接口极简前置：
`POST /produce`、`POST /consume`、`POST /ack`，HTTP 层构造 argv 走
`command::dispatch`（XADD/XREADGROUP/XGROUP/XPENDING/XACK/XREVRANGE），回包由
`src/rocksmq/respv.rs` 解析——因此自动获得 slot 前缀、panic 兜底、backup 只读门
与 `rdb_command_latency` 打点。对照常用 MQ 的 HTTP 面（含 RocksMQ 生态惯用法），
有三块差距：

| # | 差距 | 现状证据 |
|---|---|---|
| A | **consume 即回非阻塞，无长轮询**：无消息时立即回空 `{"msgs":[]}`，拉模式客户端只能外层自旋 | `src/rocksmq/api.rs` `/consume` 分支（:205-315 一带）直接调 XREADGROUP/尾拉，无 BLOCK；Lite 侧已有成熟的阻塞等待基建 `src/lite/park_wait.rs`（XREAD/XREADGROUP 共用的多流 park 循环）未被 HTTP 面使用 |
| B | **pending 不可见**：消息 consume 后未 ack 前的数量/边界无从查询（XPENDING 动词已可 dispatch，但 HTTP 面没有暴露） | `src/rocksmq/api.rs` 仅 /produce /consume /ack 三路由；`src/lite/pending.rs` 摘要（`summarize` :48：总数/最小最大 ID/消费者分布）口径现成 |
| C | **无鉴权**：与文档"无鉴权（与 Kafka front 同姿态）"一致，但 es/s3 前置已有 token 门（`src/conf.rs` `es_token` :75 / `s3_token` :88），HTTP 消息面是同级别的暴露面 | `src/conf.rs` 无 rocksmq token 键；`src/rocksmq/mod.rs` 路由层无鉴权门 |

三块都在 HTTP 前置层收敛，不动 Lite 引擎与 RESP 动词面；与 WP2/WP3 无代码
依赖（`delay_ms` 仅为参数透传契约，语义由 02 号文档定义）。

## 设计一：`wait_ms` 长轮询

### 参数与语义

```
POST /consume?channel=NAME&group=G[&n=K][&wait_ms=MS]
```

| `wait_ms` 取值 | 行为 |
|---|---|
| 缺省 / `0` | 现状：缓冲区有则回、无则**立即**回空（字节级不变，回归锚） |
| `1..60000` | 缓冲区**有消息即回**（`wait_ms` 不引入任何额外等待）；无消息则 park 至 `wait_ms` 到期后回 `200` 空列表（**非无限阻塞**） |
| `>60000` | clamp 到 60000（宽松策略：不因客户端传大值而 400，长轮询上限对齐常见网关超时） |
| 非数字 / 负数 | `400`（与既有 `n` 越界同错误面） |

- 映射：group 模式 → XREADGROUP `>` 带 `BLOCK <wait_ms>`；无 group 尾拉模式
  → XREAD BLOCK（同一 park 循环，`src/lite/park_wait.rs` 头注释即"XREAD/
  XREADGROUP 共用的多流 wait loop"），两种模式语义一致。
- 复用 `park_wait` 的既有模式而非新写等待：**ONE waiter 注册在被通知 key 上、
  分片 park（slice 上限）、lost-notify 窗口以"醒后复验"闭合**——HTTP 面只是
  把 argv 从无 BLOCK 改为带 BLOCK，等待正确性全部继承 Lite 侧（含 XADD 提交
  与注册 waiter 竞争窗口的复验）。
- park 期间到消息：即回该批（受 `n` 上限约束）；到期仍无：回空。

```
# 等待 3s 后仍无消息
HTTP/1.1 200
{"msgs":[]}
```

### 兼容

- `wait_ms` 是**新增可选参数**：不带该参数的既有客户端行为与响应字节完全不变。
- backup 只读门、slot 前缀、打点（label 仍 xreadgroup/xread）不受影响——等待
  发生在 dispatch 内部，HTTP 层不感知。

## 设计二：`POST /pending?channel=<c>&group=<g>`

### 语义

- **XPENDING 摘要只读透传**：HTTP 层构造 `XPENDING <stream> <group>`（不带
  start/end/count/consumer 详细参数），走 `command::dispatch` → `src/lite/pending.rs`
  摘要路径；**只读、不产生任何副作用**（不修改 PEL、不消费、不建组）。
- 摘要口径对齐 `summarize`（`src/lite/pending.rs` :48）：总 pending 数、最小/
  最大 pending ID、按消费者的分布。group 模式下 HTTP 面是单一逻辑消费者
  （`api.rs` :28 既有注释），consumers 数组通常只有一项。

### 响应结构（字段示意）

```json
{
  "channel": "NAME",
  "group": "G",
  "pending": 3,
  "min_id": "1760000000000-0",
  "max_id": "1760000000005-1",
  "consumers": [
    { "name": "http", "pending": 3 }
  ]
}
```

（`min_id`/`max_id`/`consumers` 在无 pending 时为 `null`/`[]`，与 XPENDING
摘要对空 PEL 的回答一致。）

### 错误面（对齐既有三接口契约）

| 情形 | 结果 |
|---|---|
| 缺/空 `channel` 或 `group`、非法名 | `400` |
| 组不存在（XPENDING NOGROUP） | `404`——沿用 `/ack` 的探组法（`api.rs` 以 XPENDING 概要探组，因 XACK/XPENDING 对未知组的回包无法直接区分，同款约束在此同样成立） |
| 已知路径非 POST | `405`（带 `Allow: POST`） |

## 设计三：`rocksmq_token` Bearer 鉴权

### 配置与姿态

`src/conf.rs` 新增：

```yaml
rocksmq_token: ""   # 默认空 = 不启用鉴权（对齐 es_token/s3_token 姿态，无敏感默认值）
```

- 请求头 `Authorization: Bearer <token>` 与配置值**常量时间比对**（优先），
  匹配即放行；鉴权逻辑为**无状态纯函数**（`headers + 配置 token -> 允许/拒绝`），
  以参数注入路由层，不持有状态、不读全局。
- 对齐 es/s3 前置姿态（`src/es/http.rs` / `src/s3/http.rs`：非空 token 即门禁），
  并增加显式**公开路径豁免清单**（模块内常量，初始为空）：清单内路径（探活/
  健康检查类）不鉴权，清单外全部要求 Bearer。空清单 = 与 es/s3 完全同款的全
  路径门禁；不做可配置豁免（避免配置面扩张），后续有探活需求再加常量。

### 401 矩阵

| 场景 | 行为 |
|---|---|
| 未配置 `rocksmq_token`（默认） | 行为完全不变，无鉴权（既有 e2e 全部不配 token 跑） |
| 配置后、无 `Authorization` 头 | `401`，body 固定文案（不回显 token、不含配置值片段） |
| 配置后、错 token / 非 Bearer scheme | `401` 同上 |
| 配置后、豁免清单路径 | 不鉴权（清单为常量，初始空） |
| 配置后、正确 Bearer | 三接口 + /pending 正常 |

- 测试形态参照 `tests/es_auth_e2e.rs`：用 e2e 专用**假 token**（如
  `"e2e-rocksmq-token"`，非真实敏感值）经 yaml 注入子进程节点，覆盖 401/成功
  两态；未配置节点的全绿即"行为不变"回归锚。

## 按需池指针（关联 05 号文档）

以下三项**列入 05 号文档的按需池**（触发条件、优先级由 05 定义，本文件不展开）：

| 条目 | 说明 | 与本包关系 |
|---|---|---|
| 批量 produce | 一次 HTTP 请求写多条消息（batch body / NDJSON） | 依赖本面参数解析风格，无代码耦合 |
| 批量 ack | 一次请求 ack 多个 id | 同上 |
| `/range` 回放 | 按 ID 区间回放消息（XREVRANGE/XRANGE 透传） | 同上；若带 `delay_ms` 透传则依赖下方参数规范 |

**`delay_ms` 参数规范（本文件定义，语义由 02 号文档负责）**：

```
POST /produce?channel=NAME&delay_ms=MS    # body 即消息负载
POST /consume?channel=NAME&group=G&delay_ms=MS
```

- 含义：延迟投递毫秒数，HTTP 层**纯透传** Lite `DELAY`（写入侧的延迟语义、
  到期可见性、与 tombstone/ack 的交互全部由 02 号文档定义并验收）。
- 校验：非负整数，越界/非数字 → `400`；缺省 = 不带该字段对（与现状逐字节一致）。
- 本面只承诺：参数名/类型/缺省行为稳定，02 的 e2e 可与本面联测（见验收）。

## 实现落点与行数预算

| 文件 | 现状 | 动作 | 预算 |
|---|---|---|---|
| `src/rocksmq/api.rs` | 399 行 | **不再扩张**：`/consume` 的 group 等待编排与 `/pending` 处理移出 | ≤400（consume_group 逻辑平移后净减） |
| `src/rocksmq/consume_wait.rs` | 无（新增） | `wait_ms` 参数解析/clamp、BLOCK argv 构造、park 唤醒→回复组装、`/pending` 摘要透传与 JSON 组装 | ≤400 |
| `src/rocksmq/mod.rs` | 377 行 | 路由 +1（`/pending`）、鉴权门注入（调用无状态鉴权函数） | ≤420 |
| `src/conf.rs` | 293 行 | `rocksmq_token` 字段（serde default 空） | ≤300 |
| `tests/rocksmq_wait_pending_e2e.rs` | 无（新增） | 本包验收 | ≤400 |

- 拆分动机：`api.rs`(399) 贴近 400 新文件上限与 800 迭代上限之间的窄带，任何
  新增路由都必须先腾挪——本包把"等待/查询"两个新职责整体移入
  `consume_wait.rs`，`api.rs` 只保留路由与参数校验骨架。
- 纯函数式约束：无 class；`wait_ms`/`delay_ms` 解析、鉴权判定、XPENDING 摘要
  → JSON 的转换均为无状态函数（输入参数 → 输出值）；park/唤醒复用 Lite 侧
  既有基建，不新增共享可变状态。

## e2e 验收：`tests/rocksmq_wait_pending_e2e.rs`

| 用例 | 断言 |
|---|---|
| wait_ms 超时回空 | 空通道 `wait_ms=500` → 200 `{"msgs":[]}`，且耗时 ≥500ms（非立即回） |
| 有消息即回不受 wait_ms 影响 | 预发消息后 `wait_ms=5000` → 立即返回该消息（耗时远小于 5000ms） |
| wait 缺省/0 行为不变 | 不带 `wait_ms` 的空通道 consume 立即回空（现状回归锚） |
| 长轮询唤醒 | `wait_ms=5000` 挂起期间另一连接 `/produce` → 迅速收到该消息（park→唤醒路径） |
| /pending 摘要一致 | produce→consume→部分 ack 后，`/pending` 的总数/min/max/消费者分布与同节点 RESP `XPENDING` 摘要逐字段一致 |
| /pending 错误面 | 缺参数 400、未知组 404、非 POST 405 |
| token 401 与成功矩阵 | 未配置 token 节点全通；配置假 token 节点：无头/错头 401、正确 Bearer 三接口 + /pending 全通 |
| delay_ms 透传 | 带 `delay_ms` produce 后，Lite 侧按 02 号文档语义可观测（与 02 的 e2e 联测，断言以 02 为准；本面仅断言参数接受与缺省不变） |

## 风险与回滚

| 风险 | 缓解 |
|---|---|
| park 挂起占用连接，与 keep-alive/上游网关超时打架 | `wait_ms` 上限 clamp 60000（低于常见网关 75s/120s 空闲超时）；文档提示生产上取值 ≤ 上游超时 |
| 长轮询放大 backup 只读节点负载 | 只读门/打点全部继承 dispatch 路径，无新增旁路；e2e 不涉 backup 面，行为不回归 |
| `/pending` 依赖探组（XPENDING 概要）误判 | 与既有 `/ack` 同款约束，错误面 404 与文档一致；e2e 负向用例锁定 |
| token 门漏放某条新路由 | 鉴权在路由层单点注入（新路由默认进门禁），`/pending` 与三接口共用同一门；豁免清单为常量且初始为空 |
| `api.rs` 移移停停再次逼近上限 | 拆出 `consume_wait.rs` 后 `api.rs` 预算 ≤400 并在 PR 描述里贴 `wc -l`；后续新路由一律先拆后加 |

回滚：三个设计各自独立 PR（wait_ms / pending / token），revert 互不影响；
`delay_ms` 仅是参数契约，随 02 号文档节奏。

## 验收命令

```bash
cargo test --test rocksmq_wait_pending_e2e
cargo test --workspace --no-fail-fast
cargo fmt --check
cargo clippy --workspace -- -D warnings
```

## 文档同步与 changelog 草稿

- `features/rocksmq-http.md`：三接口契约表扩为四接口（+/pending）；新增
  `wait_ms`（长轮询语义表）、`rocksmq_token`（Bearer + 401 矩阵 + 豁免清单）、
  `delay_ms`（透传契约，语义指向 02）小节；"无鉴权"定位句改写。
- `features/e2e-coverage.md`：登记 `rocksmq_wait_pending_e2e`。
- `COMPAT.md`：RocksMQ HTTP 段（:745 一带）补 wait_ms / /pending / token /
  delay_ms。
- changelog 草稿（`features/changelog/`）：

  ```
  feat(rocksmq): long-poll consume, pending visibility, bearer token
  ```
