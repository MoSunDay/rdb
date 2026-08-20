Commit: (working-tree, 随本提交入库)

# 上线前 P2 扫除（5 项）+ GitHub Actions 质量门禁

## 背景
上线前第三轮（也是最后一轮）扫除：复核确认的 5 个 P2 缺陷全部修复，
并补齐 CI 门禁（fmt → clippy `-D warnings` → 全量测试 → release 构建），
使 `rust/` 达到上线准备状态。

## P2 修复
- **SPOP 损坏态 panic**（`command/set_cmd.rs`）：meta 计数 >0 而成员物理
  记录全失时，`% 0` 除零与 `picked[0]` 越界两处雷；现在显式回
  `-ERR: spop failed`（不伪造 null 成功），`want` 双路径统一 clamp 到
  物理成员数。
- **RPC 双倍超时预算**（`rcache/transport.rs`）：`ensure_connected` 与
  `roundtrip` 各享 10s（最坏 20s）偏离 Go 单一 stream deadline 语义；
  改为共享一个 `RPC_TIMEOUT` deadline（`timeout_at`），错误映射
  （Unreachable/Timeout/network）不变。
- **VREM/VSETATTR 吞读错误**（`command/vectorset_cmd.rs`、
  `vectorset_attr.rs`）：`read_elem` 的 `Err` 原被 `unwrap_or(false/None)`
  吞成 `:0`（客户端误判元素不存在而 meta 基数仍在）；对齐 vgetattr
  原则改为 `-ERR`。
- **Lite `decode_entry` OOM**（`lite/model.rs`）：损坏记录伪造
  `n=u32::MAX` 直接 `with_capacity` ~25GB；加 `n > body.len()/8` 上界
  守卫（每对至少 8 字节头，不误杀合法记录），返回 None 走既有跳过路径。
- **backup_map_init 首 tick 立即执行**（`rcache/ha.rs`）：tokio interval
  首次 tick 立即完成，使循环体立即运行而非"先睡 1s"；loop 前先消费
  首 tick，与注释/Go 行为对齐。

## CI
- 新增 `.github/workflows/ci.yml`：ubuntu runner 预装 clang/libclang
  （librocksdb-sys 捆绑 C 源码编译），顺序 fmt --check → clippy
  `-D warnings` → `cargo test --workspace` → `cargo build --release`；
  `rust/.cargo/config.toml` 已入库，tokio LIFO cfg 自动生效。

## 测试覆盖
- 新增 `set_tests::spop_corrupt_empty_members_fails`（物理删成员构造
  损坏态，两条 SPOP 形态均回 -ERR）、`model::decode_entry_rejects_
  corrupt_huge_count` / `decode_entry_roundtrip_and_truncated`（伪造
  计数拒绝 + roundtrip/截断安全）。
- transport/ha 改动靠既有回归兜底：`raft_transport`、`ha_failover`、
  `raft_cluster_e2e`、`lite_e2e` 全绿。
- 全量：`cargo test --workspace` → 539 passed, 0 failed（26 个测试二进制，含 3 新增用例），
  clippy `-D warnings` 零警告。

## Impact Surface
- SPOP/VREM/VSETATTR 仅在损坏态/存储读错误下从"静默错误回复/panic"变为
  `-ERR`，正常路径行为零变化。
- transport 最坏 RPC 时延 20s → 10s（恢复 Go 语义）。
- CI 为新增门禁，不影响运行时。

## Related Docs
- [agents/rust](../../agents/rust/index.md)
- [rust/COMPAT.md](../../../rust/COMPAT.md)
