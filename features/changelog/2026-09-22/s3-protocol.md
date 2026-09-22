# S3 兼容对象存储前置：`src/s3/` + RocksDB checkpoint 发布（接口冻结）

Commit: HEAD（规范/文档先行落地；`src/s3/` 实现与本条同批合入）

## 内容
- 规范：[features/s3-object-storage.md](../../s3-object-storage.md)——本地文件
  系统后端的 S3 协议子集（bucket CRUD、ListObjectsV2/v1 marker、object
  GET/HEAD/PUT/DELETE、单区间 Range 206/416、100-continue、chunked 501、
  body 1 GiB、每连接一请求），对象 = 真实文件 + `.s3meta.json` 旁车
  （tmp+fsync+rename 原子写）。
- checkpoint 发布：`<bucket>/rocksdb/<node-bind>/ckpt_<unix_ms>/`（文件集 +
  末置 `meta.json`，类比 StarRocks rowset meta），保留最近
  `s3_checkpoint_retention` 份（0 = 默认 2）。
- 配置键（`src/conf.rs`，缺省零值）：`s3_bind`（空 = 禁用）、`s3_store_path`
  （空回退 `<store_path>/s3`）、`s3_bucket`（空 = rdb）、`s3_token`（Bearer，
  非 SigV4）、`s3_checkpoint_interval_ms`（0 = 不发布）、`s3_checkpoint_retention`。
- 文档接线：`features/index.md`、`COMPAT.md`（"S3-compatible object-storage
  frontend (Rust-only)" 节）、`config/conf.yaml` 注释化样例、`agents/rust/index.md`
  模块行。

## 偏差要点（vs AWS S3）
无 SigV4（Bearer token）、无 multipart、无版本化、key 禁 `/` 结尾（目录 marker
→ `InvalidArgument`）、DELETE object 恒 204；Go 归档实现无此功能（Rust-only）。
