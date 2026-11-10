# 2026-11-10 turso-acceptance：turso 引擎四项验收收口计划

> 状态：landed（2026-11-10 WP0–WP4 全量落地；单文档承载（README，mq-p3-backfill 惯例），
> 工作包不另拆号）
> 日期：2026-11-10（草案日期 2026-11-11，实际提前一天落地）
> 输入：`plans/2026-11-09-turso-engine/`（landed）+ allowlist batch 1
> （`features/changelog/2026-11-10/turso-allowlist-batch1.md`）
> 关联：落地摘要 `features/changelog/2026-11-10/turso-acceptance-closure.md`；
> 契约台账 `COMPAT.sql.md`（(z3) + ANN deviations 段）；模块地图 `agents/rust/sql.md`

turso-engine 主体（T0–T6）与 allowlist batch 1 落地后，上线前余下四项验收
目标的判定与收口：不新增 SQL 能力，全部是"打通 + 判定 + 台账"性质。

## 1. 背景：四项验收目标判定

1. **standalone 一体化**——单命令起全栈已就绪，但 turso 引擎需 yaml 双闸、
   缺 CLI 入口：判定 = 加 opt-in 旗标打通，缺省行为逐字节不变。
2. **SQLite 双通道互操作**——turso 文件格式宣称 SQLite 兼容但未实证：判定 =
   以宿主 stock sqlite3 CLI 为对端做文件级双向互操作 e2e，实测姿势入台账。
3. **向量可用口径**——turso 侧已有精确 KNN + 函数族、ANN 未实现：判定 =
   "可用"= 精确 KNN + 函数族（parity e2e'd），ANN 显式 non-goal（rocks 侧
   SQ8/SPANN 已在案，turso 侧 ANN 另立计划）。
4. **上线门槛**——判定 = CI 三段门全绿 + 文档四方同步（COMPAT.sql.md /
   e2e-coverage / agents/rust / changelog）。

## 2. 工作包 WP0–WP4

- **WP0 fmt chore**（`426769c`）：standalone + turso e2e 文件 rustfmt 重排，
  无语义变化。
- **WP1 standalone 旗标**（`81ca5a9`）：`rdb standalone --turso-db-dir PATH`
  opt-in——出现即翻 `turso_engine` 并接线 `turso_db_dir`，boot 预建目录，
  `endpoints()` 日志 `turso=<dir>|off`；单测扩展，`standalone_e2e` 2→3。
- **WP2 互操作 e2e**（`6a06c5b`）：新 `turso_sqlite_interop_e2e`（2，方向
  A/B 见落地摘要）；访问姿势落台账 (z3)。
- **WP3 向量口径**：`COMPAT.sql.md` ANN deviations 段补 turso posture。
- **WP4 文档四方同步**：e2e-coverage / `agents/rust/sql.md` /
  `agents/rust/index.md` / changelog + 本计划与 plans 索引。

## 3. 决策

- **D1**：SQLite 通道=文件级，不引入 hrana/sqld wire——stock sqlite3 CLI
  （≥3.37.2）即互操作对端，turso 私有 wire 协议出契约。
- **D2**：standalone turso 默认关闭——`--turso-db-dir` 为 opt-in 旗标，缺省
  逐字节不变（双闸哲学不动）。
- **D3**：并发读受限则文档化姿势——实测落点=写者独占（rdb 存活期外部读
  SQLITE_BUSY 持续，退出后 WAL 恢复全量可读），落 (z3)，不引入并发机制。

## 4. 结果与证据

| 工作包 | 证据 |
| --- | --- |
| WP0 | commit `426769c` |
| WP1 | commit `81ca5a9`；`standalone_boots_turso_engine_with_flag`（e2e 3 用例） |
| WP2 | commit `6a06c5b`；`turso_sqlite_interop_e2e`（2 用例）+ `COMPAT.sql.md` (z3) |
| WP3 | `COMPAT.sql.md` ANN deviations 段 turso posture 句 |
| WP4 | `features/e2e-coverage.md` / `agents/rust/sql.md` / `agents/rust/index.md` / changelog 摘要 |

CI 三段门（fmt / clippy -D warnings / `cargo test --workspace`）全绿；宿主缺
sqlite3 CLI 时互操作套件自跳过。
