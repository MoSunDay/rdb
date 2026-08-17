Commit: d481b1d708c248f86be394189d01ca7305fc8528
# KV 数据读写与持久化

## 能力概述
- 类 Redis 的 KV 读写：`set/get/del/mset/mget`（Go 实现经 pebble、Rust 实现经 RocksDB 持久化，Sync 写）。
- 数据按 slot 分片：物理 key 带 `{slot}/` 前缀。
- 支持 Redis Cluster 客户端（自动处理 MOVED 重定向与 AUTH）。
- Rust 实现已支持复合类型：keys 命令族（TYPE/EXISTS/EXPIRE 族/SCAN/RENAME 等）、Hash 全命令（HSET/HGETALL/HSCAN/HRANDFIELD/HINCRBY…）、Set 全命令（SADD/SMEMBERS/SSCAN/SDIFF/SINTER/SUNION±STORE…）、List 全命令（LPUSH 族/LRANGE/LREM/LTRIM/LINSERT/LPOS/LMOVE…）、ZSet 全命令（ZADD 族/ZRANGE±BYSCORE/BYLEX/ZRANK/ZPOPMIN/ZSCAN/Z*STORE…）；阻塞命令族 BLPOP/BRPOP/BLMOVE/BRPOPLPUSH/BZPOPMIN/BZPOPMAX（多 key FIFO 唤醒、超时返回 nil）。
- Rust 实现支持全类型统一 TTL：读路径惰性判定 + 后台主动采样清理。

## 触发方式
- Redis 客户端直连任意节点；README 中 `redis-benchmark -c 500` 场景验证：SET ~53k ops/s、GET ~54k ops/s，p50 < 6ms。

## 行为与规则
- 认证：redcon 服务密码 = 配置 `raft_token`，客户端需 AUTH。
- hash tag：`{tag}` 内的 key 片段归同一 slot，支持多 key 同节点。
- 错误语义：get 缺失返回 null；del 成功返回 1、失败返回 0；set/mset 成功返回 OK；参数错误返回 ERR。
- 存储：每层 bloom filter（10 bits/key）；批量写走 IndexedBatch 一次提交。

## 关键状态与异常
- 状态：数据面无独立状态；路由状态（ClusterReady/StableAddrs）来自控制面。
- 异常：处理器 panic 由 server `recover` 兜底（`fatal error: ...`）。
- 限制：Go 实现无 TTL/过期与复合类型；Rust 实现复合类型分阶段推进中（stream/json/vector-set 待后续阶段）。

## 关联逻辑模块
- [store](../../agents/store/index.md)
- [command](../../agents/command/index.md)
- [server](../../agents/server/index.md)
