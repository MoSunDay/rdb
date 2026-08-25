# 时间类型 DATE / DATETIME / TIMESTAMP（MySQL 兼容 Phase 2）

## 背景
MySQL 兼容 Phase 2：此前 SQL 类型刻意收窄（`schema.rs` 注释明言 no DATE/TIME），建表
写 DATE 列直接被解析器拒绝；`NOW()` 一类时钟函数也不存在。本次以"纯整数域"模型补齐
时间类型：DATE = 距 1970-01-01 的天数（i64），DATETIME = 距纪元的微秒数（i64），
TIMESTAMP 解析为 DATETIME 别名（无时区语义）。

## 修复
- **儒略日 civil 数学**（新 `src/sql/temporal.rs` + `temporal_tests.rs`，纯函数）：
  `days_from_civil` / `civil_from_days`（Howard Hinnant 算法，`checked_add` 防
  i64::MIN 溢出 panic）、canonical/紧凑字面量双向解析与格式化（`2024-02-29` 与
  `20240229`、微秒 6 位仅在非零时渲染）、`compact_*` Int 域互转、`now_micros` /
  `today_days`（UTC 墙钟，无会话时区）。
- **类型域与编解码**（`storage/schema.rs` / `storage/codec.rs` / `index/keys.rs` /
  `columnar/{encode,decode}.rs`）：`SqlType::{Date,DateTime}` + `Value::{Date,DateTime}`
  进入 catalog serde、行 payload（BE i64）与索引 key（codec 标签 `0x06`/`0x07`）。
- **解析与 DDL**（`parse/expr.rs` / `parse/translate.rs`）：DATE/DATETIME/TIMESTAMP
  （`(fsp)` 解析后忽略，恒为微秒精度）建表合法；TIME/DECIMAL 仍 1235 拒绝。
- **求值与比较**（`exec/expr.rs`）：Str↔temporal 双向胁迫（乱值报
  `Incorrect DATE value: '...'`）、Int 按紧凑数值跨域比较、Date↔DateTime 午夜换算、
  时钟函数 `NOW()/CURRENT_TIMESTAMP()/SYSDATE()/LOCALTIME()/LOCALTIMESTAMP()`、
  `CURDATE()/CURRENT_DATE()`；时间算术（DATE + n 等）响亮报错。
- **结果类型元数据**（`exec/select.rs`）：`LAST_INSERT_ID()/LENGTH()/CHAR_LENGTH()`
  → LONGLONG（修正 Phase 1 的 VARCHAR 偏差，COMPAT.md 同步删除）、时钟函数 →
  DATETIME/DATE、表列镜像 schema（SHOW COLUMNS 报 `date`/`datetime`）。
- **集合与渲染**（`exec/set_ops.rs` / `exec/render.rs` / `exec/show.rs`）：
  UNION 中 Date 列与 DateTime 列并集时列宽化为 DATETIME（Date 单元格抬升为当日
  午夜微秒，去重按全精度）；EXPLAIN 字面量显示 canonical 加引号拼写；SHOW
  COLUMNS 报 `date`/`datetime`。
- **聚合**（`exec/agg.rs`）：`sum_values` 对非数值输入改为无数值成员时归 NULL——
  修复 Rust 空 f64 迭代 `.sum()` 得 `-0.0` 导致时间/字符串列 SUM 渲染 `"-0"` 的
  bug（使"SUM/AVG of temporal → NULL"成立）。
- **MySQL 线协议**（`front/conv.rs`）：文本结果发 canonical 字符串；二进制协议
  发 MYSQL_TYPE_DATE 4 字节 / MYSQL_TYPE_DATETIME 7 或 11 字节 cell（µs 非零时
  追加 u32 LE）；预编译语句参数解码 string 与二进制 date/datetime（TIME 仍拒绝），
  全零日期参数显式拒绝。
- **已知偏差**（COMPAT.md 已记录）：零日期
  `'0000-00-00'` 不可表示、按乱值拒绝；TIMESTAMP 无时区；SUM/AVG 无时间语义归
  NULL；混版本集群无法解码新 catalog/段文件/ColumnarRows RPC（需整队升级）。

## 验证
- 新增 `tests/sql_types_e2e.rs`（3 用例，真实 rdb 进程）：
  - `temporal_ddl_text_roundtrip_and_predicates`：SHOW COLUMNS 类型串；canonical/
    小数/紧凑字面量往返（含闰日）；乱值整条 INSERT 拒绝（errno 1292 = MySQL ER_TRUNCATED_WRONG_VALUE + 文案）且未落行；
    ORDER BY / =、>=、BETWEEN / UPDATE；NOW/CURRENT_TIMESTAMP/CURDATE 冒烟；
    SUM/AVG → NULL。
  - `result_metadata_is_typed`：LAST_INSERT_ID/SELECT 1/LENGTH → LONGLONG、
    1.5 → DOUBLE、NOW → DATETIME、CURDATE → DATE、表列类型镜像 schema。
  - `prepared_binary_cells_params_unique_index_union`：二进制 cell 为
    `MVal::Date(...)`（微秒为 0 时省略）；string 与类型化 Date 参数均可比较；
    预编译 INSERT；DATE 列唯一索引 1062；UNION date→datetime 宽化（含 NULL 列）。
- 单测：`temporal_tests.rs` 12 个、`conv.rs` 9 个、`codec.rs`/`expr.rs`/`set_ops.rs`
  等新增断言，`cargo test --workspace` 全绿。
- 全量回归：fmt / clippy `-D warnings` / `cargo test --workspace` / release build。
