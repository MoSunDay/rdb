Commit: (working-tree, 随本提交入库)

# 归档 Go 实现至 archive/go/（Rust 为唯一维护实现）

## 背景
`rust/` 实现已全量覆盖 Go 实现的能力（RESP + openraft + RocksDB，见
[2026-08-17/rust-rewrite.md](../2026-08-17/rust-rewrite.md) 与
[2026-08-18/rust-import-ds-p0-p1.md](../2026-08-18/rust-import-ds-p0-p1.md)），
双实现并行维护成本高于收益。Go 实现整体归档：**git mv 保留历史**（31 条 R 记录），
不再接收缺陷修复与新特性；Rust 行为差异以 [rust/COMPAT.md](../../../rust/COMPAT.md) 为准。

## 变更
- `cmd/`、`internal/`、`examples/`、`go.mod`、`go.sum` → `archive/go/`（git mv，31 文件）。
- `agents.md`：总览改为「Go 已归档、Rust 唯一维护」，归档路径说明。
- `agents/{server,rcache,command,store}/index.md`：加【已归档】banner；
  `agents/rust/index.md`：标注为当前实现入口。
- 根目录不再有 `go.mod`；`scrtips/`、`config/` 仍在仓库根（Rust 复用，不动）。

## 测试覆盖
- 归档为纯移动，无代码语义变化；Rust 侧不受影响。
- 全量回归：`cargo test --workspace` → 456 passed（363 lib + 93 e2e）
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → 零警告

## Impact Surface
- 对外行为零变化；仅源码布局变化。
- 任何引用旧路径（`internal/...`、`cmd/rdb`）的本地脚本/文档需改指 `archive/go/...`。
- Go 实现自本提交起冻结：缺陷不再修复，COMPAT.md 中「Go bug-parity」项逐步由
  Rust 侧修复收编（见同日 [rust-redis-semantics-breaking.md](./rust-redis-semantics-breaking.md)）。

## Related Docs
- [agents/rust](../../agents/rust/index.md)
- [rust/COMPAT.md](../../../rust/COMPAT.md)
