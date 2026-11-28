# 2026-11-27 lite-stream-keyspace-closure：批次 14 五项缺陷闭环计划

> 状态：proposed
> 日期：2026-11-27
> 输入：`features/changelog/2026-11-27/multistore-scenarios-batch14.md`（§真实行为偏差；flake 记录 :132-138）
> 关联：`plans/2026-10-06-mysql-hardening/05-h4-test-hygiene.md`（H4-3 join head timeout，WP4 落地后回写）；
> `COMPAT.sql.md` turso 偏差台账；缺陷条目格式 `features/changelog/2026-10-08/mq-defect-fixes.md`

批次 14（#161–#175，`923ec7a`）落地过程暴露 5 项问题：1 项功能缺陷根因（lite 流名在键空间
命令中不可见，连带 WATCH 不触发）、1 项 SQL 渲染缺陷、2 项测试夹具 flake、1 项性能疑点。
本计划只做闭环，不新增能力。

## 0. 决策与范围

- **D1（已决）**：R1 取形态 **A（命令层解析）**。否决 B（lite 改按子槽存储，迁移面大）与
  C（router 对 X 族特判——X 族本就是节点本地命令，路由层不该知道存储布局）。
- 单文档承载（README，turso-acceptance 惯例），WP 不另拆号；**每个 WP 一个本地提交**。
- 批次 14 的推送授权已消耗：本计划全部提交**仅本地**，推送需另行授权。

## 1. 根因台账

| # | 症状 | 根因（file:line） |
| --- | --- | --- |
| R1 | WATCH lite 流名永不触发（#162 退腿）；RENAME/EXISTS/TYPE 看不见流（#163 仅靠同槽不动点名才能改名） | lite 以**父槽前缀 + 全名**存流（`src/lite/append.rs:53-57`、`entries.rs:89-95`、`model.rs:112-119 stream_prefix`），X 族节点本地（`lite/mod.rs:5-8`、`router.rs:115-130`）；键空间命令却拿 `slot(hash(全名))/`（`command/mod.rs:444-450`）并走通用解析（`keys_core.rs:111-141`）。WATCH 脏检查在 `w.prefix` 下重算物理键哈希（`tx_cmd.rs:65-75`、`tx/exec.rs:70-73`、`tx/watch.rs:28-65`）→ 永远比对不到。RENAME：`keys.rs:320-336 require_same_slot` 按全名 → `rename_key`（`keys_core.rs:361-375`、`move_family` 411-503）→ `Missing` → `-ERR no such key`（`keys.rs:347`）。次生：rename/DEL 不触碰 `OffsetCache`（`offset.rs:52-55` 读穿；`keys_core.rs:194-199` 已承认 DEL 缺口） |
| R2 | turso `IS NULL` 报 1064 `near ")": syntax error` | `src/sql/turso/translate/expr.rs:33-36` `format!("({} IS{}) NULL", ..)` 渲染成 `(x IS) NULL` / `(x IS NOT) NULL`，括号错位；#171 turso 腿因此被摘 |
| R3 | `rocksmq_http_e2e` 偶发 panic `mid > len` | `tests/rocksmq/rocksmq_http_e2e.rs:152-171 resp_cmd` 固定 300ms 排水 + `split_at(5)`；4 个调用点 `:249,:260,:282,:298`；未用 `common::cmd_one_shot`（`tests/common/mod.rs:634-647`，10s 超时） |
| R4 | `join raft cluster failed` 偶发使节点 `exit(1)` | `/join` 首字节超时 `HEAD_TIMEOUT=10s` 硬编码（`src/rcache/http.rs:59,163-164`；`join.rs:16`），失败即 `exit(1)` 无重试（`src/main.rs:374-381`）；harness 仅对 EADDRINUSE 重生（`tests/common/mod.rs:189-236`）。自 2026-09-17 已知（`sql-w2-decimal-composite-pk.md:76-79`） |
| R5（可选） | MySQL 前端每结果集约 42ms | `src/sql/front/serve.rs:44-61` accept 后未 `set_nodelay`（RESP 监听 `resp/mod.rs:37-41` 同缺）；对照 `sql/dist/server.rs:63`、`sql/dist/client.rs:22`、`rcache/transport.rs:239-245` 均已设置；opensrv-mysql 0.7.0 内部是否设置未证实——**需实测前后对比才能定性** |

## 2. 工作包

### WP1 R1：键空间命令按父槽解析流名（形态 A）

目标：`WATCH/EXISTS/TYPE/DEL/EXPIRE/RENAME/RENAMENX` 对能被 `lite::parse_topic_name`（`lite/mod.rs:75`）
解析为 Stream 的名字，以 `lite::model::stream_prefix(name)` 为前缀、全名为用户键。

| # | 改动 | 文件（行数预算） |
| --- | --- | --- |
| 1.1 | 新增纯函数 `stream_key_target(name) -> Option<(prefix, key)>`：`parse_topic_name` 为 Stream 且 `stream_prefix` 为 Some 时返回 (父槽前缀, 全名) | `src/lite/model.rs` 426→≈440 |
| 1.2 | `resolve_with` 接入 1.1（覆盖 EXISTS/TYPE/DEL/EXPIRE 等全部通用解析调用方） | `src/command/keys_core.rs` 504→≈520 |
| 1.3 | 新增 `Runtime::stream_renamed(old, new)`：`offset::flush_dirty` → `offset::remove_stream(old)`（`offset.rs:366`）→ `ordered::drop_stream(old)`（`ordered.rs:277`）；**不入 `reaps` 队列**（那是 `stream_reaped` :170-177 给死流用的）。若 1.5 判定 reaper 不清旧名台账，则在此显式删除旧名持久化台账 | `src/lite/mod.rs` 445→≈465 |
| 1.4 | RENAME/RENAMENX：两名均为 Stream 时按**父槽**做同槽校验（`ds/setops.rs:27-35` 改为接受已解析前缀），调用新的 `rename_stream`：按 1.3 驱逐 → `move_family`（`keys_core.rs:411-503`）以 `stream_prefix(src)` 为源、`stream_prefix(dst)` 为目标 → OK；一流一非流保持 `-ERR` | `keys.rs` 361→≈400、`keys_core.rs` ≈520→≈560、`setops.rs` 215→≈230 |
| 1.5 | DEL 命中 Stream 名：`delete_key` 后调用 `stream_reaped`；核实 `drain_reaps`（`lite/mod.rs:382`）是否清理旧名持久化 offset 台账，结论写入 changelog | `keys_core.rs:194-199` |
| 1.6 | WATCH：注册与脏检查统一用 1.1 的目标，`tx_cmd.rs:65-75`、`tx/exec.rs:70-73`、`tx/watch.rs:28-65` 三处共用同一解析函数 | `tx_cmd.rs` 182→≈195、`exec.rs` 119、`watch.rs` 162 |
| 1.7 | 单测：`keys/tests.rs` 增 `rename_stream_ordinary_name_moves_entries_and_evicts_offsets`、`exists_type_del_see_stream`；`tx_cmd.rs:139-165` 增 WATCH 流名脏检查用例；`lite/mod.rs` 增 `stream_renamed` 驱逐用例 | `keys/tests.rs`（若 >800 按 stream 拆子文件） |
| 1.8 | e2e 回填：#162 改用**普通流名**断言 WATCH 触发 → EXEC 返回 nil，EXISTS/TYPE 可见流；#163 增普通流名 RENAME 腿（条目迁移 + XLEN 守恒 + 旧名 EXISTS=0），删除文件头偏差注释；两文件保持 <400 | `scenario_watch_json_crossfront_e2e.rs` 282→≈295、`scenario_rename_typed_families_e2e.rs` 357→≈380 |

必须继续通过：`tests/kafka/kafka_rename_ledger_e2e.rs`（同槽不动点名，形态 A 下父槽=全名槽，行为不变）、
`keys/tests.rs:626-694 rename_stream_moves_offset_ledger`（随 1.4 调整签名）、`src/lite/offset.rs:464-611`。

已知局限（入台账，不在本 WP 解决）：cluster 模式下 X 族节点本地，流名键空间命令仅在持有该流的节点生效；
RENAME **不迁移**消费组水位/PEL（新名 read-through 为空）。

### WP2 R2：turso IS NULL 渲染

| # | 改动 | 文件 |
| --- | --- | --- |
| 2.1 | 改为 `"({} IS{} NULL)"`（括号归位） | `src/sql/turso/translate/expr.rs:33-36` |
| 2.2 | `#[cfg(test)]` 渲染单测：列与嵌套表达式 × IS NULL / IS NOT NULL | `expr.rs` 180→≈220 |
| 2.3 | turso e2e 增空值谓词块（IS NULL / IS NOT NULL / `= NULL` 空集 / `<=>` 仍钉 1235 `turso_crud_e2e.rs:234-241`）；#171 把 turso 侧 IS NULL 腿加回，双前端同断言 | `tests/turso/turso_crud_e2e.rs` 259→≈300、`scenario_sql_conjunct_join_null_e2e.rs` 265→≈280 |

### WP3 R3：RocksMQ http e2e 夹具
`tests/rocksmq/rocksmq_http_e2e.rs`：加 `mod common;`（同 `rocksmq_batch_range_e2e.rs:10`），删 `resp_cmd`（152-171），
4 个调用点改 `common::cmd_one_shot`；`:250` `":1\r\n"` 与 `:302` bulk 断言因 bulk 回包无尾 CRLF 改前缀匹配。423→≈410。

### WP4 R4：harness 对 join 超时重生
`tests/common/mod.rs` 已**恰好 800 行**，不能内联：把 `spawn_child`/`SPAWN_ATTEMPTS`/`spawn_node`/`spawn_node_yaml`
（:150-256）整体迁到新文件 `tests/common/spawn.rs`（≤200 行，挂载同 `lite.rs`/`mq.rs`/`mysql.rs`），`mod.rs` 回到 ≈700。
在 `spawn.rs` 内：子进程早退且 stderr 含 `main.rs:374-381` 的 join 失败信息 → 视同 EADDRINUSE 路径重生，
`JOIN_ATTEMPTS = 3`，300ms 退避，换新端口。**src 侧 `HEAD_TIMEOUT`/`JOIN_TIMEOUT` 不动**（生产缺省，H4-3 原则）。

### WP5 R5（可选，待确认）：监听 `set_nodelay`
`src/sql/front/serve.rs:44-61`、`src/resp/mod.rs:37-41` accept 后 `set_nodelay(true)`（对照 `sql/dist/server.rs:63`）。
前置：先用 `scripts/` 现有压测脚本实测前后各 3 轮，有数据才提交。

## 3. 门槛
- WP1：`cargo test --lib`；`--test lite --test kafka --test cluster`；`--test scenario_watch_json_crossfront_e2e --test scenario_rename_typed_families_e2e`。
- WP2：`cargo test --lib sql::turso`；`--test turso_crud_e2e --test turso_parity_e2e --test scenario_sql_conjunct_join_null_e2e`。
- WP3：`--test rocksmq_http_e2e` 连跑 3 次。
- WP4：**全工作区连跑 2 次**均无 flake；cluster 相关 `--test` 连跑 3 次。
- 收尾：`cargo clippy --workspace --all-targets -- -D warnings`；全工作区 1 次（预期 ≥ 2186 + 新增）；场景 175/175。

## 4. 显式不做 / 登记
- 形态 B、C；XADD 集群放置（流保持节点本地）。
- EXEC 对排队 XADD 的闩锁 `latch_key(slot_prefix(multi.slot), k)`（`tx/exec.rs:57-62`）与 lite 元闩锁（`append.rs:57`）不一致 → 登记为后续缺陷。
- turso `InList` 空列表无守卫（`expr.rs:38-54`）→ 登记。
- 流 RENAME 水位/PEL 不迁移 → 台账已知局限（除非 1.5 发现 reaper 可顺带覆盖）。
- R4 src 侧重试/超时调整；R5 未确认前不做。

## 5. 提交与收尾
本地提交序列（不推送）：① `fix(lite): resolve stream names by parent slot in keyspace commands; evict offset cache on rename/del`
② `fix(sql/turso): render IS NULL / IS NOT NULL correctly` ③ `test(rocksmq): use shared one-shot reply helper in http e2e`
④ `test(harness): respawn node on transient join head timeout` ⑤（可选）`perf(sql): TCP_NODELAY on front listeners`
⑥ `docs: batch-14 defect closure`。
文档：新建 `features/changelog/2026-11-27/batch14-defect-closure.md`（`## 缺陷 N：` 格式 ×5，含 sha）；
`COMPAT.sql.md` turso 段注记 IS NULL 已修（若台账惯例要求）；`agents/rust/index.md` 加两条已知局限；
`05-h4-test-hygiene.md` H4-3 回写"由 closure WP4 落地"；本计划状态 → landed，`plans/index.md` 同步。
