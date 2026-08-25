# AUTO_INCREMENT 主键自动分配（MySQL 兼容 Phase 1）

## 背景
MySQL 兼容 Phase 1 计划 1.8：此前 `AUTO_INCREMENT` 语法可解析但无行为——INSERT 不写
该列即报 NOT NULL 约束错误，应用侧无法用自增主键建表。本次补齐分配语义、计数器持久
化与 `LAST_INSERT_ID()`。

## 修复
- **DDL 校验**（`exec/ddl.rs` `build_schema`）：至多一个自增列、仅整数类型、必须是主键
  （MySQL 1075 文案）；旧 catalog JSON 无该字段可正常加载（`#[serde(default)]`）。
- **计数器是 raft 复制的 catalog 状态**（`storage/catalog.rs`）：FSM key
  `sql_sequence/<table>` 存十进制"下一个值"。CREATE 置 `"1"`，DROP 清空；重启/换主后
  续号延续。
- **分配语义**（`exec/sequence.rs`，纯函数 + 一个原子镜像）：
  - 缺列 / NULL / 0 → 自动分配连续 id（MySQL 默认 sql_mode 的 0 行为）；
  - 显式值 ≥ 当前水位 → 立即抬水位到 `值+1`（同语句后续行从其上继续）；低于水位不回退；
  - 显式非正值（0 除外）按字面保留；
  - 批量预留：每条自动分配语句按 `RESERVE_BATCH=64` 向前预留（接受空洞，与 MySQL
    一致），显式值语句只持久化 `值+1` 不预留。
  - 分配是 leader 上的一次串行读改写：与 DDL 同一把 raft 写锁 + 同一条
    `put_kv` 提交路径，并发 INSERT 不可能拿到同一水位；非 leader 节点报
    `AUTO_INCREMENT allocation requires the raft leader`（与 DDL 同规则）。
- **`LAST_INSERT_ID()`**（`exec/expr.rs` 2 行分发）：返回本连接最近一次自动分配的首个
  id；`LAST_INSERT_ID(n)` 置值并返回 n（MySQL 行为）。会话值存
  `SqlSession::last_insert_id`。
- **已知偏差**（COMPAT.md 已记录）：表达式求值取不到会话，`LAST_INSERT_ID` 另维护一个
  进程级原子镜像——多连接同进程时互相可见；列元数据为 VARCHAR 而非 BIGINT（受
  select.rs 归属约束，Phase 1 不改）。

## 验证
- 新增 `tests/auto_increment_e2e.rs`（3 用例）：
  - `auto_increment_full_flow`：DDL 拒绝（双自增/非主键/非整数）；多行 NULL/0 → 1,2,3；
    显式 100 后续号 101；`LAST_INSERT_ID()` / `LAST_INSERT_ID(7)`。
  - `counter_survives_restart`：进程重启后续号从预留水位 65 起（空洞可见）。
  - `cluster_allocates_unique_ids_through_raft`：3 进程集群，70 行跨预留批仍连续；各
    节点 gather 读到同一完整唯一 id 集；follower INSERT 被拒（leader-only）。断言前
    先做一次跨 band 显式 id 写入推进全部参与者的时间戳（`global_hi` 滞后是既有读语
    义，见 `tx/global.rs` 模块注释）。
- 变红验证：`git stash push -- src/` 后 `auto_increment_full_flow` 红——
  `ERROR 1048 column 'id' cannot be null`；恢复后绿。
- 单测：`sequence.rs` 7 个（纯函数 + stub 引擎 INSERT 路径：连续分配/显式抬升/批量预留
  持久化）、`schema.rs` serde 兼容 2 个、`ddl.rs` 校验与计数器生命周期 2 个。
- 旧 catalog JSON 反序列化兼容由 `#[serde(default)]` 测试覆盖；17 个测试 fixture 因新增
  `TableSchema.auto_increment` 字段机械补 `auto_increment: None,`。
- 全量回归：fmt / clippy `-D warnings` / `cargo test --workspace` / release build 全绿。
