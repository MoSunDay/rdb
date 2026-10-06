# 06 — e2e 总表 + 回归清单 + 验收命令

> 状态：proposed
> 日期：2026-10-06
> 性质：验收总表

## 0. 定位

- 本文档是 MQ 能力差距计划（00–05 号）的**验收收口**：8 个新增 e2e 文件的总表与
  逐用例 checklist、shell 场景回归清单、既有回归门、验收命令块、文档同步矩阵与
  提交策略；
- 本表是 MQ 差距计划验收的**唯一登记处**：工作包（01–04）的用例全部在此；按需池
  （05 号）条目立项时在 §8 追加区登记，不另立台账；
- 执行批次：Batch 1 = 01 全部 + 03 的 headers 回放；Batch 2 = 02 + 04 + 03 其余；
  Batch 3 = P3 按需（触发条件驱动，见 05 号）。本批（文档批）只落 plans/，不写代码。

| 批次 | 内容 | 新 e2e |
|---|---|---|
| 文档批（本批） | plans/ 8 份计划文档落盘 | 无 |
| Batch 1 | 01 号全部 + 03 号 headers 回放 | 5 个：`lite_dlq` / `lite_redeliver` / `lite_trim_minid` / `kafka_headers_roundtrip` / `kafka_rename_ledger` |
| Batch 2 | 02 号 + 04 号 + 03 号其余 | 3 个：`lite_delay` / `kafka_admin` / `rocksmq_wait_pending`（覆盖延迟、kafka admin/SASL、rocksmq 长轮询+pending、token 鉴权 4 类验收点） |
| Batch 3 | P3 按需 | 触发后按 05 号立项，用例进 §8 追加区 |

## 1. 新增 e2e 总表

| 测试文件 | 覆盖点 | 关联 WP 文档 | 执行批次 | 预算 |
|---|---|---|---|---|
| `lite_dlq_e2e.rs` | 转移触发/原子性+水位推进/重复 claim 不双转/ordered 仅头/kill -9 一致性/DLQ 可独立消费/`rdb_lite_dlq_depth` gauge | 01 | Batch 1 | 每文件 <400 行（新建文件硬上限） |
| `lite_redeliver_e2e.rs` | idle>backoff 自动重投/默认关/与手动 XCLAIM 并存/不重投已 ack | 01 | Batch 1 | 同上 |
| `lite_trim_minid_e2e.rs` | MINID `[=~]`/LIMIT/时间窗留存/kafka 守卫拒绝 | 01 | Batch 1 | 同上 |
| `kafka_headers_roundtrip_e2e.rs` | 真 headers 回放/tombstone/存量 envelope 兼容 | 03 | Batch 1 | 同上 |
| `kafka_rename_ledger_e2e.rs` | RENAME 随迁/旧名提交失败/新名 offset 保留 | 01 | Batch 1 | 同上 |
| `lite_delay_e2e.rs` | 乱序提交按 due 投递/未到期 BLOCK 不醒·到期唤醒/kill -9 恢复/XIDLE·RENAME·FLUSHDB 交互漏删回归/HTTP delay_ms | 02 | Batch 2 | 同上 |
| `kafka_admin_e2e.rs` | ListGroups/DeleteGroups wire/组删除清账本/SASL 401+成功矩阵 | 03 | Batch 2 | 同上 |
| `rocksmq_wait_pending_e2e.rs` | wait_ms 超时与即回//pending/token 401 | 04 | Batch 2 | 同上 |

### 1.1 测试基建落点

沿用既有分层，不新造 harness：

- `lite_*` 三个新文件：进程级 e2e 形态（`tests/common/`），与既有
  `tests/lite_pel_e2e.rs`、`tests/lite_ordered_e2e.rs` 同构；
- `kafka_*` 两个新文件：前端 mini-harness 形态（`tests/kafka_front_common/`），
  与既有 `tests/kafka_produce_e2e.rs` 等同构；
- `rocksmq_wait_pending_e2e.rs`：与 `tests/rocksmq_http_e2e.rs` 同款手搓 HTTP
  客户端形态；token 401 用例的测试形态参照 `tests/es_auth_e2e.rs`。

### 1.2 工程约束（每个新文件都要过）

- 每份新 e2e 文件 **<400 行**（硬上限，超限先按用例域拆分）；
- 纯函数式风格：**禁 class**，用纯函数与组合组织断言/脚手架；
- **无敏感默认值**：token / SASL 凭据等一律用测试内生成的临时值，不落任何真实
  凭据形态的默认配置。

## 2. 每文件用例 checklist

### 2.1 `lite_dlq_e2e.rs`（WP1 / Batch 1）

- [ ] 转移触发：投递次数达到 maxdelivery 的 pending 条目被转入 DLQ，原流可投递面
      不再含该 id；
- [ ] 阈值边界：deliveries 恰好达到 maxdelivery 时转移、差一次时不转移的对照断言；
- [ ] 原子性 + 水位推进：转移与 PEL 清账/组水位推进同一写批次内完成，断言不存在
      "已转移但仍在 PEL"的中间态；
- [ ] 重复 claim 不双转：同一 id 经两条 claim 路径先后触达，DLQ 深度恒为 1；
- [ ] ordered 仅头：ORDERED 组超限转移只作用于队头阻塞项，非队头 pending 项保持
      原位不动；
- [ ] kill -9 一致性：负载窗口间 SIGKILL + 同 store/配置复活（进程级手法参照
      `scrtips/e2e_scenarios/soak_kill9.sh`），复活后 DLQ 深度可复算、无半转移态；
- [ ] DLQ 可独立消费：DLQ 流以独立组名 XREADGROUP 消费并 XACK，全程不回写业务流；
- [ ] `rdb_lite_dlq_depth` gauge：转移后深度 +1、DLQ 独立消费 ack 后回落，从
      /metrics 采样断言。

### 2.2 `lite_redeliver_e2e.rs`（WP1 / Batch 1）

- [ ] idle>backoff 自动重投：pending 条目空闲超阈值后被 XREADGROUP `>` 重新投出，
      deliveries 计数递增；
- [ ] 默认关：未配置 backoff 时同样 idle 的条目不重投（默认行为回归锚）；
- [ ] 与手动 XCLAIM 并存：手动 claim 重置 idle 计时，不与自动重投双发同一条；
- [ ] 不重投已 ack：已确认条目在自动/手动任一路径都不再出现；
- [ ] 重投归属：自动重投仍发生在同一消费组语义内，不跨组/跨流污染。

### 2.3 `lite_trim_minid_e2e.rs`（WP1 / Batch 1）

- [ ] MINID `[=~]`：精确 `=` 仅删 id 严格小于给定 id 的条目并返回删除数；近似 `~`
      允许按内部粒度偏移，但保留下一条 ≥ minid 的条目；
- [ ] LIMIT：单次删除条数受 LIMIT 上限约束，剩余部分可再次 TRIM 清尾；
- [ ] 时间窗留存：以"当前时间 − 窗口"换算 ms-id 作为 minid，实现按时间窗留存；
- [ ] kafka 守卫拒绝：对 kafka 映射流执行 XTRIM MINID 被拒绝，不破坏
      offset↔序号对应关系。

### 2.4 `kafka_headers_roundtrip_e2e.rs`（WP3 / Batch 1）

- [ ] 真 headers 回放：produce 携带 headers，fetch 解出的记录 headers 键值与写入
      字节一致（不再折叠进 value envelope）；
- [ ] tombstone：value 为 null 的消息 produce→fetch 回放仍为 null，不落成空串；
- [ ] 存量 envelope 兼容：旧 envelope 形态写入的存量数据 fetch 后仍按原 key/value
      还原，不发生二次解包。

### 2.5 `kafka_rename_ledger_e2e.rs`（WP1 / Batch 1）

- [ ] RENAME 随迁：RENAME parent 流后，该流的提交偏移账本条目随迁至新名；
- [ ] 旧名提交失败：对旧名的 OffsetCommit 被拒绝，不产生悬挂账目；
- [ ] 新名 offset 保留：RENAME 后对新名 OffsetFetch 读到迁移前提交值；
- [ ] 无提交流对照：无账本条目的流 RENAME 不报错，新名 OffsetFetch 回缺省。

### 2.6 `lite_delay_e2e.rs`（WP2 / Batch 2）

- [ ] 乱序提交按 due 投递：不同 delay 的消息按到期先后投出，与提交顺序无关；
- [ ] 未到期 BLOCK 不醒：仅有未到期消息时 XREAD BLOCK 空转到超时，不提前返回；
- [ ] 到期唤醒：消息到期后阻塞读被唤醒并拿到该条；
- [ ] kill -9 恢复：SIGKILL + 同 store 复活（进程级手法参照
      `scrtips/e2e_scenarios/soak_kill9.sh`）后，未到期索引完整、已到期即可投递；
- [ ] XIDLE·RENAME·FLUSHDB 交互漏删回归：XIDLE 观测、RENAME 随迁、FLUSHDB 清理
      三路径与 delay 索引交叉时无漏删/悬挂条目；
- [ ] HTTP delay_ms：`POST /produce` 带 `delay_ms` 等价于 Lite delay 提交，投递
      同样按 due 生效。

### 2.7 `kafka_admin_e2e.rs`（WP3 / Batch 2）

- [ ] ListGroups wire：列出存在提交/成员记录的组，响应形态合面规范；
- [ ] DeleteGroups wire：删除指定组；未知组按面规范错误应答；
- [ ] 组删除清账本：DeleteGroups 后该组提交偏移账本清空，OffsetFetch 回缺省；
- [ ] SASL 401：未通过握手鉴权的请求被拒（401 语义的拒绝应答；测试形态参照
      `tests/es_auth_e2e.rs` 的 token 401 矩阵）；
- [ ] SASL 成功矩阵：合法凭据下 Produce/Fetch/组面/OffsetCommit 全链成功；
- [ ] 广告面一致性：ApiVersions 广告含新增 admin API，版本区间合面规范、旧 API
      区间不变。

### 2.8 `rocksmq_wait_pending_e2e.rs`（WP4 / Batch 2）

- [ ] wait_ms 超时：无消息时 /consume 挂起至 wait_ms 到期返回空集；
- [ ] wait_ms 即回：已有消息时立即返回，不做空等；
- [ ] /pending：查询组内未确认清单（id 维度），与 Lite PEL 一致；
- [ ] token 401：开启 token 后未带/错带凭据的 HTTP 请求被拒（测试形态参照
      `tests/es_auth_e2e.rs`）；
- [ ] 无 group 路径对照：不带 group 的尾部拉取与带 group 路径在 `wait_ms`
      超时/即回上行为同规格。

### 2.9 用例编写惯例

- 每条 checklist 至少展开为一个独立测试函数，**一断言意图一函数**，不写大而全的
  神测试；
- kill -9 用例统一以 `kill9` 入名，并在注释注明参照
  `scrtips/e2e_scenarios/soak_kill9.sh` 的"负载窗口间 SIGKILL + 同 store/配置复活"
  手法；
- 鉴权/401 用例复用 `tests/es_auth_e2e.rs` 的形态：测试内临时注入凭据配置，缺失与
  错值双路径各一断言；
- 断言精度对齐仓内既有水准：能比字节/结构（RESP 原始帧、wire 解码结构、JSON 字段）
  就不退化为"包含子串"；
- 涉及时间的断言（backoff、delay、wait_ms）用受控时钟或显式等待上界，不裸 sleep
  依赖机器负载。

## 3. 交互回归清单（shell 场景）

- **`scrtips/e2e_scenarios/scenario_lite_mq.sh` 增段**：在既有（a）–（f） 段之后
  追加三段——DLQ（转移 + 深度 + 独立消费）、delay（到期投递 + 未到期不投）、
  MINID（时间窗留存）；沿用脚本既有惯例：真实 redis-cli 驱动、每条断言先对
  源码/测试核验再落字；
- **`scrtips/e2e_scenarios/scenario_kafka_sdk.sh` 重跑**：03 号 admin 面落地后
  广告面 +2 API 变化（ListGroups/DeleteGroups wire），需以真实 SDK
  （confluent-kafka）重放既有链路并确认新 API 广告不破坏旧客户端；SASL 开启后
  补成功/401 矩阵一段；
- **`scrtips/e2e_scenarios/run_all.sh` 收口**：run_all 以 glob 收
  `scenario_*.sh`，本计划**不新增**场景脚本文件、只改既有两个；收口动作 = 完整
  跑一遍 run_all，确认 glob 生效、场景数与 RESULTS.md 无漂移措辞。

## 4. 既有回归门（Batch 1 须全绿）

| 既有套件 | 把守的交互面 |
|---|---|
| `lite_ordered_e2e` | ORDERED 严格串行组：DLQ 转移/maxdelivery 不得破坏队头语义 |
| `flushdb_lite_e2e` | FLUSHDB 与 Lite 元数据清理（delay 索引交互另有 §2.6 专项回归） |
| `lite_pel_e2e` | PEL 基线：重投/转移不改变 XPENDING 概要与 range 形态 |
| `lite_group_e2e` | 组生命周期基线：RENAME 随迁/组删除清账本的对照组 |
| `kafka_produce_e2e` | produce 基线：headers 改造不破坏无 headers 路径 |
| `kafka_fetch_e2e` | fetch 基线：记录形态与 tombstone 语义不回归 |
| `kafka_group_e2e` | 组面 wire 基线：admin 面扩展不改既有响应形态 |
| `kafka_offsets_e2e` | ListOffsets/OffsetFetch 基线：账本随迁的对照组 |
| `kafka_wire_e2e` | wire 总线：版本注册表与广告面无意外变化 |
| `backup_readonly_e2e` / `backup_surface_e2e` | 备份只读门：本计划**无新增命令名**（DLQ/重投为引擎内部行为与既有动词子句，admin/wait_pending 为非 RESP 面），readonly allowlist 无需扩；`backup_surface_e2e` 内建"表与 `readonly::ALLOWED` 集合相等"运行时断言自动把关——验证方式即原样跑绿，记录在案 |

- Batch 1 合入前上表**全绿**；Batch 2/P3 批次沿用同一门（防叠加回归）；
- 任一门转红：先判定是"预期行为变化需更新断言"还是"真回归"，前者须在对应
  WP 文档与 `features/` 规范同步后改断言，后者直接阻断合入。

门的执行方式（与 §5 单跑命令同形，逐个过）：

```bash
cargo test --test lite_ordered_e2e
cargo test --test flushdb_lite_e2e
cargo test --test lite_pel_e2e
cargo test --test lite_group_e2e
cargo test --test kafka_produce_e2e
cargo test --test kafka_fetch_e2e
cargo test --test kafka_group_e2e
cargo test --test kafka_offsets_e2e
cargo test --test kafka_wire_e2e
cargo test --test backup_readonly_e2e
cargo test --test backup_surface_e2e
```

## 5. 验收命令

每批合入前的全量验收（顺序：单跑 → 全量 → 静态检查 → 场景脚本）：

```bash
cargo test --test lite_dlq_e2e            # 新 e2e 单跑（其余同理）
cargo test --workspace --no-fail-fast     # CI 同款全量
cargo fmt --check
cargo clippy --workspace -- -D warnings
```

场景脚本（shell 层回归，Batch 1/2 各跑一遍）：

```bash
bash scrtips/e2e_scenarios/scenario_lite_mq.sh
bash scrtips/e2e_scenarios/scenario_kafka_sdk.sh
```

新文件逐个单跑的等价循环（批内自查用）：

```bash
for t in lite_dlq_e2e lite_redeliver_e2e lite_trim_minid_e2e \
         kafka_headers_roundtrip_e2e kafka_rename_ledger_e2e \
         lite_delay_e2e kafka_admin_e2e rocksmq_wait_pending_e2e; do
  cargo test --test "$t"
done
```

- `cargo test --test <name>` 逐个新文件单跑便于定位；全部新文件合入后以
  `--workspace --no-fail-fast` 作批次门；
- 场景脚本依赖真实客户端（redis-cli / confluent-kafka），缺失时按脚本内既有
  自跳过逻辑处理，不作为硬门。

## 6. 文档同步矩阵（随各批次落地）

| 文档 | 批次 | 变更点 |
|---|---|---|
| `features/e2e-coverage.md` | Batch 1 起累计 | MQ 缺口**首次入台账**；每批追加对应套件说明并更新残余清单 |
| `features/mq-lite.md` | Batch 1 / Batch 2 | DLQ/重投/MINID（B1）；delay 与交互语义（B2） |
| `features/kafka-front.md` | Batch 1 / Batch 2 | headers 销账（B1）；admin/SASL/两缺陷修复（B2） |
| `features/rocksmq-http.md` | Batch 2 | `wait_ms`/`/pending`/token/`delay_ms` |
| `COMPAT.md` | 各批 | 行为差异增补（重投默认关、MINID 语义、SASL 行为等） |
| `agents/rust/index.md` | 各批 | 模块索引更新（新模块/新文件） |
| `features/changelog/` | 每批一条 | `YYYY-MM-DD/{topic}.md`（约定见其 README.md） |

- 文档同步与实现**同批合入**，不滞后到批末补写；
- `features/e2e-coverage.md` 的台账更新从 Batch 1 第一批合入起算（MQ 缺口首次
  进台账），此后每批累计，不在 Batch 2 一次性补齐。

## 7. 提交策略

- **本批为纯文档批**：只落 `plans/`（8 份计划文档），无代码、无配置改动；
- 实现按批次提交，**每批合入前跑 §5 全量验收命令块**：
  - Batch 1：01 号全部 + 03 号 headers 回放（5 个新 e2e）；
  - Batch 2：02 + 04 + 03 号其余（3 个新 e2e 文件，覆盖延迟、kafka admin/SASL、
    rocksmq 长轮询+pending、token 鉴权 4 类验收点）；
  - Batch 3：P3 按需（05 号触发条件驱动，无预排内容）；
- commit message 用 conventional commits，scope 对应模块，示例：

```
feat(lite): dead-letter queue and maxdelivery
fix(kafka): replay real record headers
feat(lite): trim by minid with time-window retention
feat(lite): delayed message delivery by due time
feat(kafka): group admin apis and sasl handshake
feat(rocksmq): wait_ms long-poll, pending and token gate
docs(features): sync mq gap batch 1 landing
```

- 一个工作包可拆多个 commit，但**不得跨批混提**（Batch 1 与 Batch 2 的改动不同 PR）；
- 每批收尾三件事：验收命令块全绿 → `features/` 文档同步（§6）→ changelog 落条目。

## 8. 追加区（按需池立项条目登记处）

- 05 号池条目立项时，在 §1 总表下方追加行（测试文件/覆盖点/关联计划文档/批次/
  预算），并在 §2 增 checklist 小节；
- 追加条目同样受 §1.2 工程约束与 §4 既有回归门约束；
- 当前（本批）**无追加**：P3 按需池无立项，Batch 3 无预排内容。
