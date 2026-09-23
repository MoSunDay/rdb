# Release v0.3.0 — fronts (kafka/es/rocksmq/s3) + S3 对象存储

## 版本
- **v0.3.0** = `main` 谱系，范围 `v0.2.1..v0.3.0`（19 commits）。
- semver minor：含两个 `feat`（fronts、s3），破坏性变更无。

## 主要内容
- **多前置接入**（`e2d4fad`）：kafka/ES/rocksMQ/S3 订阅与消费 fronts；
  kafka launch P0 修复（含 `2a65d1c` 文档收敛）。
- **S3 兼容对象存储**（`8823f40`）：S3 协议前置 + RocksDB checkpoint 发布器；
  预曝光评审修复（`ee325e7`）：auth 大小写、header 注入、400 映射、KeyCount、空桶删除。
- **SQL 修复**：auto-inc floor RMW 在 queue-apply 窗口内串行化（`99e1f4e`）；
  移除 CatalogTxn 死代码 record/applied API（`9546915`）。
- **CI/测试加固**：失败用例名注解（`fccbd67`）、fmt/clippy 债务清偿（`2a364d9`/`a593f47`）、
  soak e2e 客户端置备（`ceba48f`）、follower 落后轮询修复（`4c4a19a`/`133e2e0`）。

## 产物
CI tag release（linux-amd64）：`rdb-v0.3.0-x86_64-unknown-linux-gnu.tar.gz`。
