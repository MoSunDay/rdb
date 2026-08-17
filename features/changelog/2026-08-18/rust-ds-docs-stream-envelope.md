# 七类数据结构收尾前置：stream 定形说明与 typed 信封物理编码补记

## 背景
P0（[2026-08-18/rust-import-ds-p0-p1.md](../2026-08-18/rust-import-ds-p0-p1.md)）落地 `ds/codec.rs` typed 物理编码时承诺在 `rust/COMPAT.md` 补记信封编码细节；stream 类语义在 P2 收尾后定形（Lite Mode 承接，非 Redis Streams 全仿真），需在文档层面固化，为 P3 JSON / P4 VectorSet 收尾清障。

## 变更（纯文档，零代码）
- **`agents/rust/index.md`**：`lite/` 模块条目补 stream 定形说明——RocketMQ 主题/队列语义模型；`KIND_STREAM_PEND 0x0F` 仅为将来完整 PEL 预留，Lite 实现不落盘该 kind（组已提交水位用 kind-0x0E 记录）。
- **`features/kv-storage/index.md`**：能力清单补流数据条目（Lite Mode 定形承接）；限制句同步改写（stream 定形、json/vector-set 待后续阶段）。
- **`rust/COMPAT.md`**：新增「Typed record physical encoding (Rust data plane)」一节——派生键/信封/过期索引物理布局、kind 0x00 raw string 裸布局例外与迁移行为、扫描分类规则及其碰撞取舍、按 kind 分段的家族删除规则；声明 Go/Rust 落盘格式不可互换。

## 验证
- 纯文档变更，无代码路径改动；基线 `cargo test --workspace` 实跑 261 项全绿（rdb lib 259 + bench 2）。
