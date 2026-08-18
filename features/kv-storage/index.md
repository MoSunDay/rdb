Commit: d481b1d708c248f86be394189d01ca7305fc8528
# KV 数据读写与持久化

## 能力概述
- 类 Redis 的 KV 读写：`set/get/del/mset/mget`（Go 实现经 pebble、Rust 实现经 RocksDB 持久化，Sync 写）。
- 数据按 slot 分片：物理 key 带 `{slot}/` 前缀。
- 支持 Redis Cluster 客户端（自动处理 MOVED 重定向与 AUTH）。
- Rust 实现已支持复合类型：keys 命令族（TYPE/EXISTS/EXPIRE 族/SCAN/RENAME 等）、Hash 全命令（HSET/HGETALL/HSCAN/HRANDFIELD/HINCRBY…）、Set 全命令（SADD/SMEMBERS/SSCAN/SDIFF/SINTER/SUNION±STORE…）、List 全命令（LPUSH 族/LRANGE/LREM/LTRIM/LINSERT/LPOS/LMOVE…）、ZSet 全命令（ZADD 族/ZRANGE±BYSCORE/BYLEX/ZRANK/ZPOPMIN/ZSCAN/Z*STORE…）、JSON 全命令（JSON.SET NX/XX、JSON.GET、DEL/FORGET、TYPE、MGET、STRAPPEND/STRLEN/NUMINCRBY、ARR 族×6、OBJKEYS/OBJLEN，RedisJSON v1 legacy 确定路径）、VectorSet 全命令（VADD FP16/VALUES、VREM、VCARD、VDIM、VSETATTR/VGETATTR、VSIM cosine 召回）；阻塞命令族 BLPOP/BRPOP/BLMOVE/BRPOPLPUSH/BZPOPMIN/BZPOPMAX（多 key FIFO 唤醒、超时返回 nil）。
- Rust 实现支持全类型统一 TTL：读路径惰性判定 + 后台主动采样清理。
- 流数据（stream 类）：Rust 侧由 Lite Mode 定形承接——RocketMQ 风格父主题 + 动态队列，复用 Streams 命令动词（XADD/XREADGROUP/XACK/XINFO 等），非 Redis Streams 全仿真；七类结构（string/hash/list/set/zset/stream/json/vector-set 中除 string 外的六类复合结构 + stream）已 7/7 齐备。

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
- 限制：Go 实现无 TTL/过期与复合类型；Rust 实现七类结构已全部落地——stream 定形为 Lite Mode 流命令族（RocketMQ 语义主题/队列，`KIND_STREAM_PEND 0x0F` 仅作将来完整 PEL 预留），json/vector-set 为语义子集（legacy 确定路径 / 暴力 cosine，偏差见 `rust/COMPAT.md`）。

## 关联逻辑模块
- [store](../../agents/store/index.md)
- [command](../../agents/command/index.md)
- [server](../../agents/server/index.md)
