# 2026-11-10 · turso 引擎验收收口（standalone 一体化 / SQLite 互操作 / 向量口径 / 上线门槛）

来源计划：`plans/2026-11-10-turso-acceptance/`（四项验收收口；原草案日期
2026-11-11，实际提前至 2026-11-10 落地）；互操作契约台账 (z3) 与向量口径：
`COMPAT.sql.md`；e2e 台账：`features/e2e-coverage.md`。

## 落了什么

- **WP0 fmt chore（426769c）**：standalone + turso e2e 文件 rustfmt 重排
  （无语义变化），清出基线。
- **WP1 standalone 一体化（81ca5a9）**：`rdb standalone --turso-db-dir PATH`
  opt-in 旗标——出现即翻 conf `turso_engine=true` 并接线 `turso_db_dir`
  （缺省逐字节关闭，沿双闸哲学），boot 预创建目录，`endpoints()` 日志
  `turso=<dir>|off`；`standalone_e2e` 2→3（旗标启动 → CREATE DATABASE/USE/
  DDL/DML 往返 + 盘上 `<dir>/<name>.db` 断言）。
- **WP2 SQLite 双通道互操作 e2e（6a06c5b）**：新 `turso_sqlite_interop_e2e`
  （2）——方向 A MySQL 写 → stock sqlite3 CLI 读（存活期外部读被拒
  SQLITE_BUSY：turso WAL 写者独占、无 -shm；优雅退出后 CLI 恢复 WAL 全量
  可读，`hex()` 钉死裸 LE f32 向量布局）；方向 B stock CLI 预建 `.db` 经
  `CREATE DATABASE IF NOT EXISTS` 收养、MySQL 侧 `VECTOR_TO_STRING`/
  `VECTOR_DIM` 读回。访问姿势落台账 (z3)（宿主 sqlite3 版本下限 3.37.2、
  rdb 独占写者、hrana/sqld 出契约）。

## 向量"可用"口径（WP3）

turso 引擎向量"可用"= 精确 KNN + 全函数族（parity e2e'd，`turso_vector_e2e`）；
ANN（`CREATE VECTOR INDEX`）在 turso 侧显式 non-goal——保持 1235（rocks 侧
SQ8/SPANN 已在案，turso 侧 ANN 另立计划）；向量 blob 另经文件级 SQLite 通道
跨界（(z3)）。已写入 `COMPAT.sql.md` ANN deviations 段。

## 验证

- CI 三段门全绿：`cargo fmt --check` / `cargo clippy --workspace
  --all-targets -- -D warnings` / `cargo test --workspace --no-fail-fast`
  （含新 `turso_sqlite_interop_e2e` 2 用例与
  `standalone_boots_turso_engine_with_flag`）。
- 宿主缺 sqlite3 CLI（或版本 <3.37.2）时互操作套件自跳过，不作硬依赖。
- 验证在独立 worktree 内完成：主工作树有并行会话 WIP（es/search 面），未触碰。

## 决策（一句话版）

- D1：SQLite 通道=文件级，不引入 hrana/sqld wire（stock sqlite3 ≥3.37.2 即对端）。
- D2：standalone turso 默认关闭，`--turso-db-dir` opt-in（双闸哲学不动）。
- D3：并发读受限的实测落点=写者独占（存活期 SQLITE_BUSY 持续，退出后 WAL
  恢复全量可读），文档化姿势即 (z3)，不引入并发读机制。

## 文档四方同步（WP4）

`COMPAT.sql.md`（ANN deviations 段 turso 口径 + (z3)）、`features/e2e-coverage.md`
（turso 块八套件 + interop 条目）、`agents/rust/sql.md`（互操作契约 + 套件
枚举）、`agents/rust/index.md`（standalone 旗标一句）。
