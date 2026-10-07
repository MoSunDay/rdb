Commit: b0d81c9

# plans/：前瞻计划索引

> 状态：proposed
> 日期：2026-10-06

## 目录定位

- 本目录存放**前瞻性计划**：状态为 proposed / accepted / 部分落地的工程计划
  （差距矩阵、工作包拆分、e2e 规划），是"准备做什么"的决策现场。
- 与 `features/`（**已落地能力台账**，"已经有什么"）互补：一项能力只有在代码合入
  后才进入 `features/` 对应文档；计划期间以本目录为准。
- 全量落地后的收尾动作：
  - 摘要归档至 `features/changelog/`，按其 `YYYY-MM-DD/{topic}.md` 约定
    （见 `features/changelog/README.md`）；
  - 计划原文**保留在本目录**供追溯，状态推进为 landed，不改写历史内容；
  - 部分落地时推进为 archived，未落地项**回写按需池**（各计划的 P3 池文档），
    由后续计划按触发条件重新拾起。

## 命名与索引

- 计划目录命名：`plans/YYYY-MM-DD-slug/`，日期为计划创建日。
- 计划内文档命名：`NN-topic.md`（两位序号 + 短横线主题），序号即阅读顺序；
  `00` 号约定为总览/差距矩阵，其后为各工作包或专项文档。
- 本 `index.md` 为 **plans/ 唯一索引**：不设子目录索引，新增计划时在此追加一行。

## 格式规范

- 对齐 `features/changelog/` 既有风格：中文叙述、`##`/`###` 小节、Markdown 表格、
  佐证路径一律用反引号包裹、命令与配置示例用 fenced code block。
- 每份文档 **<400 行**；超出时按职责拆分为多份工作包文档，而非加厚单份。
- 每份文档头部含元信息块：`状态：`、`日期：`，总览类文档另含 `关联：`
  指向其下辖的工作包文档。
- 佐证路径与行号以撰写时仓库快照为准；不确定处用描述性语言，不编造行号/签名。

## 状态模型

| 状态 | 含义 | 进入条件 |
| --- | --- | --- |
| proposed | 已成文、待评审 | 计划文档写入本目录 |
| accepted | 评审通过、已排期 | 评审通过并确定执行批次 |
| 部分落地 | 执行批次部分合入、计划仍开放 | 首个批次落地即进入；摘要已归档 `features/changelog/`，未落地项继续留在计划内推进 |
| landed | 已全量落地 | 摘要已归档至 `features/changelog/`，原文保留供追溯 |
| archived | 部分落地、计划关闭 | 未做项已回写按需池，后续按触发条件重新拾起 |

状态流转：proposed → accepted → landed；accepted → 部分落地 → landed/archived
（部分落地为执行中间态，批次全部合入后推进 landed，提前关闭则 archived）。

## 当前计划索引

| 计划目录 | 状态 | 文档数 | 一句话简介 |
| --- | --- | --- | --- |
| `2026-10-06-mq-gap/` | 部分落地 | 8 | MQ 能力完备差距对比 + 新能力 e2e 计划（A/B 级入执行批次，C 级入按需池） |
| `2026-10-06-mysql-gap/` | landed | 8 | MySQL 常用功能缺口矩阵（A–G 域）+ M0–M5 里程碑：查询语义修复/表达式函数族/DML 冲突/子查询与集合操作/DDL 与会话面/e2e 补盲 |
| `2026-10-06-mysql-hardening/` | 部分落地 | 6 | M0–M5 评审缺陷修复（H0 静默错误结果清零已落地，H1 协议会话/H2 唯一性索引/H3 P2 功能/H4 测试卫生 proposed） |
| `2026-10-07-mq-p3-backfill/` | landed | 1 | P3 按需池全池补齐（总览即 README.md）：#1–#15 分诊（13 立项、#3 观望、#9 LIMIT 语义收缩），W0 conf → W1 三车道（kafka/lite/http）→ W2 消费者 idle GC → W4 台账收尾 |

`2026-10-06-mq-gap/` 背景：以常用 MQ 功能清单为尺，对 Lite（RESP 动词面）、
Kafka（wire 面）、HTTP（rocksmq 面）三面做只读差距复核，产出分级处置矩阵；
A 级（引擎可靠性 + 已登记缺陷）与 B 级（常用语义补齐）进入执行批次，
C 级与"显式不做"进入按需池固化，避免无触发条件的功能蔓延。

> 2026-10-06 注记（mysql-gap）：M0–M5 六个里程碑全量落地（41 项 P0/P1 全部合入，
> P2 暂缓项与理由见 `gap-matrix.md`）；逐里程碑摘要归档
> `features/changelog/2026-10-06/mysql-m{0..5}-*.md`，SQL 契约与偏差台账拆出为
> `COMPAT.sql.md`，e2e 台账见 `features/e2e-coverage.md`。

> 2026-10-06 注记：Batch 1（WP1 引擎可靠性全部 + WP3 headers 真回放）已落地，
> 摘要归档 `features/changelog/2026-10-06/mq-engine-batch1.md`；Batch 2
> （延迟消息/HTTP parity/kafka admin）未开工，计划继续有效。

内部文档：

| 文档 | 一句话简介 |
| --- | --- |
| `00-gap-matrix.md` | 差距矩阵总览：复核结论 + A/B/C 级处置 + 三面覆盖视图 + 批次映射 |
| `01-engine-reliability.md` | WP1 引擎可靠性：DLQ/最大投递次数、自动重投调度、XTRIM MINID、RENAME 搬 0x20 账本、kafka 面流 XTRIM/XDEL 守卫 |
| `02-delay-messages.md` | WP2 延迟消息：XADD DELAY 前置选项、暂存 kind 0x1D + due 扫描器、键族删除登记强制项 |
| `03-kafka-parity.md` | WP3 kafka parity：headers 真回放（Batch 1）、ListGroups/DeleteGroups、SASL PLAIN token 鉴权 |
| `04-http-parity.md` | WP4 HTTP parity：wait_ms 长轮询 consume、pending 可见性、rocksmq_token Bearer 鉴权 |
| `05-p3-pool.md` | P3 按需池：C 级条目逐条触发条件 + 显式不做清单（含理由） |
| `06-e2e-matrix.md` | e2e 矩阵：8 个新增 e2e 文件与既有回归套件的覆盖映射及文档同步面 |

## 执行批次概览

> **易混淆点**：工作包文档编号（01–05）是**内容划分**，执行批次（Batch 1–3）是
> **代码提交批次**，两者不同。

| 批次 | 范围 | 新增 e2e |
| --- | --- | --- |
| Batch 1 | `01` 全部 + `03` 的 headers 回放缺陷修复 | 5 个新 e2e 文件 |
| Batch 2 | `02` + `04` + `03` 其余（admin/SASL） | 3 个新测试文件，覆盖 4 类验收点：延迟、kafka admin/SASL、rocksmq 长轮询+pending、token 鉴权 |
| Batch 3 | P3 按需池（`05`）逐条评估落地 | 按触发条件另立 |

- WP 之间**无代码依赖**，可并行开发；唯一顺序约束：Batch 2 的 delay 依赖
  Batch 1 对 `family_delete` / `move_family` 路径的改造（详见 `00-gap-matrix.md`
  复核结论与批次映射节）。

## 新增计划的操作约定

1. 建目录 `plans/YYYY-MM-DD-slug/`，`00` 号文档承载总览/差距矩阵，工作包文档按
   序号排布，每份 <400 行。
2. 每份文档头部写元信息块（状态/日期/关联），首次写入状态一律 `proposed`。
3. 在本索引"当前计划索引"表追加一行，并为内部文档补一句话简介。
4. 评审通过改 `accepted` 并标注执行批次；落地后按"目录定位"节的收尾动作处理
   changelog 归档与按需池回写，状态改 `landed` 或 `archived`。
