# WP2 — 延迟消息（引擎级）

> 状态：proposed
> 日期：2026-10-06
> 工作包：WP2（执行批次 Batch 2）
> 关联：`01`（前置：键族删除 / RENAME 搬运路径改造，Batch 1）、`04`（HTTP
> `delay_ms` 透传，同为 Batch 2）、`05`（按需池，非依赖）、`06`（e2e 总表）

## 1. 背景

延迟/定时消息是常用 MQ 的高频能力（RocketMQ 定时消息为代表，业务侧用于订单
超时关闭、重试退避、定时触发等）。rdb 现状 ❌ **无任何延迟语义**：XADD 即时
落流、XREADGROUP 立即可投，投递时间与写入时间完全绑定，没有任何手段把一条
消息"押后"。

差距矩阵（`00` 总纲）将其定级为 **B 级**（常用能力缺失，非静默错误），本工作包
补齐引擎级延迟语义：写入时可声明相对延迟，到期前不可见，到期后按普通消息投递。
与 WP1 一样，这是引擎级缺口——协议前置层（kafka front / HTTP）都无法在引擎
不支持的情况下凭空补出延迟可见性。

| 能力 | 现状 | 本包目标 |
|---|---|---|
| 延迟/定时投递 | ❌ 无任何延迟语义 | `XADD ... DELAY <ms>` |
| 到期可见性 | ❌ 写入即立即可投 | 到期前全读路径不可见 |
| 到期顺序 | — | 乱序提交按 due 升序投递 |
| 崩溃一致性 | — | 单批两态归一，不双投不丢投 |

## 2. 语法与语义

### 语法
```
XADD <stream> * DELAY <ms> field value [field value ...]
```
- `DELAY <ms>` 为前置选项：位于自动 id（`*`）之后、首对 field-value 之前；
  不携带 `DELAY` 的 XADD 语法与行为**完全不变**（走原路径）。
- `ms` 为**相对延迟**（毫秒，非绝对时间戳）：到期时刻 = XADD 服务时刻 + ms；
  `DELAY 0` 等价于不延迟。

### 可见性与投递语义
- **到期前不可见**：暂存行物理上不是流 entry（独立 kind，见第 3 节），因此
  所有读路径——XREADGROUP（`>` 新消息与历史读）、XREAD、XRANGE、XLEN——到期前
  均看不见它；XLEN 不计暂存行。
- **到期后按普通消息投递**：到期批把消息体写入目标流，此后 PEL、XCLAIM、
  XACK、ordered 组、kafka 面映射等一切既有语义原样适用——到期路径不引入任何
  消费侧新概念。
- **乱序提交按 due 时间顺序投递**：同一流内多条延迟消息即使提交顺序乱（先提交
  长延迟、后提交短延迟），到期投递也按 due 升序写入目标流（entry id 单调），
  消费者看到的顺序 = 到期顺序。
- 幂等边界：延迟消息**不提供**去重/幂等（与普通 XADD 一致），重复提交即重复
  消息；业务幂等是消费方责任（对齐 RocketMQ Lite 定位）。

### 示例
```sh
XADD orders * DELAY 30000 event timeout-check order 42
# 30 秒内：XRANGE orders / XREADGROUP 不可见，XLEN 不计
# 30 秒后：按普通消息进入 orders，消费/确认/重投与普通消息无异
```

### 设计取舍（意图）
- **相对延迟而非绝对时间戳**：绝对时刻受时钟回拨影响且与重试退避场景不合；
  相对 ms 在 XADD 服务时刻一次性换算成 due 绝对时间落盘，之后不再受改动。
- **暂存行而非"进流即标不可见"**：若 entry 先进流再靠投递侧过滤，XREADGROUP/
  XRANGE/XLEN 每条路径都要带过滤条件，且 ordered 组会被不可见消息占住队头；
  暂存行让"不可见"成为物理事实，读路径零改动。
- **周期扫描而非每条消息一个定时器**：海量延迟消息下逐条定时器不可行；due
  前缀扫描 O(到期条数) 而非 O(暂存总数)。

## 3. 存储设计

### 暂存 kind 0x1D
- 独立暂存记录 kind `0x1D`——当前未占用（`src/ds/codec.rs` kind 表枚举至 0x19、
  另有 0x20 / 0xFD，0x1A..=0x1F 区间空闲）。
- **登记两处**（缺一不可）：
  1. `src/ds/codec.rs` kind 常量表：新增暂存 kind 常量（含族区间定义）；
  2. `src/ds/codec.rs:274` 附近的 typed-roots 分类规则（`classify`）：现规则为
     `<= 0x19`、`== 0x20`、`== 0xFD` 视作 typed 记录，其余按裸字符串——`0x1D`
     在 `0x19` 之外，**必须显式加入枚举**，否则迭代器/扫描器会把暂存行误读为
     裸字符串键。
- 暂存行内容：**due 绝对时间 + 完整消息体**（原始 field-value 对与目标流名）。
- 键布局（设计意图）：`data_key(prefix, 0x1D, stream) ++ <due 有序编码> ++ <分配
  的 entry id>`——due 在前使 due 扫描天然升序，无需额外索引。

### entry id 占位
- id 在 XADD 时刻即分配并锁进暂存行，同时**推进流 meta 的 last_id**——防止随后
  的无延迟 XADD 分配出撞号 id；到期写入目标流时沿用该 id，保证与流内既有 id
  单调一致。

### due 扫描器
- 仿 `src/ds/expire/active.rs` 主动扫描器模式：周期扫描、有界预算（扫描键数与
  单轮到期条数各自上限）、旋转游标（处理到哪从哪续扫，避免头部饿死尾部）、
  到期确认后再动批（防写者竞态）。
- 扫到 due 已到的暂存行即构造到期批（见第 4 节）；未到的自然止步（键序即
  due 序）。
- 扫描节奏由 `lite.delay_sweep_ms` 控制（见第 7 节）；扫描器复用既有 200ms 类
  后台任务的编排位置，但不与 offset flush 混在同一任务体内（互不阻塞）。
- 流被整流/删除时暂存行的随删是**第 5 节的强制验收项**，不在扫描器内做补偿
  （扫描器不负责清理孤儿——孤儿本就不该存在）。

## 4. 原子性（不双投）

- **到期投递 = 单 WAL 批**："删暂存行 + 写目标流 entry"（连同流 meta 的 len /
  last 维护）同批提交，同一 per-stream latch 下执行，无中间态窗口。
- **崩溃恢复幂等**：批要么整体落盘、要么整体不落，两个稳定态归一：
  - 暂存行**在** → 目标流必无对应 entry（批未落盘）→ 重扫后再投，不丢；
  - 暂存行**不在** → entry 必已写入（批已落盘）→ 不再投，不双。
  kill -9 于批前/批后/批中重启，恢复后重放该批幂等——**不双投、不丢投**。
- 扫描器自身无状态：每轮从游标/头部重扫，恢复语义完全由上述两态归一承载，
  重启不依赖任何内存记忆。

一次到期投递的时序（意图）：

1. 扫描器在 0x1D 窗口内前向扫，遇到首条 due 未到的行即停（键序即 due 序，
   之后全部未到）；
2. 取本轮预算内的到期行集合，逐流聚合成批：每流一批（受该流 latch 保护），
   批内 = N ×（删暂存行 + 写 entry）+ 一次 meta 维护；
3. 批落盘后该批消息立即对读路径可见，park 中的消费者被唤醒；
4. 下一轮从本轮末条已处理行之后续扫——未处理完的 due 行顺延，不丢。

## 5. 强制验收项：键族删除登记（P0 回归风险）

这是本工作包**最大的回归风险点**，单列验收。

- 现状：`src/ds/expire/mod.rs:69-90` `family_delete_entries` 按 kind 段**显式
  枚举**删除——STREAM_FAMILY（0x0C..=0x0F）逐段 + **显式折叠** OFFSET_FAMILY
  （0x20）窗口；注释明确说明：单段 0x0C..=0x20 会吞掉 JSON/vectorset/search
  记录，因此必须分段枚举。
- 风险：新增暂存 kind `0x1D` **不在** STREAM_FAMILY 段内。若不登记进
  `family_delete_ranges` 的折叠路径，则：
  - **XIDLE 整流**（idle TTL 到期回收流族）、**RENAME**（`move_family` 搬运流族）、
    **FLUSHDB**（族删除路径）三者都会**漏删/漏搬暂存行**；
  - 后果：流本体已删除，暂存行却残留到期投递——**已删流复活投递**，属
    **P0 级回归**（数据面出现不该存在的消息，且指向已删除的流）。
- 登记方式：与 0x20 同法——STREAM_FAMILY 删除/搬运路径显式追加 0x1D 窗口的
  delete_range / copy（依赖 `01` 对这些路径的改造完成，见第 8 节）。
- **为什么不能把 0x1D 并进 STREAM_FAMILY 段**：STREAM_FAMILY 是连续区间
  0x0C..=0x0F，把段上界扩到 0x1D 必然跨过 0x10..=0x19（JSON / vectorset /
  search 各族）——与 0x20 不能并入是同一个原因（`src/ds/codec.rs` /
  `src/ds/expire/mod.rs:69-90` 注记），所以只能独立折叠段。
- **强制验收**：`lite_delay_e2e` 必含 XIDLE / RENAME / FLUSHDB 三个交互漏删
  回归用例，三场景**逐一断言暂存行随流删除**（到期后无投递、暂存窗口为空）。
  缺任一场景即验收不通过。

## 6. 暴露面

| 面 | 支持 | 说明 |
|---|---|---|
| Lite RESP | ✅ 完整 | `XADD ... DELAY <ms>`；到期即普通消息 |
| HTTP | ✅ 透传 | `delay_ms` 参数透传至引擎，详见 `04-http-parity.md` |
| kafka 面 | ❌ **不暴露** | 见下 |

- kafka 面不暴露的理由（记录在案）：Kafka 协议 produce 面（RecordBatch）**无
  延迟位**，无对应语义可映射；强行在 front 层自造延迟 API 属于私有扩展，违背
  "wire 协议映射层、不是引擎分叉"的定位（`features/kafka-front.md`）。延迟消息
  的到企写入后对 kafka Fetch 面自然可见（普通 entry），无需任何 kafka 侧改动。

## 7. 实现落点与行数预算

| 落点 | 现状行数 | 动作 | 预算 |
|---|---|---|---|
| `src/lite/delay.rs` | 新增 | DELAY 选项解析、暂存行构造、due 扫描、到期批构造 | <400 行 |
| `src/lite/read.rs` | 786 | **不增行**（贴近 800 迭代上限），延迟逻辑不触读路径 | 0 |
| `src/lite/append.rs` | 444 | 小改：XADD 接入 DELAY 选项探测（探测后转调 `delay.rs`） | 小改 |
| `src/ds/codec.rs` | 301 | 登记暂存 kind 常量 + `classify` 枚举 + 族区间 | 小改 |
| `src/ds/expire/mod.rs` | 106 | `family_delete_entries` 折叠 0x1D 段（第 5 节） | 小改 |
| `src/command/keys_core.rs` | 440 | `move_family` 增补 0x1D 搬运（依赖 `01` 改造） | 小改 |
| `src/conf.rs` | 293 | `lite:` 段增扫描节奏键（`01` 已开段，此处仅加键） | 小改 |

- `src/lite/delay.rs` 内 due 扫描与到期批构造均为**无状态纯函数**：状态经参数
  传递（store/handle、当前时钟、游标），返回构造好的批与新游标；不引入 class、
  无内部可变状态、优先不可变结构——与 WP1 同一约束。
- 配置（`src/conf.rs` `lite:` 段，`01` 首次引入后本包仅追加键）：
  ```yaml
  lite:
    delay_sweep_ms: 200   # due 扫描节奏；默认对齐既有 200ms 后台节奏，保守取值
  ```
  仅节奏键名与数值语义，无敏感信息。
- 指标（意图）：暂存深度 gauge（未到期暂存行数，挂 `src/monitor.rs` 既有
  `rdb_lite_*` 一族）便于运维观察延迟积压；非验收阻断项，实现时定名。
- 测试侧预算：`tests/lite_delay_e2e.rs` <400 行；进程夹具复用 `tests/common`
  既有 kill/respawn 能力，不新建夹具基建。

## 8. 依赖与顺序

- **依赖 Batch 1（`01`）**对 `move_family` / 键族删除路径的改造完成：
  - RENAME 搬运须**覆盖新 0x1D 段**，否则 RENAME 漏搬暂存行（同第 5 节风险面，
    只是从"漏删"变成"漏搬"）；
  - 族删除路径在 `01` 中为 0x20 折叠做过一次梳理，0x1D 折叠搭同一班车落地，
    避免二次触碰同一路径。
- 全计划"WP 间无代码依赖"总原则下，这是**唯一显式例外**（Batch 2 的 delay →
  Batch 1 的路径改造）；因此 delay 排 Batch 2。
- 与 `04`（HTTP parity）并行无冲突：`delay_ms` 只是透传参数，`04` 不依赖本包
  引擎实现细节；与 `05`（按需池）无任何交集。

批次时序（意图）：

| 时点 | 动作 |
|---|---|
| Batch 1 | `01` 完成 `move_family` / 族删除路径改造（0x20 折叠先例落地） |
| Batch 2 开工 | 本包在同一折叠路径上追加 0x1D 段 + 暂存引擎 + e2e |
| Batch 2 收口 | `04` 的 `delay_ms` 透传与本包 e2e 对齐断言口径 |

## 9. 兼容

- **暂存行损坏防御**：沿用 `src/lite/model.rs` 损坏计数模式——暂存行解码失败
  计入损坏计数并跳过（不投递、不 panic），单条毒数据不拖垮扫描器。
- **升级无损**：旧版本数据不含 0x1D 行，暂存窗口为空、扫描器空转；`classify`
  登记只影响新写入的暂存行识别。
- 边界取值（意图）：`ms` 按无符号整数解析，超范围/非整数/负数一律解析期报错；
  到期时刻溢出（极端大 ms）按解析期报错处理，不静默截断。
- 已知取舍（记入 `COMPAT.md`）：`classify` 把 0x1D 纳入 typed 集后，首字节恰为
  `0x1D` 的**历史裸字符串**会被误读为 typed 记录——与 0x20（空格）同类，
  属 `src/ds/codec.rs` 注记的既有 accepted breaking change 家族，实际裸字符串
  首字节落入该区间的概率可忽略。
- 无 `DELAY` 的 XADD 行为逐字节不变；RESP 无新增命令名（XADD 既有命令的选项
  扩展）。

## 10. e2e 验收：`tests/lite_delay_e2e.rs`

- 乱序提交按 due 投递：先提交 `DELAY 5000`、后提交 `DELAY 100`，后者先到期
  先投；两者均到期后按 due 序写入（entry id 单调）；
- 未到期不可见：XREADGROUP `>` 空、XRANGE/XLEN 不含暂存消息；
- 未到期 XREADGROUP BLOCK 不醒、到期唤醒（阻塞等待经 `src/lite/park_wait.rs`
  既有辅助——到期写入目标流后唤醒 park 的消费者）；
- kill -9 恢复不双投：批前/批中/批后重启，暂存行与目标流 entry 恒两态归一
  （见第 4 节），无双份、无丢失；
- **XIDLE · RENAME · FLUSHDB 交互漏删回归（强制，第 5 节）**：三场景逐一断言
  暂存行随流删除、到期后无投递、暂存窗口为空；
  - XIDLE：设 idle TTL 后等到期整流，暂存行随之删除；
  - RENAME：改名后暂存行随迁新名（due 与消息体不变），旧名窗口为空；
  - FLUSHDB：清库后暂存窗口为空，且到期后无任何复活投递；
- HTTP `delay_ms` 透传：HTTP 面提交带 `delay_ms` 的消息，行为与 RESP `DELAY`
  一致（用例与 `04` 共享断言口径）；
- 负矩阵：`DELAY` 非整数/负数报参数错误；`DELAY 0` 等价无延迟（同步可见）；
  kafka 面无延迟位（Produce 面不暴露延迟参数，Fetch 只见到期后普通消息）。

## 11. 验收命令与回归门

```sh
cargo test --test lite_delay_e2e
cargo test --workspace --no-fail-fast            # CI 同款
cargo fmt --check
cargo clippy --workspace -- -D warnings
```

回归门：`lite_ordered_e2e` / `flushdb_lite_e2e` 全绿（键族删除/整流路径的直接
相关回归）；另含 `lite_pel_e2e` / `lite_group_e2e` / `lite_e2e` /
`lite_streams_e2e` 与 `kafka_*` 全量回归（延迟到期写入走普通 entry 路径，
kafka 面行为不应有任何变化）。命令集与 `01` 第 11 节一致。

分项门：本包引擎改动（`delay.rs` + codec/expire/keys_core 小改）与 `04` 的
HTTP 透传为两个独立 PR，合入前各自单跑 `lite_delay_e2e` / HTTP e2e；Batch 2
收口时统一跑全量门。

## 12. 文档同步与 changelog 草稿

- `features/mq-lite.md`：规范节新增"延迟消息"（DELAY 选项、暂存 kind 0x1D、
  到期批原子性、kafka 面不暴露理由）；
- `features/rocksmq-http.md`：`delay_ms` 参数说明（与 `04` 对齐）；
- `features/e2e-coverage.md`：登记 `lite_delay_e2e.rs`；
- `COMPAT.md`：延迟语义条目 + `classify` 0x1D 分类取舍；
- `agents/rust/index.md`：`src/lite/` 模块族补 `delay.rs`；
- changelog 条目草稿（日期 2026-XX-XX 占位，conventional commit 风格）：
  ```text
  feat(lite): delay messages via XADD DELAY staging (kind 0x1D)
  feat(http): delay_ms passthrough for delayed messages   # 与 04 协同
  ```
  条目正文模板（占位）：动机（B 级差距：无延迟语义）→ 语义（前置选项/到期
  可见性/due 序投递）→ 原子性与恢复（单批两态归一）→ 兼容（无 DELAY 不变、
  升级无损、kafka 面不暴露）→ 验证（e2e 清单 + 回归门结果）。
