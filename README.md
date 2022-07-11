### 支持 Redis Cluster 协议的可持久化存储

#### TodoList
1. - [x] 单节点支持 `LevelDB`
2. - [x] `set/mset/get/mget` 常用命令支持
3. - [x] 支持 `redis cluster` 通过 `redis-benchmark` && `redis-py-cluster` 的测试
4. - [] 加入配置管理
5. - [] 支持其他例如 `pebble`
6. - [] `cluster` 数据交给 `Raft` 管理
7. - [] 支持数据迁移
8. - [] 加入其他数据结构
