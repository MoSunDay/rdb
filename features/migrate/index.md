Commit: d481b1d708c248f86be394189d01ca7305fc8528
# 数据迁移

## 能力概述
- `migrate task <slot> <src> <dst>` 一条命令完成一个 slot 的无损迁移（编排式）：
  按 redis-cli `--cluster reshard` 协议驱动 src/dst 两节点，数据经 DUMP/RESTORE 传输。
- `migrate list` 返回最近一次任务（raft 键 `migrate_task` 上的单条 JSON）。
- `MIGRATE host port ... [KEYS ...]`（Redis 数据面命令）与 `RESTORE` 提供底层传输，
  编排内部复用同一路径（源节点自行执行 dump→ASKING→RESTORE）。

## 触发方式
- `migrate task <slot> <src> <dst>`：`slot` 十进制；`src`/`dst` 为 RESP 地址
  （`host:port`）。执行于任意节点（作为编排者），成功回复 `+OK`，失败回复
  `-ERR <阶段>: <对端错误原文>`（如 `src MIGRATING: ERR Unknown node ...`）。
- `migrate list`：单条 bulk，内容为任务 JSON。

## 编排流程（`command/migrate.rs` 的 `run_migration`）
1. 计算 node-id：`md5_with40(addr)`（40 位 hex，与 `CLUSTER SETSLOT` 的 id 体系一致）；
2. src：`CLUSTER SETSLOT <slot> MIGRATING <dst-id>`；dst：`IMPORTING <src-id>`；
3. 循环：src 上 `CLUSTER GETKEYSINSLOT <slot> 1000` → 空则结束；否则
   `MIGRATE <dst-host> <dst-port> "" 0 60000 KEYS <key...>`（每次至多 1000 个，
   循环上限 1024 轮）；
4. src 与 dst 各自 `CLUSTER SETSLOT <slot> NODE <dst-id>`（更新本地 owner_map 并经
   raft 键 `slot_owner_map` 复制，旁观节点经 3s 拓扑 ticker 收敛）；
5. src 与 dst 各自 `CLUSTER SETSLOT <slot> STABLE`。

## 行为与规则
- 每步经出站 RESP 客户端（`resp/client.rs`，AUTH 使用本节点 raft_token），单步 5s 超时。
- 任务落盘为单条 JSON：`{"slot":..,"src":"..","dst":"..","status":"done|failed","moved":N}`；
  失败时 `error` 字段携带首个失败阶段的原文。raft 写为 best-effort（同 SETSLOT NODE）。
- 同一进程同一时刻只跑一个迁移：`migrate_busy` 原子标志，并发 `migrate task` 回复
  `-BUSY a slot migration is already running`。
- `migrate task` 参数不足或 `help` 回复 Go 原文错误 `-migrate [ list | task ]`；
  slot 非数字回复 `-ERR Invalid slot`。
- 小写命令注册表：`TASK` 等大写不匹配，落入数据面 `MIGRATE`（arity 错误）。

## 关键状态与异常
- 迁移中：src 对该 slot 缺失键回 `-ASK <slot> <dst>`；dst 无 ASKING 时回
  `-MOVED <slot> <src>`（单次 ASKING 标志见 `command/mod.rs`）。
- 迁移后：slot 归属写入 raft `slot_owner_map`，所有节点经 3s ticker 收敛
  （`CLUSTER NODES/SLOTS` 与 SQL 分布式路由 `sql/dist` 均以 owner_map 为准）。

## 关联逻辑模块
- [command](../../agents/rust/index.md)
- [resp/client.rs](../../agents/rust/index.md)
