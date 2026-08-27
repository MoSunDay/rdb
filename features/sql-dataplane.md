Commit: d481b1d708c248f86be394189d01ca7305fc8528
# SQL 数据面（MySQL 协议 + 分布式事务）

## 能力
- 任意 rdb 节点开启 `mysql_bind` 后即是一个 MySQL 服务端：native-password 登录
  （`mysql_user`/`mysql_password` 配置），支持 CREATE/DROP TABLE、CREATE/DROP INDEX、
  INSERT/UPDATE/DELETE、SELECT（WHERE/ORDER BY/LIMIT/DISTINCT/JOIN/GROUP BY/
  HAVING/聚合）、SHOW TABLES/COLUMNS/INDEX、EXPLAIN、`?` 预编译语句；列类型
  BOOL/BIGINT/DOUBLE/VARCHAR/BLOB/DATE/DATETIME/TIMESTAMP（TIMESTAMP 为 DATETIME
  别名，`NOW()`/`CURDATE()` 等时钟函数可用；TIME/DECIMAL 仍不支持）。
- 表目录经 raft 复制：集群内任一节点建表，全集群可见；DDL 仅 leader 生效
  （follower 收到会得到 "not leader" 类错误，客户端重试即可）。
- 事务：`BEGIN`/`COMMIT`/`ROLLBACK` 快照隔离——事务内重复读稳定（repeatable read）、
  自写可见、断连自动回滚；两个并发事务改同一主键，后提交者得到 1213 冲突错误。
- StarRocks 模型头：`PRIMARY KEY(...)`（行存 upsert）与 `DUPLICATE KEY(...)`
  （列存追加）可直接写在建表语句里；未识别子句与 `AGGREGATE KEY` 以 1235 大声拒绝。
- 索引：单列二级索引与唯一索引（唯一冲突报 1062）；带索引的等值/IN/BETWEEN 查询
  走索引点查（EXPLAIN 可见 IndexScan）。
- 列存表：`CREATE TABLE ... ENGINE=columnar`——追加式（仅 INSERT，UPDATE/DELETE/
  索引不支持），每次提交每表生成一个不可变列式段文件；读为全段扫描
  （WHERE/聚合/JOIN 照常生效）。详见 `COMPAT.md` "Columnar table engine" 节。

## 集群行为（3 节点及以上）
- 时间戳全局化：`CLUSTER INIT` 后所有事务时间戳由 raft leader 块授权
  （HTTP `/sql/ts`），跨节点提交顺序一致。
- 分布式写：任一节点可写任意主键——数据按 slot 归属自动路由到各节点，
  跨节点原子性由 2PC 保证（prepare→决议持久→commit 两阶段）；参与节点宕机时
  写入整体失败，不留半行数据；重启后自动恢复在疑事务。
- 分布式读：单表 SELECT 从各节点按 slot band 并发拉取后合并过滤；任一数据节点
  不可达则查询报错（不返回部分结果）。
- 限制（v1）：集群模式下索引点查暂不可用（退化为全表 gather；JOIN 两侧均按
  gather 物化后在协调者做嵌套循环，EXPLAIN 显示 `Gather(join)`）；SQL 读路径不参与
  RESP 侧 HA 故障切换。

## 配置
- `mysql_bind` / `mysql_user` / `mysql_password`：MySQL 接入。
- `sql_rpc_bind`：节点间 SQL RPC（scatter-gather/2PC），空=关闭（单机语义）。
- `columnar_flush_rows` / `columnar_flush_bytes`：列存单语句/事务-表追加限额
  （默认 65536 行 / 64 MiB，0=用内置默认）。

## 用户可见规则
- 错误语义对齐 MySQL 常用号段：1062 唯一冲突、1213 写写冲突（可重试）、
  1027 节点不可达、DDL 进事务被拒；并发 `CREATE TABLE` 由同一 raft 写守卫窗口
  内的单一目录决策仲裁（table-id 分配 + 变更同窗口生效），败者收到 1050（表已存在）。
- 表达式遵循 SQL 三值逻辑：与 NULL 比较为 NULL，`NOT NULL -> NULL`；`IN` 先短路
  相等命中，否则见过 NULL 即返回 NULL（`NOT IN` 同理）。`length()` 按字节计数，
  `char_length()` 按字符计数。
- 列元数据带可空/主键标志：SHOW COLUMNS 与线协议列定义置 `NOT_NULL_FLAG` /
  `PRI_KEY_FLAG`；自动分配主键的 INSERT 的 OK 包携带 `last_insert_id`
  （非 INSERT 语句为 0）。
- NULL 不进索引（唯一列多个 NULL 合法）；墓碑行由后台 GC 按最老活跃快照水位清理。

## 相关
- 实现模块：[agents/rust/sql.md](../agents/rust/sql.md)；契约与偏差：
  `COMPAT.md` "SQL data plane" 节。
