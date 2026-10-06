# WP3 — Kafka 面能力对齐：headers 回放、组管理 API、SASL token

状态：landed（Batch 1.5 + Batch 2 收口；状态板见 `README.md`）
日期：2026-10-06
工作包：WP3（headers 回放缺陷修复随 Batch 1 执行；其余随 Batch 2）

## 背景

Kafka front（`src/kafka/`）已覆盖 P0-P4：帧/编解码（`frame.rs`）、Produce(0)/Fetch(1)/
ListOffsets(2)/Metadata(3)、OffsetCommit(8)/OffsetFetch(9)、FindCoordinator(10)/
JoinGroup(11)/Heartbeat(12)/LeaveGroup(13)/SyncGroup(14)/DescribeGroups(15)、
ApiVersions(18)（`src/kafka/mod.rs` :53-65，广告面 13 行，`COMPAT.md` :217 同口径）。
对照常用 Kafka 客户端（confluent-kafka / librdkafka，见
`scrtips/e2e_scenarios/scenario_kafka_sdk.sh`）与管理面的最小期望，仍有三块差距：

| # | 差距 | 级别 | 现状证据 |
|---|---|---|---|
| A | **headers 往返断裂**：produce 侧已把 headers 存成 `"h"` 对，但 Fetch 回放时 headers 不还原为 Kafka record headers，整条消息被包成 JSON envelope 塞进 value | A 级缺陷（数据语义静默变形） | `src/kafka/produce.rs` :239（写 `"h"` 对）；`src/kafka/fetch_records.rs` `to_key_value`（headers 形状走 envelope 分支，`BatchRecord.headers` 恒为空 `Vec::new()`） |
| B | **组管理 API 缺失**：无 ListGroups(16)/DeleteGroups(42)，`src/kafka/` 无 admin 面（无 `admin.rs`），组生命周期只能靠重启/过期收敛 | 功能缺失 | `src/kafka/mod.rs` `implemented_apis()` 13 行；组面仅有 coordinator/（runtime）与 `ledger.rs`（0x20 账本行） |
| C | **鉴权缺失**：无 SASL/任何握手机制，与 es/s3 front 的 `es_token`/`s3_token` Bearer 姿态不齐 | 功能缺失 | `src/conf.rs` 有 `es_token`(:75)/`s3_token`(:87)，无 kafka 对应键；`src/kafka/handshake.rs` 无 SASL 分支 |

三块互不依赖、均不改存储引擎，只在 `src/kafka/` 前置层内收敛；WP 间与
WP2/WP4/WP5 无代码依赖（05 号文档的按需池不含本包条目）。

### 批次归属

| 范围 | 内容 | 批次 |
|---|---|---|
| 范围一 | headers 回放缺陷修复 + `kafka_headers_roundtrip_e2e.rs` | **Batch 1**（与 01 号文档全部条目并行） |
| 范围二 | ListGroups/DeleteGroups + `kafka_admin_e2e.rs` | Batch 2 |
| 范围三 | SASL PLAIN `kafka_token`（e2e 并入范围二用例文件） | Batch 2 |

## 范围一：headers 回放缺陷修复（Batch 1）

### 现状

- 写路径（已正确，不动）：produce 侧字段对映射为权威规则（`features/kafka-front.md`
  "字段对映射"）：headers 非空时写 `("h", headers JSON)` 对，JSON 形如：

  ```json
  [{"n":"h1","v":null},{"n":"h2","v":"00ff"}]
  ```

  （字节值 hex 编码；null 值保留 null。既有断言：`tests/kafka_produce_e2e.rs` :224-230。）
- 读路径（缺陷）：`src/kafka/fetch_records.rs` 的 `to_key_value` 只还原
  key/value 两槽；凡携带 `"h"` 对（或任何非 2 对形状）的条目一律走 generic
  envelope 分支——value = JSON `{"fields":[[name,"<hex>"]...]}`、key = None，
  且组装 `BatchRecord` 时 `headers: Vec::new()`。结果：客户端 Fetch 回来的是
  "一条 value 为内部 JSON 的消息"，headers 语义丢失，key 也被抹掉。

### 方案

回放侧（仅 `fetch_records.rs`）按写路径规则反向解码，组装真 record headers：

| 存储字段对形状 | 回放结果 |
|---|---|
| `[]` | key=None, value=None, headers=[]（不变） |
| `[("__null__","")]` | tombstone（不变） |
| `[("v",value)]` | value-only（不变） |
| `[("k",key),("v",value)]` / `[("k",key),("__null__","")]` | key/value（不变） |
| `[("k",key),("v",value),("h",json)]`（key 非 null） | key=key, value=value, **headers=headers JSON 反解** |
| `[("v",value),("h",json)]`（key null） | key=None, value=value, **headers=反解** |
| **exotic 形状**（非 (bytes,bytes) 名值对、嵌套异常、`"h"` 值非合法 headers JSON、字段对数超界等无法无损还原者） | **保留现有 envelope 编码兜底**：value=envelope JSON、key=None，并在 headers 中打 1 个标记头（如 `("rdb-envelope","1")`），保证字节不丢、可被探测 |

- headers JSON 反解：`[{"n","v"}]` → `Vec<(&str, Option<&[u8]>)>`（hex 反解回
  bytes），直接填入 `BatchRecord.headers`（槽位已存在，`src/kafka/record.rs` :227，
  `build_batch` 已按 varint 数量 + 每头 varint 长度写出——即 RecordBatch v2 的
  headers 区本来就会编码，只是今天恒空）。
- tombstone + headers（null value 带 headers）走第 6 行规则：value=None、
  headers 还原；e2e 单列用例。
- 预算：`fetch_records.rs` 现 140 行，回放分支 + 反解/兜底函数预计 +80 行 →
  ≤230 行（远低于 400/800 双上限）。

### 回归点（强制）

- `tests/kafka_produce_e2e.rs` 的既有 envelope/字段对断言（headers 用例
  :188-230 一带）将被真 headers 回放改变：**写路径存储断言（`"h"` 对原文）
  保留不动**作为 produce 回归；其中经 Fetch/envelope 视角的断言须同步改为
  真 headers 断言。同一 PR 内完成，不允许出现"先改行为、后补断言"的中间态。
- 存量数据兼容：Batch 1 上线前已按 envelope 写入盘上的条目**没有任何标记可
  判断它当年是 headers 形状还是真 exotic**——回放统一走兜底路径（envelope
  value + 标记头），与今天客户端可见行为等价（多一个可探测标记头），不丢数据、
  不误还原。即：**升级不改变存量消息的 value 字节语义**。

### 行为变更声明（客户端可见）

| 消息形状 | 修复前 Fetch 可见 | 修复后 Fetch 可见 |
|---|---|---|
| 带 headers 的消息（key 非 null / null） | value=内部 envelope JSON、key=None、headers 空 | key/value 原样 + 真 headers |
| 无 headers 的消息 | 不变 | 不变（字节级回归锚） |
| exotic 字段对（存量/非常规写入） | value=envelope JSON、headers 空 | value=envelope JSON 不变 + 1 个标记头 |

这是**有意的语义修复**（A 级缺陷销账），不是兼容性破坏：envelope 形状从未
在任何文档中承诺为对外契约，`features/kafka-front.md` 字段对映射一节同步改写
为"双向权威"（写规则 + 回放规则）。

### e2e：`tests/kafka_headers_roundtrip_e2e.rs`（新，≤400 行）

| 用例 | 断言 |
|---|---|
| 真 headers 往返 | produce 带 N 个 headers（含 null 值头、二进制值头）→ Fetch v10 解 RecordBatch，key/value/headers 全等（名称、值字节、顺序） |
| 无 headers 不受影响 | 纯 key/value、value-only、null-null tombstone 三形状回放字节与修复前一致（回归锚） |
| tombstone + headers | null value 且 headers 非空：value=None、headers 完整还原 |
| 存量 envelope 兜底 | 直接以 RESP XADD 写入 exotic 字段对（模拟存量/非常规写入）→ Fetch 得 envelope value + 标记头，字段名字节全部可在 envelope 中找到（不丢数据） |
| 压缩构建（若启 kafka-codecs feature） | 带 headers 的 Fetch 回放不经压缩路径（fetch 恒不压缩），仅核 RecordBatch CRC/长度自洽 |

## 范围二：ListGroups/DeleteGroups（Batch 2）

### API 形态（按 Kafka wire 协议通用知识）

```
ListGroups (api key 16)
  请求:  [v3+ tagged fields / state_filter 可选（v1+）] —— 仅解析至所需版本窗口
  嵌套:  throttle_ms(v1+)、error_code、groups[](group_id, protocol_type[, state(v1+)])
DeleteGroups (api key 42)
  请求:  groups[](group_id 字符串数组)
  嵌套:  throttle_ms(v1+)、results[](group_id, error_code)
```

版本窗口初定 ListGroups v0-v1、DeleteGroups v0-v1（满足 confluent-kafka
AdminClient 的 list_consumer_groups/delete_consumer_groups 基本路径）；最终
以实现时对照客户端协商结果定版，逐版本字段差异落在 codec 注释里。

### 新模块 `src/kafka/admin.rs`（≤400 行，纯函数式）

- 职责边界：**请求→响应的无状态映射** + 两个纯聚合函数；IO/锁一律复用
  coordinator 与 ledger 既有入口，本模块不持有状态。
- 组全集 = **协调器 runtime 中的组 ∪ 账本 0x20 行中的组**（进程重启后 runtime
  清空、账本仍在，ListGroups 必须仍能看到有 committed offset 的组）：
  - `list_groups(runtime_groups, ledger_groups) -> Vec<(id, protocol_type, state)>`
    纯合并去重；
  - `group_state_of(runtime_state, has_members, ledger_only) -> &'static str`
    纯映射。
- state 枚举按本仓实际可判定子集：

  | 来源 | state |
  |---|---|
  | runtime 组（协调器状态机：Empty / PreparingRebalance / CompletingSync / Stable，`src/kafka/coordinator/state.rs`） | 实际状态名 |
  | 仅账本（runtime 无此组、0x20 行存在） | `Empty`（有 committed offset、无活跃成员——与协议 Empty 语义一致） |
  | `Dead` | 只保留给 DescribeGroups 的"未知组"既有口径（`features/kafka-front.md` 已定版），ListGroups 不产出 Dead |

### protocol_type 口径

- runtime 组：取组注册时的 protocol_type（JoinGroup 首个成员带入，
  `src/kafka/coordinator/state.rs` 状态机已持有该字段）。
- 账本-only 组：0x20 行只存 committed offset/generation/leader，无
  protocol_type——统一报 `consumer`（Kafka 管理客户端对消费者组的事实缺省，
  ListGroups 的主要消费方即 list_consumer_groups）。

### DeleteGroups：清账本 + runtime 逐出

组级逐一处理，幂等语义明确化：

1. 清账本：删除该组所有 0x20 行（`src/kafka/ledger.rs` 需补一个按组前缀删行
   的辅助函数，纯函数 + Store 参数传入）；
2. runtime 逐出：从协调器 runtime 移除该组（走 coordinator 既有管理入口，
   不在 admin.rs 内开新锁）；若组仍有活跃成员，仍强制逐出（本仓单 broker、
   无上游可踢，成员随后 Heartbeat 得 `UNKNOWN_MEMBER_ID(25)` 自然收敛——
   `src/kafka/errors.rs` :24 已有该常量）。

错误码矩阵：

  | 情形 | 结果 |
  |---|---|
  | 组存在，删除成功 | `NONE(0)` |
  | 组不存在（runtime 与账本均无） | 组级对应协议错误码（语义 = group not found；数值以实现时按协议错误码表定版，`errors.rs` 需新增常量）——**不**复用 DescribeGroups 的 Dead+0 姿态（Delete 是写操作，静默成功会掩盖拼写错误） |
  | 组名非法（空/超长） | `INVALID_GROUP_ID(24)` |
  | 版本不支持 | 走既有 `api_supported` 拒绝路径（`UNSUPPORTED_VERSION(35)`，与全 API 一致） |

### 广告面 +2 API key（强制回归）

- `src/kafka/mod.rs` `implemented_apis()` 13 行 → 15 行（+16、+42），`api_name`
  同步；`COMPAT.md` :217 的 "13 wire APIs" 同步改 15。
- **`scrtips/e2e_scenarios/scenario_kafka_sdk.sh` 必须重跑通过**：广告面变化是
  真实 SDK 版本协商的直接输入（该脚本存在的理由即在此），admin API 上线但不
  重跑视为未验收。

### e2e：`tests/kafka_admin_e2e.rs`（新，≤400 行）

| 用例 | 断言 |
|---|---|
| ListGroups wire 编解码 | 空集群 → 空列表 error 0；建组（JoinGroup 收敛 Stable）后列出含该组、state=Stable、protocol_type=consumer |
| 账本-only 组 | 仅 OffsetCommit 过、重启模拟（或 runtime 无组）后 ListGroups 仍列出、state=Empty |
| DeleteGroups wire 编解码 | 删除存在的组 → 0x20 账本行消失（RESP 直查 ledger 前缀佐证）、ListGroups 不再出现 |
| 删除后重建 | DeleteGroups → 再 JoinGroup 成功重建（generation 从账本高水位语义之外重新起步，不残留旧成员/旧 generation） |
| 错误码矩阵 | 未知组、非法组名、不支持版本各得对应错误码 |
| SASL 矩阵（范围三并入） | 见下节 401/成功矩阵 |

## 范围三：SASL PLAIN `kafka_token`（Batch 2）

### 配置

`src/conf.rs` 新增：

```yaml
kafka_token: ""   # 默认空 = 不启用鉴权（与 es_token/s3_token 同姿态，无敏感默认值）
```

### 握手期校验（`src/kafka/handshake.rs`）

- 机制：SASL PLAIN（按 Kafka wire 协议通用知识：`SaslHandshake(17)` 声明
  mechanism，`SaslAuthenticate(36)` 携带 `authzid\0authcid\0passwd` token 串）。
- 校验点集中在**握手期**：token 非空时，连接先走 SaslHandshake/SaslAuthenticate，
  passwd 段与 `kafka_token` **常量时间比对**（authzid/authcid 忽略）；通过后
  连接标记已认证——该标记随连接上下文传递（per-connection，不引入全局可变
  状态，符合纯函数式约束）。
- 认证前白名单：仅 `ApiVersions(18)`（客户端 bootstrap 必需，`src/kafka/conn.rs`
  顶部注释既定规则）与 SaslHandshake/SaslAuthenticate 本身；其余 API 在未认证
  连接上直接断连（不泄露错误细节，对齐常见 broker 行为）。
- 广告面条件化：`kafka_token` 非空才把 SaslHandshake/SaslAuthenticate 计入
  ApiVersions 应答表；未配置时广告面与今天完全一致（13/15 行，不含 SASL 面）。

### 失败矩阵

| 场景 | 行为 |
|---|---|
| 未配置 `kafka_token`（默认） | 行为完全不变：无 SASL 面、任何客户端照旧可用（回归锚：既有 `kafka_*` e2e 全部不配 token 跑） |
| 配置后、无 SaslHandshake 直接发 Fetch/Produce | 握手拒绝（断连），不执行命令 |
| 配置后、SaslAuthenticate 密码错 / 畸形 PLAIN 串 | 拒绝并断连；**错误响应不含 token 任何片段**（长度也不泄露：常量时间比对 + 固定错误文案） |
| 配置后、正确 token | 认证通过；此后 Fetch/Produce/JoinGroup/OffsetCommit 等已过握手 API **不再重复校验**（每连接一次） |
| 错机制名（非 PLAIN） | SaslHandshake 拒绝该 mechanism |

### e2e（并入 `kafka_admin_e2e.rs`）

不启用 token 节点全绿（不变）+ 启用 token 节点：无凭据断连、错凭据断连、
正确凭据后 Produce/Fetch/ListGroups 全通；测试 token 为 e2e 专用假值（参照
`tests/es_auth_e2e.rs` 的 `ES_TOKEN` 假 token 形态，不涉真实敏感值）。

## 实现落点与行数预算

| 文件 | 现状 | 动作 | 预算 |
|---|---|---|---|
| `src/kafka/fetch_records.rs` | 140 行 | 修改：回放分支 headers 反解 + exotic 兜底 | ≤230 |
| `src/kafka/admin.rs` | 无（新增） | ListGroups/DeleteGroups 无状态映射 + 聚合纯函数 | ≤400 |
| `src/kafka/admin_tests.rs` | 无（新增） | 聚合/状态映射/错误码纯函数单测（沿 `produce_tests.rs` 惯例） | ≤300 |
| `src/kafka/handshake.rs` | 343 行 | 修改：SASL PLAIN 握手分支 + 应答体构造 | ≤430（超则拆 `sasl.rs`） |
| `src/kafka/mod.rs` | 222 行 | 修改：`implemented_apis` +2 行、`api_name` | ≤235 |
| `src/kafka/conn.rs` | 319 行 | 修改：+2 dispatch 分支、认证门（白名单判断） | ≤380 |
| `src/kafka/ledger.rs` | 250 行 | 修改：按组删 0x20 行辅助（纯函数 + Store 参数） | ≤280 |
| `src/kafka/errors.rs` | 109 行 | 修改：新增组不存在错误码常量 | ≤120 |
| `src/conf.rs` | 293 行 | 修改：`kafka_token` 字段（serde default 空） | ≤300 |
| `tests/kafka_headers_roundtrip_e2e.rs` | 无（新增） | Batch 1 验收 | ≤400 |
| `tests/kafka_admin_e2e.rs` | 无（新增） | Batch 2 验收（含 SASL 矩阵） | ≤400 |

全部改动遵守：新文件 <400 行、迭代中文件 <800 行；不引入 class/OOP——新增
逻辑均为「请求/输入 → 响应/输出」的无状态函数，状态只经参数传递（协调器
runtime 经既有入口、连接认证标记随连接上下文）。

## 兼容与安全

- **ApiVersions 只增不改**：旧 13 个 key 的版本区间一行不动，仅追加 16/42
  （及条件化的 17/36），旧客户端协商结果不变。
- **headers 回放对存量 envelope 数据兼容**：见范围一"回归点"——兜底路径保证
  存量消息 value 字节语义不变。
- **token 安全**：默认空=关闭；错误响应不回显配置值；常量时间比较；e2e 用
  假 token；文档/示例不写真实凭据。
- 删除组是显式写操作，影响 0x20 账本（committed offset 不可恢复）——文档中
  标注不可逆。

## 风险与回滚

| 风险 | 缓解 |
|---|---|
| headers 反解把存量 exotic 数据误判为 headers 形状 | 形状判定只认严格的 2/3 对规范布局 + `"h"` 值必须整体反解成功；任何偏差落兜底，e2e 存量用例锁行为 |
| `kafka_produce_e2e.rs` 断言更新遗漏（Batch 1） | 断言更新与行为改动同一 PR；`cargo test --workspace --no-fail-fast` 门禁兜底 |
| DeleteGroups 误删活跃组 | 组级错误码 + e2e"删除后重建"用例；文档标注 0x20 账本删除不可逆 |
| SASL 面让旧客户端协商退化 | 未配置 token 时广告面/行为零变化（既有 e2e 全部无 token 跑通即证明）；SASL key 只在配置后广告 |
| 单文件超行数预算 | `admin.rs`/`admin_tests.rs` 先拆；`handshake.rs` 超 430 行拆 `sasl.rs`（均 <400 新文件上限） |

回滚：范围一、二、三各自独立成 PR，revert 单个 PR 不影响其余两块（广告面
revert 后 `scenario_kafka_sdk.sh` 需再重跑一次）。

## 验收命令

```bash
cargo test --test kafka_headers_roundtrip_e2e        # Batch 1 单跑
cargo test --test kafka_admin_e2e                    # Batch 2 单跑（含 SASL 矩阵）
cargo test --workspace --no-fail-fast                # 既有 kafka_* e2e 全绿（kafka_produce/fetch/group/group_failover/offsets/wire/codec）
cargo fmt --check
cargo clippy --workspace -- -D warnings
bash scrtips/e2e_scenarios/scenario_kafka_sdk.sh     # 广告面变化后必须重跑通过
```

## 文档同步与 changelog 草稿

- `features/kafka-front.md`：字段对映射表旁补"回放 headers 还原规则 + exotic
  兜底"；新增 ListGroups/DeleteGroups 小节（含错误码矩阵）与 SASL
  `kafka_token` 小节；"13 行广告面"改 15。
- `COMPAT.md`：Kafka front 段（:217 一带）补 headers 往返、admin API、
  `kafka_token`；RocksMQ/ES 段不动（WP4 负责）。
- `features/e2e-coverage.md`：登记 `kafka_headers_roundtrip_e2e` /
  `kafka_admin_e2e`。
- changelog 草稿（`features/changelog/`，两条）：

  ```
  fix(kafka): replay real record headers
  feat(kafka): ListGroups/DeleteGroups + SASL token
  ```
