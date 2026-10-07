# SQL M1：表达式与函数族（exec/func 拆分 + CASE/CAST/约 50 函数 + GROUP_CONCAT）

Commit: (working-tree, 随本提交入库)

## 背景
MySQL 兼容差距计划（`plans/2026-10-06-mysql-gap/`）A 组（表达式与函数）定位出函数层的整块缺口：
标量函数仅 13 个入口挤在 `exec/expr.rs`（708 行，已逼近 800 上限），无 CASE/CAST、无字符串/
数值/日期函数族，无 `<=>`/REGEXP/位运算/XOR，聚合缺 GROUP_CONCAT。M1 的**第一动作是拆文件**
（禁止直接向 708 行的 expr.rs 追加），再按族补齐 P0/P1 函数。

## 修复内容（src/sql，待提交工作区）

### 求值层拆分：`src/sql/exec/func/`（纯 dispatch，无注册器 struct、无状态）
- `mod.rs`：`eval_func` = 家族链式 `control -> string -> numeric -> datetime`，每家族只答
  `Option<SqlResult>`（拥有名字即给出结果或大声错误）；`wrong_param_count` 统一 MySQL 1582
  措辞，与翻译期签名表共用。
- `string.rs`（299）+ `string_more.rs`（238）：CONCAT/CONCAT_WS/SUBSTRING/LEFT/RIGHT/LPAD/
  RPAD/REPEAT/LOCATE/INSTR/POSITION/REPLACE/TRIM/REVERSE/HEX/UNHEX + 迁入 UPPER/LOWER/
  LENGTH/CHAR_LENGTH；`regexp_match`（byte-wise、大小写敏感）与 `value_text`（CONCAT 强制）
  为家族导出。
- `numeric.rs`（250）+ `numeric_more.rs`（152）：ROUND（Decimal 精确 half-away、负 d 整数位
  缩放、Double 走 f64）/CEIL/CEILING/FLOOR/TRUNCATE/MOD/POW/POWER/SQRT（负数→NULL）/SIGN/
  GREATEST/LEAST（NULL 传播 + 数值域宽度提升；字符串/数值混合大声拒绝）+ 迁入 ABS；
  `eval_bitop` 实现 `& | ^ << >>` 的 **u64 语义**（负数按补码字移位，shift ≥64 归零，不掩码）。
- `datetime.rs`（141）+ `datetime_more.rs`（275）：DATE/YEAR/MONTH/DAY/DAYOFMONTH/HOUR/
  MINUTE/SECOND（非法拼写→NULL）/DATE_ADD/DATE_SUB/ADDDATE/SUBDATE（INTERVAL 单位集
  YEAR..MICROSECOND，月份/闰日钳制，时间单位把 DATE 升格为 DATETIME）/DATEDIFF（只比日期
  部分）/DATE_FORMAT（%Y %y %m %d %H %i %s %T %p %W %M %j %%）/UNIX_TIMESTAMP/FROM_UNIXTIME
  + 迁入 NOW/SYSDATE/CURDATE/CURTIME 族。
- `control.rs`（290）：VERSION/LAST_INSERT_ID + 三个**结构化**入口（不走值级 dispatch）：
  `eval_case`（两形态 CASE，按序短路，NULL 永不匹配）、`eval_cast`（复用写路径
  fit_column/rescale_decimal）、`eval_lazy`（IF/IFNULL/NULLIF/COALESCE 在实参求值**之前**拦截，
  未取分支永不求值）。
- `meta.rs`（146）：纯 `name -> SqlType` 结果类型表（输入镜像条目带实参类型），投影元数据
  据此给列：CHAR_LENGTH→LONGLONG、CONCAT→VAR_STRING、ROUND(DECIMAL)→NEWDECIMAL(d)、
  NOW→DATETIME、CURDATE→DATE、UNHEX→BLOB 等，`select.rs` 的投影 typer 查表。
- `expr.rs` 收缩为通用算子求值 + 对 `func::eval_func` 的转发；家族单测 `*_tests.rs` 逐文件
  ≤400 行。

### 翻译层：AST 扩展 + 特殊形态 + 签名表
- `parse/ast.rs`：`Expr::Case`（operand=None 即搜索式）、`Expr::Cast`、`Expr::Regexp`、
  `Expr::Agg.sep`；`BinOp` 增 NullSafeEq/LogicalXor/BitAnd/BitOr/BitXor/Shl/Shr；
  `AggFunc::GroupConcat`。
- `parse/expr.rs`：CASE/CAST(CastKind::Cast)/CONVERT(两形态，USING 仅 utf8/utf8mb4)、RLIKE/
  NOT RLIKE、SUBSTRING(FROM/FOR)、TRIM(BOTH/LEADING/TRAILING remstr)、POSITION(.. IN ..)、
  `d ± INTERVAL n unit`（算子形态两侧互换）均直翻或去糖为普通调用；超出 MySQL 子集的形态
  （TRY_CAST、CONVERT(x, type, style)、BINARY 目标等）大声 unsupported。
- `parse/func_forms.rs`（新增）：上述特殊形态的去糖 + INTERVAL 单位白名单
  （YEAR/MONTH/DAY/HOUR/MINUTE/SECOND/MICROSECOND，其余单位+range/precision 形式拒绝）+
  GROUP_CONCAT（DISTINCT 复用、SEPARATOR→节点字段、**内层 ORDER BY 翻译期 1235 拒绝**、
  多实参包 CONCAT）。
- `parse/func_sig.rs`（新增）：纯 name→(min,max) arity 表，prepare 期即报 1582；与家族求值器、
  `func::meta` 三方同步约定写在头注。
- `exec/agg.rs`：GROUP_CONCAT 聚合（跳过 NULL，缺省 `,`，按分组首次出现顺序拼接，全 NULL 组
  为空串）；`exec/render.rs`：EXPLAIN/列元数据对新节点出可读形态并查 meta 表。

### 新依赖
- `regex = "1.13"`（optional，`full` 门控）：REGEXP/RLIKE。已在依赖树中（jieba-rs、
  opensrv-mysql 均引入），lockfile 无新 crate，默认构建不变。

合计：**51 个标量函数入口（含别名）+ CASE/CAST 两结构 + 9 种新算子**（`<=>`、REGEXP/RLIKE、
`& | ^ << >>`、XOR）+ GROUP_CONCAT 聚合。既有 13 个函数行为不变，但获得精确结果列元数据。

## 有意偏离（记录于 COMPAT.md）
- REGEXP/LIKE 均为 **byte-wise、大小写敏感、无 collation**（MySQL 默认 utf8mb4 不区分大小写）；
  正则方言为 Rust `regex` crate（无反向引用/回溯），不支持 MySQL ICU 方言扩展；
- 时钟族（NOW/SYSDATE/CURDATE/UNIX_TIMESTAMP）走 **UTC**，无时区/session tz；
- GREATEST/LEAST 拒绝字符串/数值混用（无跨域隐式强转）；ROUND/SIGN 对字符串实参大声
  NotSupported（MySQL 会尝试隐式转换）。

## deferred（gap-matrix 保持 P2 + 理由）
- GROUP_CONCAT 内层 ORDER BY：需分组内排序基建，单独立项（e2e 钉死 1235 拒绝）；
- GREATEST/LEAST 混合类型：无跨域比较语义，本期大声拒绝；
- INTERVAL 单位超集（WEEK/QUARTER/复合单位/range 形式）：白名单外 1235；
- ROUND/SIGN 的字符串隐式转换（MySQL 宽容路径）不做。

## e2e（tests/，本工作区新增）
- `tests/sql_funcs_common/mod.rs`（137 行）：四套件共享的 MySQL e2e 脚手架（leader-retry DDL、
  文本协议 cell/row/col 读取、列元数据、错误探针、`SELECT <expr>` 表驱动断言、跨类型系统
  fixture 表 BIGINT/VARCHAR/DOUBLE/DECIMAL(10,3)/DATE/DATETIME + NULL 行）——沿用
  `backup_surface_common` 的挂载模式。
- `tests/sql_funcs_string_e2e.rs`（7 用例）：36 条表达式精确文本（覆盖全部字符串函数 + FROM/FOR、
  TRIM remstr、POSITION IN 形态）、NULL 传播（13 条 + 列路径 + REGEXP 的 NULL 列）、REGEXP
  大小写敏感 + 坏模式 1292、函数进 WHERE/GROUP BY/JOIN ON、prepared 参数作函数实参
  （UPPER(=)、LOCATE(?)、CONCAT 投影、LIMIT ?）、GROUP_CONCAT（DISTINCT/SEPARATOR/跳 NULL/
  分组内/全 NULL 组空串/内层 ORDER BY 1235）、负矩阵（UPPER/REPLACE/ROUND/LOCATE/LPAD arity
  1582@prepare、GROUP_CONCAT() 1064、未知函数 1235）。
- `tests/sql_funcs_numeric_e2e.rs`（8 用例）：ROUND(2.005,2)=2.01、ROUND(-2.5)=-3、负 d 缩放
  （ROUND(15.00,-1)=20 / ROUND(125,-1)=130）、TRUNCATE(±1.999,1) 向零、DECIMAL(10,3) 列上
  ROUND/TRUNCATE/CEILING 的精确文本与 NEWDECIMAL 元数据（DOUBLE 列走 f64 舍入）、GREATEST/
  LEAST NULL 规则 + Int 赢家提升到最粗 decimal scale（GREATEST(5,2.5)=5.0）、位运算 u64 语义
  （1<<63=i64::MIN、-1>>1、-1&255、shift≥64→0）、函数进 WHERE/GROUP BY、prepared 参数
  （MOD/SIGN/价格谓词）、负矩阵（ROUND('abc')/SIGN('x')/GREATEST('a',1) 1235 + arity 1582）、
  CEIL/FLOOR 关键字形态（已随后修复为可用，见下）。
- `tests/sql_funcs_datetime_e2e.rs`（7 用例）：时钟族形状 + DATETIME/DATE/VAR_STRING 元数据、
  抽取族（含非法→NULL）、DATE_ADD/SUB 全单位（月/闰钳制、时间单位升格）+ `d ± INTERVAL` 算子
  形态 + ADDDATE/SUBDATE 天数形态、DATEDIFF 符号与纯日期语义、UNIX_TIMESTAMP/FROM_UNIXTIME
  精确值 + 无参时钟、DATE_FORMAT 全 specifier、函数进 WHERE/GROUP BY DATE(at)、负矩阵
  （WEEK/QUARTER 单位 1235、DATE_ADD 无 INTERVAL 1582）。
- `tests/sql_funcs_control_e2e.rs`（7 用例）：CASE 两形态 + WHEN NULL 三值 + 惰性 THEN（错误
  分支不触发）、IF/IFNULL/NULLIF/COALESCE 惰性 + VERSION、CAST/CONVERT 全目标 + 1292 拒绝 +
  元数据、`<=>` vs `=` 真值表 + XOR 真值表（TINY 列）+ `<=>` 进 WHERE、跨族元数据
  （ROUND/CONCAT/YEAR/CAST/CHAR_LENGTH/IFNULL/DATE_FORMAT 一行断言）、prepared
  （WHERE id=?、IFNULL(name,?)、CASE 参数 + LIMIT ?）、负矩阵（arity 1582、BINARY/latin1 1235）。

## e2e 发现的实现侧缺口（本工作区不改 src，钉死现状 + 待跟进）
- ~~`CEIL(x)` / `FLOOR(x)` 关键字形态 1235 拒绝~~ **已修复**：`parse/func_forms.rs` 增补
  `translate_ceil_floor`（sqlparser 的 `Expr::Ceil`/`Expr::Floor` 专用节点 → `ceiling`/`floor`
  Func 调用；`TO <unit>` 间隔缩放形态仍响亮拒绝），e2e 已翻转为值断言。
- prepared 二进制协议下，实参含 `?` 的文本型函数列（如 `COALESCE(NULL, ?)`，占位符静态类型
  VarChar）绑定**数值**时服务端编码器报 io 错并断连（Int 值写 VAR_STRING 列）；e2e 以文本
  绑定规避并注释。跟进项：绑定值与静态列类型不匹配时的兼容编码或响亮错误。

## 验证
- `cargo test --test sql_funcs_{string,numeric,datetime,control}_e2e`：29/29 绿
  （string 7 / numeric 8 / datetime 7 / control 7，两轮复跑）；`cargo fmt --check` 干净。
- 单测（`src/sql/exec/func/*_tests.rs`、`parse/func_sig_tests.rs` 等）为落地工作区自带；
  全量 `cargo test --workspace` 以落地提交的 CI 运行为准。

## 关联与残留
- 计划：`plans/2026-10-06-mysql-gap/m1-expression-functions.md`（A 组 P0/P1 销账；后续
  M2 DML 冲突 → M3 子查询/集合 → M4 DDL/session → M5 e2e 盲区）。
- 文档同步（`COMPAT.md` 函数清单与偏离、`agents/rust/sql.md` 模块地图、
  `features/sql-dataplane.md`、`features/e2e-coverage.md`）随 M1 收尾提交一并补齐。
