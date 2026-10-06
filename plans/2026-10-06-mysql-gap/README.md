# MySQL 兼容差距收敛计划（2026-10-06）

针对 Rust SQL 层（`src/sql/`：`parse` / `exec` / `front` / `plan` / `dist` / `index` / `columnar` / `tx`）
与 MySQL 语义差距的调查已完成，本目录把调查结论固化为可执行的里程碑计划。核心判断：

- 差距分两类：**行为静默错误**（silent wrong behavior，如 `ORDER BY 1` 被当常量、别名解析失败、
  `LIMIT ?` 拒绝、`FROM DUAL` 报表不存在）——最高优先级；以及**功能缺失**（函数族、DML 冲突子句、
  子查询、DDL/session 面等）。
- `src/sql/exec/expr.rs`(708)、`write.rs`(749)、`ddl.rs`(733)、`parse/translate.rs`(727) 四个文件
  已逼近 800 行迭代上限，任何功能补齐前必须先按职责拆分。
- 部分与 MySQL 不同的行为是**有意偏离**（byte-wise 大小写敏感、Int/Int 整除、PK 重复 INSERT 静默
  upsert），只记录、不修改。

## Scope

- In scope：表达式/函数族、DML 冲突子句、子查询与集合操作、DDL/session 表面积、查询语义修复、
  e2e 盲区补齐。全部条目见 `gap-matrix.md`。
- Out of scope：gap-matrix G 组（binlog/权限/caching_sha2/窗口函数/JSON/ENUM/SET 等），逐项给出理由。

## 文件索引

| 文件 | 内容 |
| --- | --- |
| `gap-matrix.md` | 全量差距矩阵（A–G 域，优先级 + 里程碑归属） |
| `m0-query-semantics.md` | M0：查询语义四项静默错误修复 |
| `m1-expression-functions.md` | M1：表达式/函数族（含 expr.rs 拆分） |
| `m2-dml-conflicts.md` | M2：ODKU / REPLACE / INSERT…SELECT（含 write.rs 拆分） |
| `m3-subquery-setops.md` | M3：EXISTS / 相关子查询 / INTERSECT / EXCEPT |
| `m4-ddl-session-surface.md` | M4：TRUNCATE / RENAME / ALTER INDEX / SHOW 面（含 ddl.rs 拆分） |
| `m5-e2e-blindspots.md` | M5：e2e 基建收敛 + outer join / failover 盲区 |

## 里程碑一览

- **M0** 查询语义修复：`ORDER BY 1`/`GROUP BY 1` 序数、ORDER BY/HAVING 别名解析、`LIMIT ?`
  占位符、`FROM DUAL` → 四项 P0 静默错误一次清零。
- **M1** 表达式与函数：拆分 `exec/expr.rs` → `exec/func/{mod,string,numeric,datetime}.rs` 纯函数
  注册表；补齐 CASE/IF 族、CAST/CONVERT、字符串/数值/日期时间函数族、`<=>`/REGEXP/位运算、
  GROUP_CONCAT（SEPARATOR）。
- **M2** DML 冲突：拆分 `exec/write.rs` → `upsert.rs`（ODKU + REPLACE）与 `insert_select.rs`；
  唯一索引迁移、AUTO_INCREMENT、列存拒绝、集群 2PC 路径。
- **M3** 子查询与集合操作：`exec/subquery.rs` 加 EXISTS IR 与相关外层行绑定；`exec/set_ops.rs`
  加 INTERSECT/EXCEPT（复用去重基建）。
- **M4** DDL/session 表面积：拆分 `exec/ddl.rs` → `ddl_alter.rs`（TRUNCATE/RENAME/ALTER INDEX）；
  `exec/show.rs` 加 SHOW CREATE TABLE；`front/vars.rs` 统一 sysvar 表 + SHOW DATABASES/VARIABLES/STATUS
  与 DATABASE()/USER()/CONNECTION_ID()。
- **M5** e2e 盲区：`tests/common/mysql.rs` 收敛 16 个 SQL e2e 文件重复的 ~100 行样板；
  新增 outer-join e2e 与杀 leader 的真实三进程 failover e2e。

## 验收标准

- `cargo test --workspace --no-fail-fast` 全绿（workspace 含全部 e2e）。
- gap-matrix 的 **P0/P1 全部落地**；**P2 逐项标注 deferred 及理由**（WITH RECURSIVE、ADD COLUMN、
  复合/前缀索引、KILL、GROUP_CONCAT 内层 ORDER BY 等）。
- 有意偏离项（byte-wise 大小写、整除、PK 重复静默 upsert）在 `COMPAT.md` SQL 章节有明确记录。
- 每个里程碑落地时同步：`COMPAT.md` SQL section、`agents/rust/sql.md`、`features/sql-dataplane.md`、
  `features/e2e-coverage.md`、`features/changelog/<落地日期>/`。

## 横切约束

- **行数上限**：新增文件 ≤400 行；迭代中文件 ≤800 行。M1/M2/M4 的第一步是拆分而不是追加
  （expr.rs 708 / write.rs 749 / ddl.rs 733 / translate.rs 727 均已逼近上限，直接追加必然超限）。
- **纯函数式**：不引入 class / OOP 封装（Rust 侧对应：不引入携带内部可变状态的 trait-object
  注册器）。函数注册表保持纯 dispatch（name → 纯函数的 match/查表），求值走
  `fn eval(&Expr, &FromScope, &[Value]) -> SqlResult<Value>` 这类显式传参返回值风格。
- 测试与实现同仓库演进：每个新模块带 `*_tests.rs`（拆出主文件，避免主文件超限）。
- 代码中不得出现任何敏感数据（连接串/密钥/口令字面量等）。

## 风险与决策点

1. **ODKU 集群路径**：冲突判定若需要 gather-read（读旧值算 affected rows / 更新列），集群模式下
   成本可能过高。若实测不可接受：集群对 ODKU **大声拒绝**（MySQL ER 1235 "unsupported"），单机
   先行；语义正确优先于覆盖面。
2. **保留 PK 重复 INSERT 静默 upsert**：这是 StarRocks PK 模型导入路径的既有依赖，不改为报错；
   ODKU 提供显式冲突路径（affected rows 1/2/0 可区分），`COMPAT.md` 记录两者关系。
3. **保留 byte-wise 大小写敏感**：LIKE 与比较按字节序（等价 MySQL binary collation），不做
   collation / 大小写折叠；理由是存储层无 collation 概念，引入即全局工程。
4. **GROUP_CONCAT 只支持 SEPARATOR**：内层 ORDER BY（`GROUP_CONCAT(x ORDER BY y)`）v1 延后
   （deferred），需要分组内排序基建，单独排期。
5. **TRUNCATE 按 DDL 语义**：隐式提交、事务内拒绝、不可回滚、重置 auto-increment；不提供
   可回滚的"事务型 TRUNCATE"开关。

## 执行顺序

```
docs（本目录 8 篇） → M0 → M1 → M2 → M3 → M4 ∥ M5
```

- M0 最先：纯解析/语义修复、无文件拆分依赖，立刻消除静默错误。
- M1 → M2 → M3 串行：M1 的 `exec/func/` 拆分是 M2 VALUES() 求值与 M3 相关子查询表达式求值的
  共同前置；M2 的 upsert 路径被 M3 的 INSERT…SELECT 复用。
- M4 与 M5 并行：DDL/session 面与 e2e 基建互不阻塞；M5 的 `tests/common/mysql.rs` 会被 M4 的
  新 e2e 直接消费（并行时 M4 e2e 可先本地样板、基建合入后切过去，或接受合并顺序约束）。
