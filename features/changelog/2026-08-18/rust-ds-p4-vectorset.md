# 数据结构 P4：VectorSet 全命令集（七类结构 7/7 收官）

## 背景
七类结构计划最后一阶段：落地 VectorSet（meta=kind 0x11 / elem=kind 0x12，编码在 P0 已预留），注册表 133 → 140；`features/kv-storage` 的「stream/json/vector-set 待后续阶段」限制句清零，七类 7/7。

## 变更
### ds 层（`rust/rdb/src/ds/`）
- **`vectorset_ds.rs`**（400 行，测试内联）：meta = envelope + LEB128(dim) + LEB128(count)；elem = `dim × 8B LE f64` + LEB128 长度前缀属性（0 = 无属性）；`elems_range` 按 kind 有界扫描单键元素；手写 `fp16_to_f64`（含 subnormal/inf/NaN，无新依赖）；`write_meta` 维护过期索引、`delete_family` 整键族删除。

### 命令层（`rust/rdb/src/command/`）
- **`vectorset_cmd.rs`**：VADD（`FP16 <blob>`/`VALUES <文本>` 两种形态；dim 1..=4096；重加元素仅换向量、保留属性与 TTL，回 `:0`，新元素回 `:1`）、VREM（清空即整键删除）、VCARD、VDIM（缺键报 `ERR vector set does not exist`）。
- **`vectorset_attr.rs`**：VSETATTR（元素缺失回 `:0`，空串清属性）、VGETATTR（单属性模型，无属性回 nil）。
- **`vectorset_sim.rs`**：VSIM——暴力 O(n·dim) cosine 扫描（无 HNSW/EF/FILTER，偏差记录），score=(cos+1)/2 零向量记 0.5，最短往返 f64 格式化，同分按元素字节序；COUNT/WITHSCORES/WITHATTRIBS 任意顺序解析（双开时 element,attr,score 三元组）。
- **`mod.rs`**：+7 注册臂（vadd/vrem/vcard/vdim/vsetattr/vgetattr/vsim），不进路由白名单。

### 文档
- `rust/COMPAT.md`：Intentional deviations 增补 VectorSet 段（暴力扫描、原始 f64 存储、score 格式化、缺键报错 vs Redis nil、单属性模型、VADD 保属性、tie-break 规则）。
- `agents/rust/index.md` / `features/kv-storage/index.md`：七类 7/7 收官同步。

## 测试覆盖
| 功能 | 测试名 | 文件 |
|------|--------|------|
| meta/elem roundtrip、FP16 换算、范围互斥、族删除 | 6 项 | `rust/rdb/src/ds/vectorset_ds.rs` |
| VADD 两形态/重加语义/错误路径、VREM 清空、ATTR 增改查、VSIM 排序与格式、TTL 交互 | 9 项 | `rust/rdb/src/command/vectorset_tests.rs` |
| e2e：VSIM 召回正确性（含精确命中 score=1）、COUNT/WITHSCORES/WITHATTRIBS、FP16、VREM、TTL | 8 项 | `rust/rdb/tests/vectorset_e2e.rs` |

- `cargo test --workspace` 实跑 **371 passed / 0 failed**；`cargo clippy --workspace --all-targets -- -D warnings` 零警告；`cargo fmt --check` 通过。
- 额外人工实测：真实二进制 TCP RESP 演练 30 项断言全过——VSIM 召回排序与 score 精确值（1 / 0.8535533905932737 / 0.5）、FP16 双向、属性跨重加保持、TTL 跨 VADD 保持、清空删键、缺键报错。
- 约束核验：`codec.rs`/`expire.rs`/`router.rs`/`Cargo.toml` 零改动；新文件最大 400 行（≤400）。
