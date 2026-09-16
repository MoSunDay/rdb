# RESP 数据面上线门槛四项修复（审计收口）

Commit: 工作区变更（09e2bce 之上）

## 背景

上线标准审计结论：核心质量达标，放行前清 4 个门槛项（均为小改动）。

## 变更

1. **分发路径锁中毒韧性**（`src/command/mod.rs`、`src/resp/conn.rs`）：`redirect_line`
   三处路由锁读取（importing/topology/migrating）由 `.unwrap()` 改为
   `unwrap_or_else(PoisonError::into_inner)`（`ds/latch.rs` 范式）——控制面 writer 持锁
   panic 不再级联打崩每条跨节点命令的连接 task；`dispatch` 内 `redirect_line` 调用与
   `process_command` 内 `queue_command` 调用补 `catch_unwind` 覆盖（对齐 handler 的
   `fatal error` + 关连接契约）。
2. **备份只读白名单补齐**（`src/command/readonly.rs`）：新增 JSON 纯读全族
   （get/mget/type/strlen/arrindex/arrlen/objkeys/objlen）与 FT.\* 纯读
   （ft.info/ft.search）——failover 切 backup 后与 VSIM 等读家族对称；
   JSON/FT 变更动词仍拒 `-READONLY`。
3. **已认证连接 inline 行长上限**（`src/resp/codec.rs`）：无换行 inline 行超 64KB
   （Redis `PROTO_INLINE_MAX_SIZE` 对齐，严格大于）回
   `-ERR Protocol error: too big inline request` 并关连接，消除单连接向 1GB 累计上限
   的内存 pin；有换行的行不受限（Redis 同语义）。
4. **nightly soak CI**（`.github/workflows/soak.yml`、`scrtips/e2e_scenarios/soak_kill9.sh`）：
   cron '17 3 * * *' 跑 900s kill -9 soak + 全量 e2e 场景；soak 增加
   `RDB_SOAK_P99_MAX_MS`（默认 1000，0 关闭）每轮 bench p99 断言，捕捉 LIFO 冻结类
   只在负载下暴露的回归。

回归测试：`redirect_survives_poisoned_routing_locks`、
`dispatch_survives_poisoned_routing_locks`（mod.rs 单测）、readonly 单测扩展、
`backup_listener_serves_json_and_ft_reads_but_denies_mutations`、
`inline_unterminated_line_is_capped_at_64kb`、
`inline_line_with_newline_beyond_64kb_still_parses`、
`authed_inline_line_over_64kb_gets_too_big_error_and_close`。
