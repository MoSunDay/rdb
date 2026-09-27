# E2E 覆盖补齐：命令面 30→0 零覆盖、进程级 backup 接管、前端缺口与 CLI 场景

Commit: 98e17a5（基线；本条目所述变更在待提交工作区，落地提交后以其 sha 为准）

## 背景
2026-09-26 全量只读比对（基线 `f071032`，台账 `features/e2e-coverage.md`）量化出：
注册表 188 命令中 30 个零覆盖、RESTORE/ZREVRANGEBYLEX 无成功路径、JSON/Vector/FT 仅
in-proc 派发从未走 wire、进程级 `backup_target_map` 接管零覆盖、ES 前端无 token 负向与
routing_exception 测试、S3 checkpoint 无 retention/一致性断言、备份监听 73+ 允许命令仅
断言 ~8 个、CLI 套件无 HA/Lite-MQ/migrate 场景且文档存在“4 场景”漂移。

## Rust e2e（新增 8 个测试文件 + 3 处增补）
- 新增：`hash_read_e2e`、`set_more_e2e`、`zset_more_e2e`、`keys_more_e2e`（in-proc 精确字节）、
  `wire_families_e2e` + `wire_ft_config_e2e`（JSON/Vector 与 FT.*/CONFIG 真实 TCP）、
  `asking_restore_e2e`（ASKING 门 + DUMP→RESTORE wire 往返）、`backup_failover_e2e`
  （进程级 kill -9 → MOVED→backup → 回切）、`backup_surface_e2e`（allowlist 87/87，表驱动 +
  与 `readonly::ALLOWED` 集合相等断言，共享件在 `tests/backup_surface_common/`）、
  `es_auth_e2e`（401 矩阵 + routing_exception）。
- 增补：`list_e2e`（LPUSHX/RPUSHX）、`search_e2e`（FT.DROPINDEX=FT.DROP 连文档删，与
  RediSearch 语义差异钉住）、`kafka_group_e2e`（LeaveGroup/DescribeGroups 未知 id）、
  `s3_e2e`（checkpoint retention 剪裁 + 内容字节一致性）、`es_common`（dead_code 惯例标注）。
- 新 helper 一律放各自测试文件（`tests/common/mod.rs` 保持 752 行不膨胀）。

## 修复的真实 bug
- `src/lite/info.rs`：`XINFO GROUPS` 每组数组头 `*14` 实发 13 元素 → 严格 RESP 客户端
  （redis-cli）永久阻塞。头修正为 13；`scenario_lite_mq` 以 `timeout` 包裹 redis-cli 防回归。

## Shell 场景与卫生
- 新增 `scenario_ha_failover.sh`（yaml 循环 target 指向存活同伴的 backup 监听；首次调用
  `e2e_kill_node`；kill→MOVED-backup 实测 ~4-5s、重启回切 ~5s）、`scenario_lite_mq.sh`
  （组生命周期 + PEL + XAUTOCLAIM + ORDERED/INFLIGHT 窗口）、`scenario_migrate.sh`
  （`migrate task` 正反向全流程；MIGRATE 子命令仅小写）。
- `scenario_redis_session.sh`：redis-py `RedisCluster()` 探针**连接成功**（protocol=2 跳过
  未实现的 HELLO；COMMAND/CLUSTER SLOTS 握手通过，remote-band SET/GET 经 MOVED 路由）。
- 漂移修正：RESULTS.md/soak.yml “4 场景”→ glob 措辞；soak.yml pip 补 `confluent-kafka`；
  RESULTS.md kafka_bench “不属 run_all” 更正（glob 实际包含）。
- 安全卫生：`scrtips/redis-py.py`/`redis-async-py.py` 内嵌真实 raft token → `RDB_TOKEN`
  环境变量 + 显式 fake 占位。

## 验证
- `cargo test --workspace --no-fail-fast`：80 套件（Phase 1 时 76）全绿基线 1427-1433 用例
  （增量 = 本次新增套件）；src/lite/info.rs 修复后全量复跑见提交。
- 偶发观察：全量并行下 `blocking_concurrency_e2e`（唤醒延迟）与 `sql_2pc_e2e`（2PC 时序）
  出现过一次负载型失败，单独复跑恒绿（同 09-23 changelog 记录的偶发类）；顺手把
  `src/command/keys/tests.rs` 的 EXPIREAT+3s TTL 断言窗口放宽到 +5s（该处为可复现的临界抖动）。
- `scrtips/e2e_scenarios/run_all.sh` 本地 **9 pass / 0 fail**（含 3 个新场景）。
- `cargo fmt --check` + `cargo clippy --workspace -D warnings` 干净。
