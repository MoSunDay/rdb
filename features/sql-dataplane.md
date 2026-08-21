Commit: c0cce389e75f34cf39c06ac4b56f22cde7efd1f3
# SQL 数据面（MySQL 协议 + 分布式事务）

## 能力
- 任意 rdb 节点开启 `mysql_bind` 后即是一个 MySQL 服务端：native-password 登录
  （`mysql_user`/`mysql_password` 配置），支持 CREATE/DROP TABLE、CREATE/DROP INDEX、
  INSERT/UPDATE/DELETE、SELECT（WHERE/ORDER BY/LIMIT/DISTINCT/JOIN/GROUP BY/
  HAVING/聚合）、SHOW TABLES/COLUMNS/INDEX、EXPLAIN、`?` 预编译语句。
- 表目录经 raft 复制：集群内任一节点建表，全集群可见；DDL 仅 leader 生效
  （follower 收到会得到 "not leader" 类错误，客户端重试即可）。
- 事务：`BEGIN`/`COMMIT`/`ROLLBACK` 快照隔离——事务内重复读稳定（repeatable read）、
  自写可见、断连自动回滚；两个并发事务改同一主键，后提交者得到 1213 冲突错误。
- 索引：单列二级索引与唯一索引（唯一冲突报 1062）；带索引的等值/IN/BETWEEN 查询
  走索引点查（EXPLAIN 可见 IndexScan）。

## 集群行为（3 节点及以上）
- 时间戳全局化：`CLUSTER INIT` 后所有事务时间戳由 raft leader 块授权
  （HTTP `/sql/ts`），跨节点提交顺序一致。
- 分布式写：任一节点可写任意主键——数据按 slot 归属自动路由到各节点，
  跨节点原子性由 2PC 保证（prepare→决议持久→commit 两阶段）；参与节点宕机时
  写入整体失败，不留半行数据；重启后自动恢复在疑事务。
- 分布式读：单表 SELECT 从各节点按 slot band 并发拉取后合并过滤；任一数据节点
  不可达则查询报错（不返回部分结果）。
- 限制（v1）：集群模式下索引点查与 JOIN 暂走全表 gather；SQL 读路径不参与
  RESP 侧 HA 故障切换。

## 配置
- `mysql_bind` / `mysql_user` / `mysql_password`：MySQL 接入。
- `sql_rpc_bind`：节点间 SQL RPC（scatter-gather/2PC），空=关闭（单机语义）。

## 用户可见规则
- 错误语义对齐 MySQL 常用号段：1062 唯一冲突、1213 写写冲突（可重试）、
  1027 节点不可达、DDL 进事务被拒。
- NULL 不进索引（唯一列多个 NULL 合法）；墓碑行由后台 GC 按最老活跃快照水位清理。

## 相关
- 实现模块：[agents/rust/sql.md](../agents/rust/sql.md)；契约与偏差：
  `rust/COMPAT.md` "SQL data plane" 节。
