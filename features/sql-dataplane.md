Commit: 98e17a5
# SQL 数据面（MySQL 协议 + 分布式事务）

## 能力
- 任意 rdb 节点开启 `mysql_bind` 后即是一个 MySQL 服务端：native-password 登录
  （`mysql_user`/`mysql_password` 配置），支持 CREATE/DROP TABLE、CREATE/DROP INDEX、
  INSERT/UPDATE/DELETE、SELECT（WHERE/ORDER BY/LIMIT/DISTINCT/JOIN/GROUP BY/
  HAVING/聚合）、SHOW TABLES/COLUMNS/INDEX、EXPLAIN、`?` 预编译语句；列类型
  BOOL/BIGINT/DOUBLE/VARCHAR/BLOB/DATE/DATETIME/TIMESTAMP/DECIMAL(p,s)（TIMESTAMP
  为 DATETIME 别名，`NOW()`/`CURDATE()` 等时钟函数可用；TIME 仍不支持）。
- 主键：单列或复合（`PRIMARY KEY(a,b)`，列类型限制见下节）；AUTO_INCREMENT 仍限
  "单一整数列且该列即整个主键"。
- 表目录经 raft 复制：集群内任一节点建表，全集群可见；DDL 仅 leader 生效
  （follower 收到会得到 "not leader" 类错误，客户端重试即可）。
- 事务：`BEGIN`/`COMMIT`/`ROLLBACK` 快照隔离——事务内重复读稳定（repeatable read）、
  自写可见、断连自动回滚；两个并发事务改同一主键，后提交者得到 1213 冲突错误。
- StarRocks 模型头：`PRIMARY KEY(...)`（行存 upsert）与 `DUPLICATE KEY(...)`
  （列存追加）可直接写在建表语句里；未识别子句与 `AGGREGATE KEY` 以 1235 大声拒绝。
- 索引：单列二级索引与唯一索引（唯一冲突报 1062）；带索引的等值/IN/BETWEEN 查询
  走索引点查（EXPLAIN 可见 IndexScan）；索引值域含 DECIMAL（保序定宽 key 编码）。
- 列存表：`CREATE TABLE ... ENGINE=columnar`——追加式（仅 INSERT，UPDATE/DELETE/
  索引不支持），每次提交每表生成一个不可变列式段文件；读为全段扫描
  （WHERE/聚合/JOIN 照常生效）。详见 `COMPAT.md` "Columnar table engine" 节。

## DECIMAL(p,s)（精确十进制）
- 声明 `DECIMAL(p,s)`/`NUMERIC(p,s)`：p 取 1..=38，s ≤ p（裸 `DECIMAL` = (10,0)）；
  内部为 i128 定点尾数
  （有效位实际受 i128 而非 p 约束）。字面量与预编译参数按精确十进制解析，
  绝不经由 double。
- 算术精确：`+`/`-` 对齐到较粗 scale，`*` 尾数相乘、scale 相加，`%` 对齐较粗
  scale，均走 checked i128（溢出大声报错）；`/` 为 MySQL 式长除——商带被除数
  scale+4（`div_precision_increment=4`）、余数 half-away-from-zero。任一 DOUBLE
  操作数则整条表达式回落 double。
- 比较精确：Decimal/Decimal 跨 scale 与 Decimal/Int 全程 i128 精确；与 DOUBLE/
  字符串比较经 f64/解析（显式降级）。`abs()` 可用。
- 聚合定型：SUM(decimal) = 同 scale 的 DECIMAL；AVG 走 scale+4 精确除法——int 列
  AVG 定型 DECIMAL(38,4)，decimal 列 AVG 定型 DECIMAL(38, scale+4)；SUM/AVG 参数
  含 DOUBLE 则回落 DOUBLE。
- 写入裁剪：值写入 DECIMAL(p,s) 列先按列 scale 舍入（half away from zero），
  整数位超出 p-s 位报 MySQL 1292（整条语句拒绝）。
- 线协议：结果列 MYSQL_TYPE_NEWDECIMAL，文本 cell 为定点规范串（小数位按 scale
  零填充）；SUM/AVG/decimal 算术的结果列元数据同样定型 NEWDECIMAL（曾误标 DOUBLE/
  INT 致 SDK 把 "0.60" 读成 0.6——已修复）。
- 存储：typed tag 0x08（payload 与 key 同标签）；key 用定宽保序编码（字节序 =
  值序，含负数），故 DECIMAL 可进二级/唯一索引与 ORDER BY。
- 限制（大声拒绝，1235）：DECIMAL 列不可作主键（单列 pk 同样拒绝）；DECIMAL 列
  不可进 columnar 表（段页无 decimal 编码）。

## 复合主键
- `PRIMARY KEY(a,b,...)`：多列按声明顺序保序拼接为 pk key（变长成分 NUL 转义
  0x00→0x00 0xFF + 0x00 终止符，拼接无歧义且字节序 = 元组序）；二级/唯一索引项、
  2PC 写集、锁读 latch、行探测（write probe）全部携带完整 pk 元组。
- 列类型限制：仅 TINYINT/SMALLINT/INT/BIGINT（引擎内均为 Int）/VARCHAR/DATE/
  DATETIME；BOOL/DOUBLE/BLOB/DECIMAL 一律 1235（不可无歧义拼接的成分类型）。
- AUTO_INCREMENT 列必须"就是"单列主键；复合 pk 含自增列拒绝 1075。
- 目录兼容：`TableSchema.pk` 由 String 改为 Vec<String>（旧 catalog JSON 的
  `"pk":"id"` 字符串反序列化为单元素向量）；catalog 形状变更 → 跨版本必须
  同批共升（见 `COMPAT.md`）。

## 时间戳下限持久化（单机可见性）
- 每个携带 MVCC 时间戳的持久化批都原子捎带保留键 `\x00sql_ts_floor`（记该批
  最大 ts，随批 fsync，零额外写）；boot 以 `advance_to` 恢复（normal 与 backup
  两类监听路径均覆盖），保证 kill -9 重启后 oracle 时钟不回退（时钟回退会让
  旧版本行遮蔽自己的重写、已提交数据"消失"）。
- 旧版二进制原地升级兜底：floor 键缺失时，boot 做一次性全键空间扫描（行版本键
  带 slot 交叉校验 + columnar 段 commit_ts）取最大 ts 并立即落键；此后 boot 永不
  再扫。
- `\x00sql_ts_floor` 无 `"N/"` slot 前缀，按 Foreign 归类：FLUSHDB 不清、
  DBSIZE/INFO 不计。
- 升级演练：`scrtips/e2e_scenarios/upgrade_rehearsal.sh`（c22ff37→HEAD 同批共升，
  含旧数据快照比对、一次性 floor 扫描、kill -9 复启不二扫），证据见同目录
  `RESULTS.md`。

## 集群行为（3 节点及以上）
- 时间戳全局化：`CLUSTER INIT` 后所有事务时间戳由 raft leader 块授权
  （HTTP `/sql/ts`），跨节点提交顺序一致。
- 分布式写：任一节点可写任意主键——数据按 slot 归属自动路由到各节点，
  跨节点原子性由 2PC 保证（prepare→决议持久→commit 两阶段）；参与节点宕机时
  写入整体失败，不留半行数据；重启后自动恢复在疑事务。
- 分布式读：单表 SELECT 从各节点按 slot band 并发拉取后合并过滤；任一数据节点
  不可达则查询报错（不返回部分结果）。
- 限制（v1，维持现状、非配置项）：集群模式下索引点查不可用——`gatherable()`
  结构性分流使其退化为全表 gather（EXPLAIN 显示 Gather banner）；JOIN 两侧均按
  gather 物化后在协调者做嵌套循环（`Gather(join)`）；SQL 读路径不参与
  RESP 侧 HA 故障切换。

## 配置
- `mysql_bind` / `mysql_user` / `mysql_password`：MySQL 接入。
- `sql_rpc_bind`：节点间 SQL RPC（scatter-gather/2PC），空=关闭（单机语义）。
- `columnar_flush_rows` / `columnar_flush_bytes`：列存单语句/事务-表追加限额
  （默认 65536 行 / 64 MiB，0=用内置默认）。

## 用户可见规则
- 错误语义对齐 MySQL 常用号段：1062 唯一冲突、1213 写写冲突（可重试）、
  1027 节点不可达、1075 复合 pk 含 AUTO_INCREMENT、1235 未支持的类型/子句、
  1292 值超 DECIMAL 列界；DDL 进事务被拒；并发 `CREATE TABLE` 由同一 raft 写守卫
  窗口内的单一目录决策仲裁（table-id 分配 + 变更同窗口生效），败者收到 1050
  （表已存在）。
- 表达式遵循 SQL 三值逻辑：与 NULL 比较为 NULL，`NOT NULL -> NULL`；`IN` 先短路
  相等命中，否则见过 NULL 即返回 NULL（`NOT IN` 同理）。`length()` 按字节计数，
  `char_length()` 按字符计数。`Int / Int` 仍为整除（MySQL 对 `/` 返回小数）——
  已知偏差，见限制清单。
- 列元数据带可空/主键标志：SHOW COLUMNS 与线协议列定义置 `NOT_NULL_FLAG` /
  `PRI_KEY_FLAG`（复合 pk 的各列均置）；自动分配主键的 INSERT 的 OK 包携带
  `last_insert_id`（非 INSERT 语句为 0）。
- NULL 不进索引（唯一列多个 NULL 合法）；墓碑行由后台 GC 按最老活跃快照水位清理。

## 限制清单
- DECIMAL 不可作 pk 列（1235）、不可进 columnar 表（1235）。
- 复合 pk 列类型仅 Int/VarChar/Date/DateTime；复合 pk 无 AUTO_INCREMENT（1075）。
- 集群模式索引点查退化为 scatter-gather 全表拉取（EXPLAIN Gather banner）——
  结构性分流维持现状，非 flag。
- `Int / Int` 整除：与 MySQL（返回小数）有差异，本轮未改。
- 无锁等待超时、无死锁检测：锁读冲突 1205 立即失败，从不阻塞。
- 隔离级别仍为既有的快照隔离一级（对外呈现 REPEATABLE-READ；`SET TRANSACTION
  ISOLATION LEVEL` 接受并回显，但不改变行为）。

## 相关
- 实现模块：[agents/rust/sql.md](../agents/rust/sql.md)；契约与偏差：
  `COMPAT.md` "SQL data plane" 节。
