Commit: (working-tree, 随本提交入库)

# 无损 slot 迁移：migrate task 编排 + redis-cli reshard 协议兼容

## 背景
`migrate task/list/help` 原为任务登记（下划线串经 raft 键 `migrate_task` 复制，
无实际搬迁）。本次落地完整迁移：单命令编排 + 与 `redis-cli --cluster reshard`
字节级兼容的命令面（node-id 体系、MIGRATING/IMPORTING/ASKING 门控、SETSLOT NODE
经 raft `slot_owner_map` 收敛）。

## 变更
### 编排（`src/command/migrate.rs`）
- `migrate task <slot> <src> <dst>`：MIGRATING→IMPORTING→GETKEYSINSLOT/MIGRATE
  排空→NODE（双端）→STABLE（双端）；`migrate_busy` 单飞；任务 JSON 落 raft
  `migrate_task`（覆盖语义保留，值改为 JSON）；`migrate list` 返回该 JSON。
- 数据面 `MIGRATE ... KEYS` + `RESTORE`（TTL=绝对毫秒截止，ABSTTL 兼容）实现
  dump/restore 传输：`src/ds/dump.rs` 线格式（v1 大端：version|kind|count|
  (body_len|body|value_len|value)*，0xFD 过期索引不传输，restore 重建），
  源端每 key 加锁、单条复用 AUTH 连接、非 COPY 源删、目标错误原文透传。
- 出站 RESP 客户端 `src/resp/client.rs`（connect_authed/send_command/read_reply）。
- 协议面（`src/command/cluster.rs`、`src/command/mod.rs`、`src/router.rs`、
  `src/tx/session.rs`、`src/resp/conn.rs`）：SETSLOT MIGRATING/IMPORTING/STABLE/
  NODE、KEYSLOT、GETKEYSINSLOT、ASKING 单次标志；`redirect_line` 三序门控
  （IMPORTING 无 ASKING→MOVED 源、owner_map/band→MOVED、MIGRATING 缺失键→ASK）。
- SQL 分布式路由（`src/sql/dist/mod.rs`）：Routing 携带 owner_map，
  `bands()` 按槽归属合并连续同主区间。

## 兼容性
- 与 redis-cli reshard 协议兼容的命令面（node-id = `md5_with40(addr)` 40-hex）。
- `migrate task/list` 回复文本与旧下划线格式不同（有意变更，测试同步更新）；
  `TASK` 大写落入数据面 MIGRATE。
- 迁移状态（MIGRATING/IMPORTING）为节点本地，`slot_owner_map` 经 raft 复制，
  非 leader 的 SETSLOT NODE 为乐观 +OK（编排由 leader 端驱动，风险接受）。
