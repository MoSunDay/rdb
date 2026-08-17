# 数据结构 P3：JSON 全命令集（RedisJSON v1 legacy 路径）

## 背景
七类结构计划推进至 P3：在 `ds/` 基座上落地 JSON（kind 0x10），注册表 116 → 133。语义取 RedisJSON v1 legacy 确定路径子集（`$`/`.f`/`['f']`/`[i]`，无通配符/递归/过滤器），偏差显式记录于 `rust/COMPAT.md` 而非猜测对齐。

## 变更
### ds 层（`rust/rdb/src/ds/`）
- **`json_ds.rs`**：JSON 单记录存取——物理 key `data_key(KIND_JSON)`，value = LEB128 过期信封 + compact serde_json 整文档；每次变更为「读整文档 → 内存改 → 单记录单 fsync」；`write_doc` 维护过期索引（old→new），`delete_family` 单 kind 范围整键删除；读路径惰性过期清理。
- **`Cargo.toml`**：serde_json 开启 `preserve_order` 特性——对象键保持插入序，JSON.SET → JSON.GET 字节稳定（与 Redis 行为一致）。

### 路径与命令层（`rust/rdb/src/command/`）
- **`json_path.rs`**：legacy 路径文法解析（非法路径含 `$..`/`*`/`[*]` 一律拒绝）+ `serde_json::Value` 导航——`get_at`（负索引读不解析）、`set_at`（缺失中间字段自动建对象、数组 `index==len` 追加、越界/负值报不存在）、`remove_at`（对象删键/数组删位带位移）。
- **`json_cmd.rs`**：JSON.SET（NX/XX 条件不满足回 nil）、GET（多路径回扁平数组，偏差记录）、DEL/FORGET（根删=整键，路径删回 0/1）、TYPE（integer/number 分流）、MGET（多 key CROSSSLOT 校验，末参为路径）。
- **`json_str.rs`**：STRAPPEND（回新字节长）、STRLEN、NUMINCRBY（整数结果存 i64 序列化，浮点走最短往返表示）。
- **`json_arr.rs`**：ARRAPPEND/ARRPOP（负索引从尾数，越界报错——偏差记录）/ARRINDEX（stop 排他、-1 表到尾）/ARRINSERT/ARRLEN/ARRTRIM（stop 含端、-1 表末元素）。
- **`json_obj.rs`**：OBJKEYS（保序）/OBJLEN。
- **`mod.rs`**：+17 注册臂（json.set/get/del/forget/type/mget/strappend/strlen/numincrby/arrappend/arrpop/arrindex/arrinsert/arrlen/arrtrim/objkeys/objlen），不进路由白名单（首参即 key，正常 slot 路由）。

### 文档
- `rust/COMPAT.md`：Intentional deviations 增补 JSON 段（单记录存储、legacy-only 路径、多路径 GET 扁平数组、ARRPOP 越界报错、NUMINCRBY 数字格式化、MGET 遇异型键整单报 WRONGTYPE 等）。
- `agents/rust/index.md` / `features/kv-storage/index.md`：能力与模块索引同步（json 落地，仅剩 vector-set）。

## 测试覆盖
| 功能 | 测试名 | 文件 |
|------|--------|------|
| 记录 roundtrip/惰性过期/索引维护 | 3 项 | `rust/rdb/src/ds/json_ds.rs` |
| 路径文法接受与拒绝/导航/set 自动建 | 6 项 | `rust/rdb/src/command/json_path.rs` |
| SET 条件路径/GET 字节稳定/TYPE/MGET+CROSSSLOT | 7 项 | `rust/rdb/src/command/json_tests.rs` |
| ARR 族边界/OBJ 族/TTL 交互 | 6 项 | `rust/rdb/src/command/json_arr_tests.rs` |
| e2e 全命令生命周期（含 TTL 保持与清理） | 9 项 | `rust/rdb/tests/json_e2e.rs` |

- `cargo test --workspace` 实跑 **348 passed / 0 failed**；`cargo clippy --workspace --all-targets -- -D warnings` 零警告；`cargo fmt --check` 通过。
- 额外人工实测：拉起真实二进制走 TCP RESP——JSON.SET→GET 往返字节稳定（含键序保持）、嵌套路径导航、ARR 族、TTL 跨变更保持、根删后 GET 回 nil，22 项断言全部通过。
- 约束核验：`codec.rs`/`expire.rs`/`router.rs` 零改动（`git diff --stat` 仅新增文件 + `mod.rs`×2 + `Cargo.toml`/`Cargo.lock` + 文档）；新文件最大 378 行（≤400）。
