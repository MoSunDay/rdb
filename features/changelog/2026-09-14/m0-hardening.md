Commit: (working-tree, 随本提交入库)

# M0 加固：tokio_unstable 编译期守卫、备份实例 -READONLY 门禁、凭证脚本清除、e2e LIFO 断言

## 背景
复制通道改造（W1）动工前的地基加固批次（M0）：把「二进制必须带 LIFO-slot 规避构建」
从口头约定升级为编译期强制；补上备份监听器缺失的只读门禁与过期清扫；清除仓库中的
明文凭证与失效脚本；让 e2e harness 在启动期就拒绝带隐患的二进制。

## 变更摘要
- **W0.1 编译期守卫**（`src/build_guard.rs` / `src/lib.rs` / `.cargo/config.toml`）：
  `full` 特性构建若缺 `--cfg tokio_unstable` 直接 `compile_error!`（LIFO-slot 丢唤醒
  问题的强制规避，见 COMPAT.md）。store-only 切片
  （`--no-default-features --features store`）豁免——它不启动 rdb 运行时。rustdoc 不读
  `[build] rustflags`，故补 `rustdocflags` 镜像，否则 doc-test 编译会误触守卫（本批
  实测发现并修复）。CI 新增两个守卫 step：`RUSTFLAGS=''` 全量构建必须失败、store 切片
  必须成功（空串 RUSTFLAGS 会覆盖 config.toml，已实证）。
- **W0.2 备份实例只读门禁 + active-expire 清扫**（`src/command/readonly.rs` 新增 /
  `src/command/mod.rs` / `src/resp/conn.rs` / `src/main.rs`）：
  `backup_bind` 监听器对非只读命令统一回 Redis 标准错误
  `READONLY You can't write against a read only replica.`——允许清单 78 条（纯读 +
  协议/元命令 + MULTI 控制），在 `dispatch` 查表后、路由前拦截（EXEC 重放同路径覆盖），
  MULTI 入队时同样拦截并置 dirty（EXEC 整体 EXECABORT）。Go 的 BackupServer 无此门禁
  （COMPAT.md「Intentional deviations」已记录）。备份 store 另起独立
  `spawn_active_expire` 清扫（此前只有普通监听器的 store 被清扫）。
  新增 e2e `tests/backup_readonly_e2e.rs`（真实进程 + 双监听器：写命令逐条 -READONLY、
  读放行、普通端口不受影响、MULTI→EXECABORT、进程存活）。
- **W0.3 凭证/失效脚本清除**（删除 `scrtips/update_rdb_force.sh`、`scrtips/startup.sh`）：
  前者内嵌明文部署 API token（安全约束违例），后者是 Go 时代的 `go build
  cmd/rdb/main.go` 启动流程，对 Rust 实现已失效。全仓 grep 验证零引用、token 字符串
  仓库内零残留。
- **W0.4 e2e LIFO 启动断言 + tag 发布**（`scrtips/e2e_scenarios/env.sh` /
  `run_all.sh` / `.github/workflows/ci.yml`）：env.sh 在集群拉起后逐节点断言 stderr
  含 `tokio LIFO slot: disabled` 横幅且无 DANGER 变体；run_all.sh 预检二进制内嵌横幅
  字面量（cfg 二选一编译，仅正确构建携带）；ci.yml 增加 `tags: ['v*']` 触发与
  `release` job（tag 推送 → 构建发布 linux-amd64 tar 包 → gh release）。

## 验证
- 守卫三向：`RUSTFLAGS='' cargo build --release` 失败（守卫文案命中）；同 flag store
  切片构建成功；正常 `cargo build --release` 成功。CI 以独立 step 固化。
- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` 干净；
  `cargo test --workspace` 全绿（含 doc-tests、`backup_readonly_e2e`）。
- `scrtips/e2e_scenarios/run_all.sh`（release 二进制，LIFO 断言生效）：
  redis_session / starrocks_analytics / vector_search PASS；**mysql_orders FAIL 为
  HEAD 既有缺陷**——同一断言在未含本批改动的基线工作树（98e17a5）同样确定性失败
  （跨节点转账后 node2 读回 bob 余额丢 40），与本批无关，待 W1 排查。
- 提交门禁复核（2026-09-15）：build/clippy 干净；`cargo test --workspace
  --no-fail-fast` 除 `sql_setop_cluster_e2e` 外全部目标通过——该目标为共享宿主机
  高载抖动（当时 load avg 172；2pc participant 请求超时），隔离重跑 3/3 通过，
  且本批未触碰 sql/2pc/网络路径，与 2026-09-05 已登记的同类环境噪声一致。
