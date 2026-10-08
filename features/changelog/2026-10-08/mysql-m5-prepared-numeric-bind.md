# SQL：mysql-gap M5 开放跟进项收口——prepared 二进制协议数值绑定不再断连

Commit: (working-tree, 随本提交入库)

## 背景

`plans/2026-10-06-mysql-gap/` M5 e2e 发现的唯一连接级缺陷（gap-matrix"落地状态"
与 `COMPAT.sql.md` deviation ledger 在案）：二进制结果集按**公告列类型**打类型标记，
而引擎的静态结果类型是尽力而为（`?` 占位符在 EXECUTE 绑定前定型 VAR_STRING、
CASE 取首个 THEN 定型），运行期 cell 与公告列不符时 opensrv 的按类型编码器报
io 错——**整条连接被拖死**。复现：prepared `SELECT COALESCE(NULL, ?)` 绑定数值
（mysql_async 二进制参数 i64/f64/DATE）。e2e 此前靠文本绑定规避。

## 方案（兼容编码 + 结果集前预检）

- 新增 `src/sql/front/conv_bin.rs`（256 行 + 独立测试文件）：`Cell` 包装运行期
  `Value`，`to_mysql_bin` **按公告列型分发**——凡有忠实拼法的组合一律按公告
  列型编码（"兼容编码"）：
  - 字符串族列（VAR_STRING/BLOB 族/ENUM/SET/BIT/JSON/GEOMETRY/NEWDECIMAL 等，
    MySQL 二进制协议本就发 lenenc 串）：任何运行期类型发其**规范文本**（与文本
    协议逐字节一致）——数值绑进文本占位符列即出 `"42"`/`"1.5"`；
  - 整型列：Int/Bool 按列宽 LE 编码（窄列溢出响亮）；
  - DOUBLE/FLOAT 列：任意数值（Decimal 走 f64 近似，对齐引擎 Decimal→Double）；
  - DATE/DATETIME/TIMESTAMP 列：既有类型编码器 + Date↔DateTime 互转
    （对齐引擎零点/截断语义）；
  - `to_mysql_text` 逐字节委托 opensrv 原实现（含 NULL 的 0xFB 单字节）——
    文本协议路径零变化。
- **pre-flight**（`conv_bin::preflight_binary`）：行集在落线前已全量物化，逐 cell
  对公告列试编码；无忠实拼法的组合（字符串 cell 对数值列、NULL 对公告 NOT NULL
  列、行长不齐）在**结果集开始前**响亮回 1292（ER_TRUNCATED_WRONG_VALUE）
  ERR 包——连接存活，取代行中 io 错断连。`shim.rs::write_outcome` 仅
  `on_execute`（二进制路径）启用预检。
- 原先可编码的配对（i64→LONGLONG、Str→VAR_STRING、Decimal→NEWDECIMAL 文本、
  Date/DateTime→类型列）字节不变；长串的 0xfc/0xfd lenenc 形态保留。

## 测试

- 单测（`conv_bin_tests.rs`，7 例）：数值/时间 cell → 文本列 lenenc 文本、
  文本协议字节不变（含 NULL 0xFB）、原配对字节回归（i64/Bool/Decimal/Double）、
  整型列宽收窄与溢出、浮点列吃全部数值、Date↔DateTime 互转、预检三态
  （不可编码带列名 / NOT NULL / 长不齐）。
- e2e（`tests/sql_e2e.rs` prepared 节扩展）：数值/DATE 绑进 `COALESCE(NULL, ?)`
  文本占位符列返回 `"42"`/`"1.5"`/`"2024-02-29…"`；DOUBLE 定型投影吃 Int 运行
  cell（CASE 取 ELSE 分支）；**同一连接后续查询照常**（断连消除）。回归验证：
  暂存 src 改动重跑，新断言以 `connection closed` io 错失败——证明用例真实覆盖。
- 面上回归：`cargo test -p rdb --lib` 1320 全绿；全部 22 个 SQL e2e 套件
  （compat/types/decimal/funcs×4/query_semantics/upsert/txn/index/ddl_surface×2/
  insert_select/join×3/subquery/setop/oracle/composite_pk/2pc/dist_read/rename_gc/
  restart/failover/auto_increment/starrocks/columnar）+ sql_e2e 全部通过。

## 文档

- `COMPAT.sql.md` deviation ledger：M0–M5 条目内 "Known protocol limitation
  (open follow-up)" 改为 CLOSED（2026-10-08）并描述现行为；
- `plans/2026-10-06-mysql-gap/gap-matrix.md` 落地状态节：开放跟进项标记收口。

## 后续（非本批）

- mysql-hardening H1–H4（偏差 (6)–(9)：溢出 errno 通道 1690/22003、decimal SUM
  措辞、SUM(double) 饱和、整型宽度坍缩/UNSIGNED 拒绝）按计划在途；
- P2 五项（ALTER COLUMN、复合/前缀索引、WITH RECURSIVE、GROUP_CONCAT 内层
  ORDER BY、KILL）维持 deferred，按需立项。
