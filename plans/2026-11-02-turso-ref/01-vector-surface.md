# 01 · libsql 向量 SQL 面 ↔ MySQL 9.0 VECTOR 面 ↔ rdb 落地形态

- 参考：`link_repos/turso/`（pin `d8bc862d`，只读）。libsql 侧行为**全部以代码与自带测试为准**
  （`libsql-sqlite3/src/vector*.c`、`test/libsql_vector.test`、`test/libsql_vector_index.test`；
  仓库 `docs/` 只有协议 spec，无向量文档）。
- MySQL 9.0 侧凭既有知识撰写，凡未经实测的一律标注【待实测确认】。
- 本文只记录 SQL 面契约与决策，不改代码；ANN 内部结构对照归 02 号文档。

## 0. 先决更正（覆盖任务假设的三处偏差）

1. **`MATCH` 语法在该 pin 已不存在**。`vectorvtab.c` 注册的 module 名是 `vector_top_k`
   （`vectorIndexInt.h:239`），形态是 eponymous 表值函数（隐藏列 `idx, vector, k`，
   `vectorvtabConnect` 声明 `CREATE TABLE x(idx hidden, vector hidden, k hidden, id)`）。
   旧版 `WHERE embedding MATCH '[..]' AND k = ?` 形态已被整体移除，src/ 内无任何 MATCH 钩子。
2. **`CREATE VECTOR INDEX ... USING libsql_vector(...)` 已废弃**：`vectorIndex.c:842` 对新建索引
   直接报错（见 §1.6 原文），现行 DDL 是普通 `CREATE INDEX ... ON t(libsql_vector_idx(col, 'k=v'))`。
3. **该 pin 没有 `vector_distance_dot` / `vector_debug`**（全 src/ grep 无命中）。dot 距离只在
   DiskANN 内部使用，未暴露为 SQL 函数。另：所谓 int8 变体实为 `FLOAT8`（逐维 u8 量化 +
   alpha/shift 仿射），另有 `FLOAT1BIT`（符号位量化，bit=1→+1 / bit=0→-1，含 0）。

## 1. libsql 面（pin d8bc862d 实测）

### 1.1 类型系统

| 类型 | 列类型名（`vectorIndex.c:380` VECTOR_COLUMN_TYPES） | 存储形态（`vectorInt.h` on-disk 注释） |
|---|---|---|
| FLOAT32 | `FLOAT32(N)` / `F32_BLOB` | dims×f32 LE 小端；**尾类型字节可省**（偶长度 blob 默认按 f32 解析） |
| FLOAT64 | `FLOAT64(N)` / `F64_BLOB` | dims×f64 LE + 尾字节 `0x02`（必填） |
| FLOAT1BIT | `FLOAT1BIT(N)` / `F1BIT_BLOB` | 位图 + padding + trailing_bits + 尾字节 `0x03` |
| FLOAT8 | `FLOAT8(N)` / `F8_BLOB` | 逐维 u8 + 对齐 padding + alpha(f32) + shift(f32) + 尾字节 `0x04` |
| FLOAT16 | `FLOAT16(N)` / `F16_BLOB` | IEEE half + 尾字节 `0x05` |
| FLOATB16 | `FLOATB16(N)` / `FB16_BLOB` | bfloat16 + 尾字节 `0x06` |

- 上限：`MAX_VECTOR_SZ 65536`（`vectorInt.h`，作用于运行时向量，不是列声明上限）。
- 列声明 `FLOAT32(3)`：维度写在类型名括号里，`vectorIdxParseColumnType`（`vectorIndex.c:534`）
  解析；代码注释明确 `FNN_BLOB` 才与 SQLite 亲和性规则对齐（`FLOAT32` 含 "FLOA" 走 REAL 亲和）。
- blob 字面量（`x'...'`）经 `vectorParseMeta` 按长度奇偶/尾字节判定类型并校验整除性。

### 1.2 函数注册表全清单（`vector.c:1257 sqlite3RegisterVectorFunctions`）

| 函数 | 元数 | 语义 |
|---|---|---|
| `vector(X)` | 1 | = `vector32`（默认 f32），TEXT/BLOB → 序列化 vector blob |
| `vector32(X)` / `vector64(X)` | 1 | 文本/低位类型输入 → f32/f64 vector blob（内部会做类型转换） |
| `vector1bit/8/16/b16(X)` | 1 | 同上，目标为对应量化类型 |
| `vector_extract(X)` | 1 | 任意 vector blob → TEXT `'[a,b,...]'`（低位类型先转 f32 再输出） |
| `vector_distance_cos(X,Y)` | 2 | 余弦距离 `1 - dot/(|a||b|)`；FLOAT1BIT 时退化为 Hamming |
| `vector_distance_l2(X,Y)` | 2 | 欧氏距离 `sqrt(Σ(a-b)²)`；**float1bit 明确不支持**（报错） |
| `libsql_vector_idx(...)` | -1 | 索引标记函数，no-op（原样返回第一参），专用于 CREATE INDEX 语法 |
| `vector_top_k(idx, vector, k)` | — | 不是函数：eponymous vtab（§1.4），可见列只有 `id` |

函数名大小写不敏感（SQLite 惯例，`sqlite3StrICmp` 匹配）。距离函数返回 DOUBLE；
两个入参要求同类型且同维度，否则报错（§1.6）。

### 1.3 文本解析器逐条行为（`vectorParseSqliteText`，`vector.c:174`）

| 输入 | 行为 |
|---|---|
| 前导/尾随空白 | 跳过（isspace）；`[ ]` 前后空白均可 |
| 首/尾字符 | 必须 `[` 开头、`]` 结尾，否则报错 |
| 元素内空白 | **直接剔除后拼接**：`'[1 2]'` 解析为单元素 `[12]`（不是错误，也非分隔） |
| 分隔符 | `,`；元素以 `,` 或 `]` 结束 |
| 空向量 `'[]'` | 合法，dims=0（`hex(vector32('[]'))` = 空） |
| 数值文法 | `sqlite3AtoF`：十进制 + 科学计数法 + 前导符号；**不接受** inf/nan 字面量、**不接受十六进制**（`util.c:773` 注释明示） |
| 溢出 | 接受：f32 round-trip 后显示 `Inf`/`-Inf`（测试 vector-1-conversion 断言） |
| 单元素长度 | ≤ `MAX_FLOAT_CHAR_SZ`（1024）字符，超出报错 |
| 维度上限 | 元素计数 ≤ 65536，超出报错 |
| 非法浮点 | `'[1, 2, 1.1.1, 4]'` → invalid float at position 2 |
| `']'` 后内容 | 仅允许空白，其余报错（`'[1]]'` 即此错） |

维度预估（`detectTextVectorParameters`）只数逗号+数字做粗检，真实维度由 parse 决定；
因此 `'[1,2]'` 这类 TEXT 可直接喂给 `vector_distance_*`/`vector_top_k`（无需先 `vector()`）。

### 1.4 查询与 DDL 语法（现行形态）

```sql
-- 建表 + 建向量索引（DiskANN）：
CREATE TABLE t(id INTEGER PRIMARY KEY, v FLOAT32(3));
CREATE INDEX t_idx ON t( libsql_vector_idx(v) );                        -- 最简形态
CREATE INDEX t_idx ON t( libsql_vector_idx(v, 'type=diskann',
                      'metric=cosine', 'compress_neighbors=float8',    -- 可选参数
                      'max_neighbors=50') );
-- KNN 查询（TVF；输出列 id = 命中行的 pk/rowid，无 distance 列）：
SELECT rowid FROM vector_top_k('t_idx', vector('[3,4,5]'), 4);
SELECT *    FROM vector_top_k('t_idx', '[1,2,3]', 1);   -- 裸 TEXT 亦可
```

- 参数表（`vectorIndex.c:411` VECTOR_PARAM_NAMES）：`type=diskann`（唯一值）；
  `metric=cosine|l2`；`compress_neighbors=float1bit|float8|float16|floatb16|float32`；
  `alpha`(float)、`search_l` / `insert_l` / `max_neighbors`(正整数)。
- 影子表：全局 `libsql_vector_meta_shadow` 存参数；每索引 `<idx>_shadow(index_key, data)`
  存 posting；旧版 pekka 元数据表 `libsql_vector_index` 仅向后兼容读取（`vectorIndex.c:766`）。
- `vector_top_k` 约束（`vectorIndexSearch`，`vectorIndex.c:975`）：恰好 3 参；k 必须
  非负整数（浮点整值可，负数/非数值报错）；k=0 合法返回空；结果按距离升序、等距按 rowid
  升序打破平局（`vectordiskann.c:934` 注释）；查询向量维度 ≠ 索引维度报错（§1.6）。
- 注意 **ANN 是近似的**：等距序确定性有保证，但召回率依赖建图参数（02 展开）。

### 1.5 错误消息原文（风格：`模块前缀: 小写句子`，含具体值）

| 场景 | 原文 |
|---|---|
| 非 TEXT/BLOB 输入 | `vector: unexpected value type: got FLOAT, expected TEXT or BLOB`（INTEGER/NULL 同款） |
| 缺 `[` | `vector: must start with '['` |
| 缺 `]` | `vector: must end with ']'` |
| 尾随杂字符 | `vector: non-space symbols after closing ']' are forbidden` |
| 非法浮点 | `vector: invalid float at position %d: '%s'` |
| 元素过长 | `vector: float string length exceeded 1024 characters: '%s'` |
| 维度超限 | `vector: max size exceeded 65536` |
| blob 长度非法 | `vector: float32 vector blob length must be divisible by 4 (excluding optional 'type'-byte): length=%d`（f64/8、1bit、f8、f16、fb16 各有对应变体） |
| 未知 blob 类型 | `vector: unexpected binary type: %d` |
| 距离维度不匹配 | `vector_distance: vectors must have the same length: 3 != 2` |
| 距离类型不匹配 | `vector_distance: vectors must have the same type: 1 != 2` |
| 1bit L2 | `vector_distance: l2 distance is not supported for float1bit vectors` |
| USING 已废弃 | `vector index: USING syntax is deprecated, please use plain CREATE INDEX: CREATE INDEX xxx ON yyy ( libsql_vector_idx(zzz) )` |
| 索引表达式约束 | `vector index: must contain exactly one column wrapped into the libsql_vector_idx function` / `collation in expression is forbidden` / `first argument must be a column token` |
| KNN 参数 | `vector index(search): third parameter (k) must be a non-negative integer, but negative value were provided` 等 |
| KNN 维度不匹配 | `vector index(search): dimensions are different: %d != %d` |

## 2. MySQL 9.0 VECTOR 面（凭知识；【待实测确认】= 未在真机上验证）

### 2.1 类型与字面量

- 列类型 `VECTOR(N)`：N ∈ 1..16383【待实测确认 16383】；N 必填（无 bare VECTOR）【待实测确认】。
  内部存储为 N×4 字节小端 f32 二进制串。
- 字面量 `'[1,2,3]'` **本质是字符串**；写入 VECTOR 列或在期望 vector 的位置发生隐式转换
  （与 `STRING_TO_VECTOR` 同解析器）。元素间空白容忍度、科学计数法/inf/nan 是否接受、
  空向量是否拒绝【全部待实测确认】。
- 列限制：不可作 PRIMARY KEY / 任何索引键；无 DEFAULT（NULL 除外）；无 COLLATION；
  比较仅支持 `=` / `<=>`（无大小比较）【待实测确认】。

### 2.2 函数（签名以 MySQL 手册为准）

| 函数 | 语义 |
|---|---|
| `STRING_TO_VECTOR(str)`（别名 `TO_VECTOR`）【别名待实测确认】 | `'[..]'` 文本 → VECTOR |
| `VECTOR_TO_STRING(v)`（别名 `VEC_TO_TEXT`）【待实测确认】 | VECTOR → `'[a,b,..]'` 文本 |
| `VECTOR_DIM(v)` | 维度数，返回 INT |
| `DISTANCE(v1, v2, 'cosine'\|'euclidean'\|'dot')` | 返回 DOUBLE；第 3 参必须是字符串常量，不可为列/绑定参数【待实测确认】 |

- metric 语义：`cosine` = 1 − cos 相似度；`euclidean` = L2；`dot` 的方向（原内积还是
  **负内积**以便"越小越近"）【待实测确认——HeatWave 惯例倾向负内积】。
- 维度不匹配 / 非法文本的 errno 与文案【待实测确认】。

### 2.3 KNN 形态与 wire

- **社区版 MySQL 无原生 ANN 索引**：KNN 常规写法是
  `SELECT * FROM t ORDER BY DISTANCE(embedding, STRING_TO_VECTOR('[..]'), 'cosine') LIMIT k`
  全表扫描 + 排序（HeatWave 云侧加速器不在社区路径内）。这与 rdb V1 精确 KNN 一致。
- wire：`MYSQL_TYPE_VECTOR` 疑为 **242**【待实测确认】；文本协议下 VECTOR 列/表达式回传
  形态（原始 4N 字节二进制串还是 `'[..]'` 文本）【待实测确认】。

## 3. rdb 落地形态决策表

| # | 决策 | 内容与理由 | 接入点 |
|---|---|---|---|
| D1 | DDL 语法 | 采用 MySQL 形态 `VECTOR(N)`，N 必填、1..=4096。sqlparser 0.62 **无** `DataType::Vector` 变体（已核对 crate 源码），`VECTOR(N)` 会落到 `DataType::Custom`，加一条按名匹配的 arm；不引入 libsql 的 `FLOAT32(N)`/`F32_BLOB` 列名 | `src/sql/parse/translate_type.rs` |
| D2 | 维度上限 4096 | 与 `src/search/ft_cmd.rs:95`（FT.CREATE VECTOR dim 1..=4096）及 SQ8/SPANN 管线兼容；4096×4B=16KB/行，内存与 MVCC 值体积合理。相对 MySQL 16383 是收窄，进台账 | 同 D1（translate_type 校验） |
| D3 | 函数名大小写 | **沿用现有 lowercase 规范**（不是大写）：rdb 在 parse 时把函数名统一 `to_lowercase()`（`parse/expr.rs:326`），签名表/求值族/结果类型表全部以 lowercase 名注册，天然满足 MySQL"函数名不区分大小写"。无需新增规范化层 | `src/sql/parse/expr.rs`（已有机制，不动） |
| D4 | 函数注册接入点 | 三处联动（现状约定见 `func_sig.rs` 头注释）：① 元数表 `src/sql/parse/func_sig.rs`；② 求值族新增 `src/sql/exec/func/vector.rs` 并挂进 `func/mod.rs::eval_func` 固定链；③ 结果类型表 `src/sql/exec/func/meta.rs::result_type` | 见左列 |
| D5 | 函数集（V1，MySQL 面） | `STRING_TO_VECTOR`(1)、`TO_VECTOR`(1, 别名)、`VECTOR_TO_STRING`(1)、`VEC_TO_TEXT`(1, 别名)、`VECTOR_DIM`(1)、`DISTANCE`(3)。**不引入** `vector()/vector32()/vector_extract()/vector_distance_*`/`libsql_vector_idx`（libsql 专属面，进台账） | 同 D4 |
| D6 | DISTANCE metric 枚举 | `'cosine' | 'euclidean' | 'dot'`，ASCII 大小写不敏感【大小写是否敏感待实测】；第 3 参仅接受字符串字面量常量。非法值文案（MySQL 原文未取得【待实测】）暂定：`Invalid distance metric 'xxx' for function DISTANCE. Must be one of: COSINE, EUCLIDEAN, DOT`，进台账 | `src/sql/exec/func/vector.rs` |
| D7 | 距离语义 | cosine = `1 - cos`（复用 `src/search/vecmath.rs::cosine` 后取 `1-x`）；euclidean = `vecmath::l2`；dot = **负内积**（`-vecmath::dot`，与"ORDER BY 距离升序=最相似"一致；与 MySQL 是否一致待实测，进台账）。零向量：vecmath 语义 cosine 相似度=0 → 距离=1（libsql 是 0/0=NaN，rdb 不跟随，进台账） | 同上 |
| D8 | VECTOR 列约束 | 不可作 pk、不可作 UNIQUE/普通二级索引键（`src/sql/index/` 维护路径拒绝，loud error 而非静默）；允许 NULL；无 DEFAULT【DEFAULT 是否禁用待实测对齐】 | `src/sql/exec/ddl.rs` + `parse/translate_ddl.rs` |
| D9 | 隐式转换规则 | 逐条对照 libsql §1.3：① 期望 vector 处接受 TEXT，跳过前后空白；② 必须 `[..]`；③ 元素内空白剔除拼接（`'[1 2]'`→`[12]`，**照搬 libsql**，进台账）；④ 空向量 `'[]'` **拒绝**（rdb 列有声明维度 N≥1，维度不匹配即错；libsql 接受 dims=0，进台账）；⑤ 数值文法：十进制+科学计数法+前导符号；**拒绝 inf/nan 字面量**（libsql 同拒；MySQL 待实测）；溢出→±Inf 接受（同 libsql）；⑥ 单元素 ≤1024 字符；⑦ **不接受 blob→vector**（无 `x'..'` 字面量入口；MySQL 亦无，进台账）；⑧ 维度 ≠ 列 N 报错 | DDL 校验 + `vector.rs` 解析器 |
| D10 | 维度不匹配文案 | libsql 原文 `vectors must have the same length: 3 != 2` 仅作风格参考；rdb 暂定 MySQL 风格英文（errno 待实测后对齐）：`Vector dimension mismatch: expected 3, got 2`，进台账 | 同上 |
| D11 | KNN 形态（V1） | `SELECT * FROM t ORDER BY DISTANCE(v, STRING_TO_VECTOR('[..]'), 'cosine') LIMIT k`，精确全扫 + 排序（与社区 MySQL 同形态；复用现有 ORDER BY 表达式 + LIMIT 执行路径）。不做 `vector_top_k` TVF、不做 MATCH（MySQL 生态无此形态）；SPANN 加速接入归 02 | `src/sql/exec/select.rs`（现有排序路径） |
| D12 | wire 决策 | **文本协议先行**：VECTOR 列/函数结果以 `'[a,b,..]'` 文本返回，列声明 `MYSQL_TYPE_VAR_STRING`；二进制协议（prepared）同以字符串编码；**不启用 242**（`front/conv.rs` 类型映射不动，无 Vector wire 分支），进台账 | `src/sql/front/conv.rs` / `conv_bin.rs` |
| D13 | 存储形态 | 新增 `SqlType::Vector { dim }`（`storage/schema.rs`）；运行时值复用 `Value::Bytes`（f32 LE `dim*4`，即 MySQL 二进制形态）——不动 `Value` 枚举与行 codec tag，改动面最小；投影/回传时渲染 `'[..]'` 文本 | `src/sql/storage/schema.rs`、`row.rs` |

## 4. 兼容性偏差清单（预填 COMPAT.sql.md 素材，编号延续现有台账）

- (a) 维度上限 4096（MySQL 疑为 16383【待实测确认】）——与 search 侧 SQ8/SPANN 管线对齐的主动收窄。
- (b) wire 不启用 `MYSQL_TYPE_VECTOR`(242)：VECTOR 列以 `MYSQL_TYPE_VAR_STRING` 文本
  `'[..]'` 回传，文本/二进制协议同策略——客户端拿不到二进制 4N 形态。
- (c) VECTOR 列不可 pk/二级索引（MySQL 同限制；rdb 报错文案/errno 可能不同【待实测对齐】）。
- (d) 空向量 `'[]'` 拒绝（libsql 接受 dims=0；MySQL 疑似拒绝【待实测确认】）。
- (e) inf/nan 字面量拒绝、溢出→±Inf 接受（对照 libsql §1.3；MySQL 行为待实测确认）。
- (f) 元素内空白剔除拼接：`'[1 2]'` = `[12]`（照搬 libsql；MySQL 解析器行为待实测确认）。
- (g) metric `'dot'` = 负内积（HeatWave 惯例方向【待实测确认】；若实测为原内积则修正并回写本条）。
- (h) 零向量 cosine 距离 = 1（rdb `vecmath` 语义）；libsql 为 NaN，MySQL 待实测。
- (i) 不提供 libsql 函数面（`vector/vector32/vector64/vector_extract/vector_distance_cos/l2/
  libsql_vector_idx`）、不提供 `FLOAT32(N)`/`F*_BLOB` 列类型名与 blob 字面量→vector 入口——
  rdb SQL 面只暴露 MySQL 形态；`vector_top_k` TVF 不做（KNN 走 ORDER BY+LIMIT，见 D11）。
- (j) ANN DDL（libsql `CREATE INDEX .. (libsql_vector_idx(..))` / 已废弃的 `USING libsql_vector`）
  V1 不提供；是否以 MySQL 生态外语法引入 SPANN 索引由 02 决定。

## 5. 开放问题（待实测清单，建议真机 MySQL 9.x 跑一轮）

1. `VECTOR(N)` N 上限与是否必填；bare `VECTOR` 是否合法。
2. 文本协议下 VECTOR 列回传形态；`MYSQL_TYPE_VECTOR` 常量值（疑 242）。
3. `STRING_TO_VECTOR`/`VECTOR_TO_STRING` 别名拼法（`TO_VECTOR` / `VEC_TO_TEXT`）。
4. `DISTANCE` 第 3 参常量性约束与大小写敏感性；`'dot'` 数值方向。
5. 隐式转换边界：空白、科学计数法、inf/nan、溢出、`'[ ]'` 空向量、维度不匹配的 errno/文案
   （含 VECTOR 列 INSERT 时维度校验报错原文）。
6. VECTOR 列 DEFAULT、比较运算符集合（`=`/`<=>` 之外）与 CAST 交互。
