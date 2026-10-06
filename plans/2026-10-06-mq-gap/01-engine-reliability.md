# WP1 — 引擎可靠性：DLQ + MAXDELIVERY、自动重投、XTRIM MINID、两缺陷修复

> 状态：proposed
> 日期：2026-10-06
> 工作包：WP1（执行批次 Batch 1）
> 关联：`00`（总纲与差距矩阵）、`02`（延迟消息，WP2/Batch 2）、`03`（kafka parity，
> headers 回放缺陷修复随 Batch 1 一并执行）、`06`（e2e 总表）

## 1. 背景与缺陷佐证

Lite MQ 面（`features/mq-lite.md`）已落地 PEL / XCLAIM / XAUTOCLAIM / 有序消费组等
投递基建，但距"一个可用的 MQ"仍缺**投递失败兜底**与**留存治理**两块引擎级能力；
另有两项已登记缺陷随本工作包一并销账。

- **投递次数不驱动策略**：PEL 行的 `times_delivered` 仅作展示/查询字段，不参与任何
  投递决策（`src/lite/claim.rs`：claim 路径只刷新投递时钟并自增该计数，计数随后
  随 XPENDING 输出展示，全链路无任何策略分支消费它）。消息被反复 claim 也不会有
  任何出口——毒消息（poison message）可无限期占住 PEL；在 ordered 组里还会卡死
  队头，其后全部消息无法投递。
- **留存只有条数窗**：`XTRIM` 仅支持 `MAXLEN` 形态（`src/lite/append.rs:246` 附近，
  `XTRIM <stream> MAXLEN [<~|=>] <count>`），没有按时间/id 裁剪的能力；"保留最近 N
  小时"这类时间窗留存目前无任何手段。
- **已登记缺陷 ×2**（`features/kafka-front.md:267-270` "账本回收缺口"清单）：
  1. RENAME（`src/command/keys_core.rs:377,398` `move_family`）不搬 kind 0x20
     committed-offset 账本行——STREAM_FAMILY 之外的家族搬运本就未覆盖；重命名后
     旧名残留孤儿账本行，新名查不到已提交偏移；
  2. kafka 面流未拒绝 XTRIM/XDEL（P2 交付时守卫顺延）——ordinal 稳定性风险仍在，
     裁剪/删除会动摇 Fetch 的 ordinal 语义。
- **headers 回放断裂（交叉引用）**：kafka 面另一处缺陷（record headers 回放丢失）
  在 `03-kafka-parity.md` 详述，不在本文展开；按批次划分其修复**随 Batch 1 一并
  执行**，其 e2e（`kafka_headers_roundtrip_e2e.rs`）计入本工作包验收门。

差距小结（详细定级见 `00` 总纲）：

| 能力 | 现状 | 本包目标 |
|---|---|---|
| 毒消息出口 | ❌ 无（`times_delivered` 仅展示） | MAXDELIVERY 超限进 DLQ |
| 无人值守重投 | ❌ 仅客户端主动 XAUTOCLAIM/XCLAIM | 引擎侧 idle sweep（默认关） |
| 时间窗留存 | ❌ 仅 MAXLEN 条数窗 | XTRIM MINID |
| RENAME 账本随迁 | ❌ 0x20 行漏搬（已登记缺陷） | 折叠搬运 + e2e 钉死 |
| kafka 面裁剪守卫 | ❌ 未拒绝（已登记缺陷） | 有账本引用即拒绝 |

这些缺口没有一个能靠换协议解决：kafka front / HTTP 面都只是 Lite 引擎之上的映射
前置，投递兜底与留存治理必须落在引擎（`src/lite/` 与族删除/搬运路径）——与
`features/mq-lite.md` "先引擎、后协议"的既有路线判断一致。

## 2. 目标 / 非目标

### 目标
1. **DLQ + MAXDELIVERY**：消费组可配置最大投递次数，超限消息原子转移到死信流，
   毒消息有确定性出口。
2. **自动重投**：默认关闭、可配置的 idle 自动重投 sweep，无人值守场景不再依赖
   客户端主动 XAUTOCLAIM。
3. **XTRIM MINID**：对齐 Redis 的按 id（时间）裁剪，兼作时间窗留存策略。
4. **两缺陷销账**：RENAME 搬运 0x20 账本行；kafka 面流 XTRIM/XDEL 守卫。

### 非目标
- 延迟/定时消息：WP2（`02-delay-messages.md`）。
- kafka admin/SASL/压缩等其余 parity 项：WP3（`03`）；本包只含 headers 修复。
- HTTP 面 parity：WP4（`04`）。
- 事务、幂等生产、ISR/多副本承诺：`05` 显式不做（单节点持久性风险已在路线决策中
  显式接受，见 `features/mq-lite.md`）。
- **消息级 TTL / 每消息过期**：显式不做，以 XTRIM MINID 时间窗留存**替代**（见
  设计三与 `05` 的不做清单）。
- DLQ 流的再消费编排（重试拓扑、退避策略编排器）：只提供 `<stream>/dlq` 可独立
  消费的事实，不做上层编排。

### 验收口径
- 四个新 e2e 全绿 + 既有回归门全绿（见第 11 节）；
- 未配置 MAXDELIVERY、未开启重投的部署：行为与改动前**逐字节一致**（既有回归
  原样通过即证明）；
- 五项设计互相独立、可分项合入与回滚：单项 revert 不牵连他项（RENAME 修复与
  kafka 守卫除外——两者同属"账本完整性"，建议同 PR）。

## 3. 设计一：DLQ + MAXDELIVERY

### 语法
```
XGROUP CREATE <stream> <group> <id> [MKSTREAM] ... MAXDELIVERY <n> [DLQ <stream>]
```
- `MAXDELIVERY <n>`：同一 PEL 行累计投递（`times_delivered`）达到 `n` 后触发转移；
  未配置 = 现状（永不转移）。
- `DLQ <stream>`：死信流名；未显式给时默认 `<stream>/dlq`。DLQ 流按普通流处理，
  首次转移时惰性建立，无需预建。

### 转移语义（关键不变量）
超限转移在**同一 per-stream latch 下的单个 WAL 批**内完成三件事：

1. 向 DLQ 流追加一条 entry（保留原消息体，并附原 entry id、来源组、投递次数等
   追溯字段——以普通 field-value 对写入，DLQ 流侧零新语义）；
2. 删除该 PEL 行；
3. 推进连续前缀已确认水位（沿用 ack 路径的既有约束：水位只跨**连续已确认前缀**
   前进，绝不越过存活的 pending 行）。

三件事同批即**语义等同 ack**：消费方视角消息已离开组——XPENDING 不再可见、不再
计入 backlog gauge、重启后不复活。转移触发点在 claim/交付路径上（`times_delivered`
自增后判定），不是独立后台任务，因此**不会与客户端 claim 竞态双转**：判定与转移
在同一 latch 内串行。

一次转移的时序（意图）：

1. XCLAIM / XAUTOCLAIM（或设计二的 sweep 重投）把某 PEL 行的 `times_delivered`
   推到 `n`；
2. 同一 latch 内立即判定 `>= maxdelivery`，构造单批（DLQ entry + 删 PEL 行 +
   水位推进）并提交；
3. 落盘后 claim 回复照常携带该消息（本次投递合法），但组内 pending 行已消失——
   客户端下一次 XPENDING 即看不到它，与 ack 后的表现一致。

### ordered 组约束
ordered 组**仅 PEL 头（最小未确认 id）可转移**：非头转移会在有序投递流中打洞
（头之后的消息永远等不到头确认），破坏保序约束。头转移后组水位推进、后继消息
自然接续——与 ordered 组既有的头冻结/接管语义一致。

### GroupPayload 扩展
`src/lite/model.rs` `GroupPayload` 追加字段（maxdelivery 配置值 + DLQ 流名），沿用
既有 `ordered` / `inflight_max` 字段的追加模式：`#[serde(default)]` 兜底，旧记录
缺字段按默认读入、行为不变；解码防御沿用损坏计数模式（decode 失败计数跳过，
不 panic）。

### 指标
新增 gauge `rdb_lite_dlq_depth`（各 DLQ 流深度合计），挂入 `src/monitor.rs` 既有
`rdb_lite_streams` / `rdb_lite_backlog` 一族，随 200ms 背景节奏刷新。

### 边界情形
- `MAXDELIVERY 1`：首次投递即计 1，达到阈值即转（判定为 `>=`，不设下限特判）；
- DLQ 目标流指回原流或两组互指成环：不禁止——语义退化为"重新入列"，不做环
  检测，文档记录即可（保持实现简单）；
- 配置的 DLQ 流被删除：下次转移时按普通流路径惰性重建；
- `XGROUP DELCONSUMER`：其"清该消费者全部 pending 行"的既有语义先于转移判定，
  行已不存在则无从转移，两者天然不冲突；
- 转移后 XPENDING 不可见（行已删），追溯信息在 DLQ entry 的字段里，不在 PEL。

### 重启一致性
转移批落盘后重启：PEL 行已删 + 水位已推进 + DLQ entry 已在——三态一致；转移批
未落盘（kill -9 于批前后）：三态全无，消息仍在原 PEL，重新 claim 后再次判定，
转移幂等可重入。不存在"DLQ 有 entry 而原 PEL 行仍在"的双份中间态（同批保证）。

## 4. 设计二：自动重投调度

- **独立 sweep 任务**：复用现有 200ms 后台节奏模式（`src/lite/mod.rs` 背景循环），
  每轮按有界预算扫描活跃组的 PEL，把 idle 超过阈值的行重投。
- **配置**（`src/conf.rs` 首次新增 `lite:` 段）：
  ```yaml
  lite:
    redelivery_idle_ms: 0   # 0 = 关闭（默认）；>0 即启用自动重投
  ```
  默认 0 = 关：升级零行为变化，是否开启完全由部署方决定。完整键名记法
  `lite_redelivery_idle_ms`（`00` 号矩阵同指此键，即 `lite:` 段下的
  `redelivery_idle_ms`）。
- **重投 = 无人触发的自动 claim**：走与 XREADGROUP 交付、XAUTOCLAIM、手动 XCLAIM
   相同的 claim 原语（`src/lite/claim.rs`）——覆盖写 PEL 行、刷新投递时钟、
   `times_delivered` 自增。三者并存不冲突：所有路径收敛到同一 latch 串行化。
- **ordered 组**：走 `force_takeover` 头接管（`src/lite/ordered.rs` 既有原语），
   绕过头冻结——自动重投只作用于 PEL 头，接管后从头继续，保序不破。
- **与设计一的衔接**：sweep 重投使 `times_delivered` 达到 MAXDELIVERY 时，转交
   设计一的转移路径——重投给了消息最后的机会，超限后进 DLQ。

### 预算与节奏
- 每轮有界预算：扫描 PEL 行数与实际重投条数各自设上限，仿主动过期扫描器
  （`src/ds/expire/active.rs`）的预算模式——病态大 PEL 不会拖垮单个 tick，
  未处理完的顺延下一轮；
- sweep 与既有 offset flush 共用 200ms 节奏但任务体独立，互不阻塞、互不重入；
- idle 判定 = 当前时钟 − PEL 行记录的投递时钟，只读既有字段，不新增持久化
  状态；sweep 自身无跨轮内存态。

| 组形态 | sweep 行为 |
|---|---|
| 无序组 | idle 超阈值的 PEL 行逐行重投（claim 原语） |
| ordered 组 | 仅 PEL 头可重投，`force_takeover` 头接管绕过头冻结 |

## 5. 设计三：XTRIM MINID

### 语法（对齐 Redis）
```
XTRIM <stream> MINID [~|=] <id> [LIMIT n]
```
- 语义：删除所有 id **严格小于** `<id>` 的 entry，与 Redis 一致。
- `~` 近似裁剪（可按批次提前停，`LIMIT n` 限制单次删除量）；`=` 精确裁剪
  （逐 id 判定到边界）。
- 实现复用 `src/lite/append.rs` 既有 XTRIM 的 latch + meta len 维护路径：MAXLEN
  与 MINID 是两种策略参数，**正交可并存**（一次调用只带一种，对齐 Redis；交替
  使用互不干扰）。

### 兼作时间窗留存
`MINID <now-window 的毫秒部分>-0` 即"保留最近 window 时长"——这是**替代消息级
TTL** 的方案（消息级 TTL 在 `05` 显式不做）：调用方或定时任务周期执行即可获得
时间窗留存，不需要每消息一条索引记录。

### 示例
```sh
XTRIM mystream MINID ~ 1760000000000-0        # 近似：保留 id >= 该值，可提前停
XTRIM mystream MINID ~ 1760000000000-0 LIMIT 1000   # 单轮最多删 1000 条
XTRIM mystream MINID = 1760000000000-0        # 精确：删到边界为止
```
- `LIMIT` 仅随 `~` 生效（对齐 Redis 语义；`=` 配 `LIMIT` 的处理按 Redis 行为
  对齐，具体错误文案以对齐为准）；
- 空流 / `<id>` 早于首条：0 条删除，正常回复；`<id>` 非法（格式/越界）沿用既有
  id 解析错误路径。
- 裁剪只动 entry（kind 0x0D）窗口，PEL 行（0x0F）与组记录（0x0E）分属不同 kind
  窗口、不受影响——与 MAXLEN 既有行为一致。

## 6. 设计四：RENAME 搬 0x20 账本

- 现状：RENAME 见到流的 kind 0x0C meta 行，`move_family`（`src/command/
  keys_core.rs:377,398`）只搬触发键所属的 STREAM_FAMILY（0x0C..=0x0F）窗口，
  kind 0x20 committed-offset 账本行留在旧名下成为孤儿。
- 改法：源为 STREAM_FAMILY 时，把 OFFSET_FAMILY（0x20）窗口**显式折叠搬运**——
  与删除侧先例同构：`src/ds/expire/mod.rs:69-90` `family_delete_entries` 已用
  显式折叠处理 0x20（注释明确单段 0x0C..=0x20 会吞掉 JSON/vectorset/search
  记录，故必须分段）。
- 行为断言：搬运后旧名上的提交/读取失败（键不存在），新名 offset 原值保留；
  搬运扫描失败的语义沿用 `move_family` 既有契约——部分记录集绝不提交。

### 步骤（意图）
1. RENAME 解析出源为流（触发键 kind 0x0C）后，在既有 STREAM_FAMILY 搬运段之外
   追加一段 OFFSET_FAMILY（0x20）窗口的复制 + 删除范围；
2. 两段在同一 WriteBatch、单次批量写提交——中途失败即整体不落盘；
3. 非流键（string/hash/...）的 RENAME 路径零改动（不经过折叠段）。

## 7. 设计五：kafka 面流 XTRIM/XDEL 守卫

- 对**存在 offset 账本引用**（该流名下有 kind 0x20 行）的流，XTRIM 与 XDEL 直接
  拒绝：裁剪/删除会改变活跃条目集，动摇账本 committed_ordinal 的对照基准。
- 账本引用探测为**有界扫描**（扫该流 0x20 窗口，超预算时按"存在引用"保守拒绝），
  不做全量计数。
- 错误文案区分两类：`stream has consumer-group offset ledger references`
  （存在组账本引用，提示先处理组/账本）与普通参数错误——运维可据文案直接定位。
- 守卫只拦 XTRIM/XDEL，不拦整流删除：DEL/FLUSHDB/XIDLE 整流走族删除路径，
  `family_delete_entries` 已连带清账本（0x20 折叠），守卫自然解除。

### 错误文案（意图）
```text
XTRIM mytopic/p0 MINID ~ 0-1
(error) ERR stream mytopic/p0 has consumer-group offset ledger references; \
resolve the group ledger before trimming
```
- 文案要点：含流名 + 指明"组账本引用"这一具体成因，与"参数个数错误 / id 非法"
  等普通错误一眼可分；
- 有界扫描预算耗尽时同样返回该错误（保守拒绝），并在日志侧记录预算截断事实。

## 8. 实现落点与行数预算

| 落点 | 现状行数 | 动作 | 预算 |
|---|---|---|---|
| `src/lite/dlq.rs` | 新增 | 转移判定、单批构造、DLQ 流惰性创建、gauge 上报 | <400 行 |
| `src/lite/redeliver.rs` | 新增 | sweep 循环、idle 判定、ordered 头接管衔接 | <400 行 |
| `src/lite/read.rs` | 786 | **不再增行**（贴近 800 迭代上限）：DLQ 触发点经外置函数注入——判定与批构造全部在 `dlq.rs`，接入处以等量改写完成，净增行 ≤ 0 | 0 |
| `src/lite/claim.rs` | 336 | 小改：claim 结果透出触发判定所需信息 | 余量充足 |
| `src/lite/append.rs` | 444 | 小改：XTRIM MINID 策略分支 | 余量充足 |
| `src/lite/model.rs` | 417 | 小改：GroupPayload 追加字段 | 余量充足 |
| `src/command/keys_core.rs` | 440 | 小改：`move_family` 折叠搬运 0x20 | 余量充足 |
| `src/conf.rs` | 293 | 首次新增 `lite:` 段（`redelivery_idle_ms`） | 小改 |
| `src/monitor.rs` | 360 | 新增 `rdb_lite_dlq_depth` gauge | 小改 |
| `src/ds/codec.rs` | 301 | 本包不动 kind 表（0x1D 登记属 WP2） | 0 |

纯函数式约束：不引入 class；判定/转移/批构造均为无内部可变状态的函数，输入经
参数、输出经返回值；若需表达转移状态机，仿 `src/kafka/coordinator/state.rs`
纯状态转移风格（旧状态进、新状态出，无自修改）。

`lite:` 段形态：嵌套子段（对齐 `src/conf.rs` 既有 `tx:` 段的组织方式），首版仅
含 `redelivery_idle_ms` 一个键；WP2 的延迟扫描节奏键随后加入同段（见 `02`）。

测试侧预算：四个新 e2e 文件各自 <400 行；公共夹具复用 `tests/kafka_front_common`
（kill/respawn、组协议 helpers）与 `tests/common` 既有进程夹具，不新建夹具基建。

## 9. 兼容与安全

| 变更面 | 兼容策略 | 验证方式 |
|---|---|---|
| GroupPayload | 追加字段 + `#[serde(default)]` 兜底 | 旧组读写回归（`lite_group_e2e`） |
| RESP 面 | 无新增命令名，仅既有命令的选项扩展 | `resp_reply_semantics` 等既有回归 |
| 配置 | `lite:` 段新键默认关（`redelivery_idle_ms: 0`） | 空配置启动 + 既有 e2e |
| 备份只读面 | DLQ 流即普通流，随既有面复制 | `backup_readonly_e2e` / `backup_surface_e2e` |
| kafka 面 | 守卫仅新增"拒绝"分支，无行为改写 | `kafka_*` 既有回归 |

- **GroupPayload 向后兼容**：追加字段 `#[serde(default)]` 兜底，旧记录读入即默认
  值，未配置 MAXDELIVERY 的组行为与现状逐字节一致。
- **无新增 RESP 命令名**：XGROUP / XTRIM 均为既有命令的子参数/选项扩展，RESP 面
  无新动词，命令注册表与路由不动。
- **配置默认关闭**：`redelivery_idle_ms: 0` 即现状；升级即零行为变化。
- **备份只读面不受影响**：DLQ 流即普通流，随既有备份面复制；`tests/
  backup_surface_common` 的备份只读面相等断言不变——验证方式：跑
  `backup_readonly_e2e` / `backup_surface_e2e` 回归确认全绿。
- 代码不含任何敏感信息（键名/流名/配置键均为语义命名）。

## 10. e2e 验收（明细见 06 总表，此处列关键用例）

### `tests/lite_dlq_e2e.rs`
- 转移触发：投递 n 次后进 DLQ，原组 XPENDING 清空；
- 原子性 + 水位推进：kill -9 于转移批前后重启，三件事要么全成要么全无，水位
  不越过存活 pending 行；
- 重复 claim 不双转：并发 claim 与转移判定在 latch 内串行，DLQ 中至多一条；
- ordered 组仅 PEL 头可转移：非头超限不转移（卡头场景由头转移解开）；
- DLQ 流可独立消费（XREADGROUP `>` 于 `<stream>/dlq` 正常投递/确认）；
- kill -9 一致性（转移后重启不复活）；
- `rdb_lite_dlq_depth` gauge 随转移递增、随 DLQ 消费回落；
- 负矩阵：未配置 MAXDELIVERY 的组投递任意次数不转移；`MAXDELIVERY` 非正整数
  报参数错误；DLQ 流名与原流同名不报错（语义记录为重新入列）。

### `tests/lite_redeliver_e2e.rs`
- idle > `redelivery_idle_ms` 自动重投（`times_delivered` 自增、消费者可见）；
- 默认关：`redelivery_idle_ms: 0` 时 idle 任意久不重投；
- 与手动 XCLAIM 并存：两者交替作用于同一 PEL 行不冲突；
- 不重投已 ack 行（ack 后行消失，sweep 空转）；
- 负矩阵：阈值未到不投；ordered 组非头行 idle 再久也不被 sweep 触碰（等头）；
  sweep 不改变任何消息的可见性（只动 PEL 行的投递时钟与计数）。

### `tests/lite_trim_minid_e2e.rs`
- `MINID ~` 近似 / `=` 精确各一例（边界 id 恰好相等时不删）；
- `LIMIT n` 截断一轮删除量；
- 时间窗留存：构造跨时间窗数据后按 `now-window` 裁剪；
- kafka 守卫拒绝：带 0x20 账本引用的流 XTRIM/XDEL 均报"存在组账本引用"错误。

### `tests/kafka_rename_ledger_e2e.rs`
- RENAME 随迁：重命名后新名 OffsetFetch 读到原 committed offset；
- 旧名提交失败（键不存在）；
- 新名 offset 值不变（generation/leader 字段原样）。

（`tests/kafka_headers_roundtrip_e2e.rs` 随 Batch 1 执行，用例明细见 `03`。）

## 11. 验收命令与回归门

```sh
cargo test --test lite_dlq_e2e
cargo test --test lite_redeliver_e2e
cargo test --test lite_trim_minid_e2e
cargo test --test kafka_rename_ledger_e2e
cargo test --test kafka_headers_roundtrip_e2e   # 03 号文档详述，随 Batch 1 执行
cargo test --workspace --no-fail-fast            # CI 同款
cargo fmt --check
cargo clippy --workspace -- -D warnings
```

回归门：Batch 1 须全绿通过既有 `lite_ordered_e2e` / `flushdb_lite_e2e` /
`lite_pel_e2e` / `lite_group_e2e` / `kafka_produce_e2e` / `kafka_fetch_e2e` /
`kafka_group_e2e` / `kafka_offsets_e2e` / `kafka_wire_e2e` 回归（含
`backup_readonly_e2e` / `backup_surface_e2e`）。

分项门（PR 粒度，可独立合入）：每项设计对应独立 PR，合入前单跑该项 e2e + 受
影响的既有回归；Batch 1 收口时统一跑全量门——本文五项与 headers 修复（`03`）
任何一项红，Batch 1 即不算通过。

## 12. 文档同步与 changelog 草稿

- `features/mq-lite.md`：规范节增补 DLQ/MAXDELIVERY、自动重投、XTRIM MINID；
- `features/kafka-front.md`：两缺陷销账（账本回收缺口 + XTRIM/XDEL 守卫顺延），
  偏差清单同步收口；
- `COMPAT.md`：Lite Mode 条目增补（MAXDELIVERY/DLQ 语义、MINID 对齐说明、
  kafka 面守卫行为）；
- `agents/rust/index.md`：`src/lite/` 模块族补 `dlq.rs` / `redeliver.rs`；
- `features/e2e-coverage.md`：登记四个新 e2e 文件；
- changelog 条目草稿（日期 2026-XX-XX 占位，conventional commit 风格）：
  ```text
  feat(lite): XGROUP MAXDELIVERY/DLQ poison-message escape hatch
  feat(lite): idle auto-redelivery sweep (redelivery_idle_ms, off by default)
  feat(lite): XTRIM MINID time-window retention
  fix(keys): RENAME moves kafka committed-offset ledger rows (kind 0x20)
  fix(kafka): reject XTRIM/XDEL on streams with offset ledger references
  fix(kafka): preserve record headers on replay   # 03 号缺陷，随 Batch 1
  ```
- changelog 条目正文模板（占位）：动机（毒消息无出口 / 时间窗留存缺失 / 两项
  登记缺陷）→ 语义（上文各设计要点）→ 兼容（默认关、追加字段、无新命令名、
  备份面不变）→ 验证（新 e2e 清单 + 回归门结果）。
