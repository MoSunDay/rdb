# S3 前置评审修复：鉴权大小写 / 响应头注入 / 400 映射 / KeyCount / 空目录删桶

Commit: HEAD（`8823f40` 评审后、对外暴露前合入）

## 背景
`8823f40` 评审结论：默认关闭（`s3_bind` 空）可合入可部署，核心 RESP/raft 路径
零扰动；但作为 S3 服务**对外暴露前**有 2 个必修安全缺陷 + 3 个兼容缺陷 + 3 处
文档超前于实现（详见 `features/s3-object-storage.md` 偏差清单）。

## 修复
1. **鉴权大小写（安全必修）**：请求侧 lower-case 而配置 token 原样拼接，含大写
   字母的 `s3_token` 永久 401。抽取纯函数 `authorized`，双侧小写（同
   `es/http.rs` 姿态）。
2. **响应头注入（安全必修）**：PUT 的 Content-Type 原样入库、GET/HEAD 原样回写，
   含裸 CR/LF 的值可拆分响应头。双层防护：入库前 `valid_header_value` 拒控制
   字节（400 `InvalidArgument`）+ 回写时 `sanitize_header_value` 清洗。
3. **非法 bucket 名 500→400**：PUT/DELETE 走文件系统层报错映射成 500；现在
   bucket 路由入口统一 `400 InvalidBucketName`（GET/HEAD 同）。
4. **ListObjectsV2 `<KeyCount>` + v1 `<NextMarker>`**：严格客户端（部分
   awscli/boto3）需要 KeyCount；v1（无 `list-type=2`）截断页改回 NextMarker
   （v2 仍为 NextContinuationToken）。
5. **空目录删桶 500→204**：嵌套 key 删除后空目录不回收致桶 ENOTEMPTY 永久
   不可删。`delete` 剪枝空父目录 + `delete_bucket` 自查空后整树清扫（含崩溃
   残留 `.staging`/孤儿旁车），竞态路径映射 409。
6. **PUT 缺 Content-Length**：原静默存空对象，现 `411 MissingContentLength`
   （文档既有承诺）。
7. **文档对齐**：旁车内容（ETag+Content-Type，大小/mtime 取 stat）、411、
   NextMarker 三处描述与实现对齐；token 双侧小写写入安全姿态。
8. **测试基建**：e2e fixture 重试循环在 `Node::drop` 清目录后未重建（潜伏
   ENOENT flake），`create_dir_all` 移入 `spawn_once` 每次重建。

## 测试覆盖
- `src/s3` 单测 12→17：http.rs 0→3（鉴权大小写、控制字节拒绝、Content-Length
  解析）、xml v1 NextMarker、object 空目录剪枝/残留清扫/非空拒绝。
- `tests/s3_e2e` 7→11：混合大小写 token 认证、非法桶名四方法 400、嵌套 key
  删空桶 204、KeyCount/NextMarker 翻页/LF 注入拒绝/无 Content-Length 411。
- 实测：`cargo test --lib` 1071 全绿、`--test s3_e2e` 11 全绿（两轮稳定）。
