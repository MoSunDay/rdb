# CI 质量门禁修复：fmt/clippy 债清偿 + kafka e2e 模块双重加载 + 失败名观测

Commit: 2a364d9（fmt/clippy/测试修复）、fccbd67（CI 失败名观测）

## 背景
`e2d4fad` 起 CI 第一步 `cargo fmt --check` 红，clippy/test/release 五天（09-18→09-23）未执行——fronts 各提交实际带着零 CI 佐证合入。fmt 修复后暴露两层被掩盖的债务：

1. **clippy `-D warnings` 共 27 错**（bench/es/kafka/s3/search：复杂类型、map_or、clamp、冗余闭包、too_many_arguments 等，全部机械修复，含 3 个 `type` 别名与惯例 `#[allow(clippy::too_many_arguments)]`）。
2. **6 个 kafka e2e crate 双重加载 `tests/common`**：`tests/kafka_front_common/mod.rs` 以 `#[path = "../common/mod.rs"]` 二次包含，rustc 仅告警（`cargo test`/soak 因此一直绿），CI 的 `-D warnings` 升级为错误。修复为复用各测试根的 `mod common;`（`use crate::common::TOKEN`）。**新增 e2e 共享模块时不得再用 `#[path]` 重复包含 `tests/common`**；共享助手按二进制部分使用属常态，模块顶部 `#![allow(dead_code)]`（同 `tests/common/mod.rs` 惯例）。

## CI 观测缺陷
test 步骤的 `set -e` + pipefail 在 cargo 失败时直接终止脚本，`::error title=test-failures` 失败名注释从未发出（run 35870416460 以 exit 101 失败、零失败名）。改为管线期间 `set +e` 并加 `--no-fail-fast`（run 35873131585 起生效）。

## 验证与残留
- `fccbd67` CI 五步全绿（fmt/clippy/test/release/双 guard）；本地 `cargo test --workspace` 三次全过（16 核×2、4 核亲和×1，40 个测试二进制）。
- **残留**：run 35870416460 的 test 步骤曾失败一次，失败名未被捕获（上述观测缺陷），同代码复跑即绿——存在未识别的偶发测试，复现条件未知；再发时失败名将出现在 check-run annotations。
