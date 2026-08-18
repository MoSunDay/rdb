Commit: d481b1d708c248f86be394189d01ca7305fc8528
# store

> **已归档**：Go 实现已移至 `archive/go/internal/store`（git mv 保留历史，不再维护）。当前实现见 [agents/rust/index.md](../rust/index.md)。以下内容描述归档前的 Go 实现。


## 职责
- 基于 cockroachdb/pebble 的持久化 KV 封装，提供带前缀的读写接口。

## 边界
- 负责：数据落盘（Set/MSet/Get/MGet/Del/Iter）、批量写（IndexedBatch）、BloomFilter 配置。
- 不负责：slot 计算与 key 构造（server 传入 PrefixKey）、复制与迁移逻辑。

## 关键设计
- 物理 key = `prefix + key`；业务前缀为 `"{slot}/"`（如 `5465/`），由 server 计算后传入。
- 写操作（Set/MSet/Del）均以 `pebble.Sync` 提交。
- `OpenPebble`：`EnsureDefaults` + 每层 `bloom.FilterPolicy(10)`。
- `Size()` 返回 `NewIndexedBatch().Len()`，即键数量而非字节数。
- `Iter` 以 LowerBound/UpperBound 做前缀范围迭代。
- `store/db.go` 的 `DB` 接口：`MGet` 声明返回 `[]string`，与 `Pebble.MGet`（`[][]byte`）签名不一致，接口当前未被引用（疑似遗留）。

## 核心链路
1. command 处理器 → `db.Set/Get/...` → pebble 同步写读

## 依赖与接口
- 依赖：utils（BytesCombine）、pebble。
- 对外：`*store.Pebble` 注入 `CommandContext`。

## 关联模块
- [command](../command/index.md)
- [server](../server/index.md)
