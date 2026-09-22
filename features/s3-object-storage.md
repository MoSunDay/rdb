# S3 对象存储前置：本地文件系统的 S3 兼容面 + checkpoint 发布

## 定位
- **协议前置适配层**，与 `src/es/` 之于检索内核、`src/rocksmq/` 之于 Lite 同构：
  `src/s3/` 只做 S3 协议（REST 语义 + XML 回包）编解码，后端是**本地文件系统**——
  一个对象 = 一个真实文件 + `.s3meta.json` 旁车元数据，无独立存储引擎、无打包
  格式。
- 手写 HTTP（`rcache/http.rs` / `es/` / `rocksmq/` 一派，零新增 crate）：每连接
  一请求（`Connection: close`，无 keep-alive）、仅 Content-Length body
  （`Transfer-Encoding: chunked` → 501）、body 上限 1 GiB（413）、
  `Expect: 100-continue` 先应答再读 body。
- 接线：`s3_bind`（空 = 禁用，同 `es_bind` 姿态；backup 监听器不含）；
  `s3_token` 非空时要求 `Authorization: Bearer <token>`（同 `es_token`，
  **非 SigV4**）。
- 第二职责：RocksDB 周期 checkpoint 发布为对象集（StarRocks tablet→对象存储
  布局），给备份实例提供可拉取的恢复源（见「与备份体系的关系」）。

## 对象物理布局
- 根目录 = `s3_store_path`（空回退 `<store_path>/s3`）；bucket = 根下一级真实
  目录（`s3_bucket` 空 = `rdb`）；对象 key = bucket 目录下的相对文件路径。
- 对象 = 真实文件 `<bucket>/<key>` + 旁车 `<bucket>/<key>.s3meta.json`（大小、
  修改时间、ETag 等）。原子写：同目录 tmp + fsync + rename，rename 落地后才对
  协议面可见（对象与旁车各一段）。
- ListObjects 遍历真实目录树（`.s3meta.json` 旁车不出现在结果里），字典序。

## StarRocks 布局映射表
checkpoint 发布布局类比 StarRocks tablet→对象存储（rowset 文件集 + 末置
rowset meta）：

| StarRocks 概念 | rdb S3 checkpoint 布局 |
|---|---|
| tablet 的对象存储路径 | `<bucket>/rocksdb/<node-bind>/`（`node-bind` = 节点 `bind` 地址串） |
| rowset 版本目录 | `ckpt_<unix_ms>/`（checkpoint 时刻毫秒） |
| rowset 数据文件集 | RocksDB checkpoint 产出的 CURRENT/MANIFEST/SST 等真实文件 |
| rowset meta（最后落盘） | `meta.json`（文件清单 + 产出时刻，**整集末置写入**） |
| 版本保留 | 最近 `s3_checkpoint_retention` 份（0 = 内建默认 2），旧 `ckpt_*` 整目录删 |

`meta.json` 末置即 rowset meta 语义：拉取方以它的存在性判定一个 `ckpt_*` 集是否
发布完整（缺失 = 未完成/残留，跳过或清理）。发布走节点内部文件路径，不经
HTTP PUT（不受 1 GiB body 上限约束）。

## API 一览与语义
| 操作 | 请求 | 成功 | 语义 / 错误 |
|---|---|---|---|
| ListAllMyBuckets | `GET /` | 200 `ListAllMyBucketsResult` | 列根一级目录为 bucket |
| CreateBucket | `PUT /{bucket}` | 200 | 已存在 409 `BucketAlreadyOwnedByYou`；非法名 400 |
| HeadBucket | `HEAD /{bucket}` | 200 / 404 | 无 body |
| DeleteBucket | `DELETE /{bucket}` | 204 | 非空 409 `BucketNotEmpty` |
| ListObjectsV2 | `GET /{bucket}?list-type=2...` | 200 `ListBucketResult` | 参数见下；v1（无 `list-type`）走 `marker` 同语义 |
| PutObject | `PUT /{bucket}/{key...}` | 200（带 `ETag` 头） | Content-Length 必填（chunked 501）；>1 GiB 413；key 以 `/` 结尾 400 `InvalidArgument` |
| GetObject | `GET /{bucket}/{key...}` | 200 / 206 | `Range: bytes=a-b` 单区间 → 206 + `Content-Range`；不可满足 416 `InvalidRange` |
| HeadObject | `HEAD /{bucket}/{key...}` | 200（`Content-Length`/`ETag`）/ 404 | 无 body |
| DeleteObject | `DELETE /{bucket}/{key...}` | **恒 204** | 幂等：不存在也 204，无 404 分支 |

ListObjectsV2 参数（v1 `marker` 与 v2 `continuation-token` 等价，翻页由
`IsTruncated` + `NextMarker`/`NextContinuationToken` 驱动）：
- `prefix`：key 前缀过滤；
- `delimiter`：公共前缀聚合为 `CommonPrefixes`（目录视图），可与 prefix 组合；
- `max-keys`：默认 1000，>1000 钳到 1000；
- `encoding-type=url`：key / 公共前缀值做 URL 编码回传（`<node-bind>` 含 `:`，
  外部消费建议始终带上）；
- versioning 参数（`versionId`/`key-marker` 等）不在子集。

Range：仅接受单一 `bytes=first[-last]` 区间（last 饱和到文件尾）；多区间/非
bytes 单位不在子集（不回多段 206）；起点越出文件尾 → 416。

key 校验：不允许以 `/` 结尾（目录 marker → 400 `InvalidArgument`）；含 `..`
路径段拒绝（防逃逸出 bucket 目录）；空 key 400。

## 偏差清单（vs AWS S3）
- **无 SigV4**：鉴权 = `Authorization: Bearer <s3_token>`（同 `es_token` 姿态）；
  原生签名客户端（aws-cli / SDK 默认路径）不能直连——用 curl/自定客户端，或经
  反向代理注入 Bearer 头。
- **无 multipart upload**：`POST ?uploads` / `UploadPart` /
  `CompleteMultipartUpload` 不在子集；>1 GiB 的对象无法经协议面写入（413）。
- **无版本化**：无 version id、无 delete marker；DELETE 即删文件 + 旁车。
- 目录 marker 不存在（key 禁 `/` 结尾）；目录随对象路径隐式出现/随删除隐式消失。
- DeleteObject 恒 204（真 S3 对不存在对象亦是，此处写死为契约）。
- 传输面：HTTP/1.1 每连接一请求 `Connection: close`、chunked 501、body 1 GiB
  上限、100-continue——与 ES 前置同款约束。
- 无 ACL/policy/cors/lifecycle/website/tagging/CopyObject 等子资源面。

## 配置键（`src/conf.rs`，全部缺省零值）
| 键 | 缺省 | 语义 |
|---|---|---|
| `s3_bind` | `""` | S3 HTTP 监听地址；空 = 禁用（backup 监听器不含） |
| `s3_store_path` | `""` | 对象根目录；空回退 `<store_path>/s3` |
| `s3_bucket` | `""` | 发布/默认 bucket 名；空 = `rdb` |
| `s3_token` | `""` | Bearer token；空 = 无鉴权 |
| `s3_checkpoint_interval_ms` | `0` | checkpoint 发布周期 ms；0 = 不发布 |
| `s3_checkpoint_retention` | `0` | 保留最近 N 份 checkpoint；0 = 内建默认 2 |

## 安全姿态
- token 同 `es_token`：非空即全端点强制 Bearer；无签名/时间戳 ⇒ 无防重放，
  token 泄露等同读写权限，轮换 = 改 yaml 重启。
- 对象面**可写**（PUT/DELETE 直接落本地文件系统）：不可信网络下必须绑定回环/
  受控内网（同 Kafka/RocksMQ 前置姿态）；`s3_store_path` 回退落在 `store_path`
  之下时随数据盘同权限管理。
- key 映射为 bucket 内相对路径，`..` 段拒绝（见上），杜绝路径逃逸。

## 与备份体系的关系
- 现状（raft-ha 文档既有结论）：备份实例（`backup_bind`）数据依赖外部同步；
  S3 前置补上「源」——主节点周期发布 RocksDB checkpoint 为对象集。
- 恢复：任意 S3 client 从端点把 `rocksdb/<node-bind>/ckpt_<ms>/` 文件集拉到
  本地目录；**目录内全是真实文件**（无打包/无私有格式），拉完即一份自洽的
  RocksDB 快照目录，把 `store_path` 指向它启动即完成恢复。rsync/tar 直接拷
  目录同样成立——S3 面只是访问协议，不是数据格式。
- 完整性：以 `meta.json` 存在性判定集完整；多节点各占 `<node-bind>/` 一支，
  互不覆盖；保留 N 份 ⇒ 拉取窗口有限，备份侧按 interval 数倍周期轮询。
