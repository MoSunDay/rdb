# WP1：位置化 posting（kind 0x1A v2 codec）

> 状态：landed（2026-11-10，Batch 1）
> 日期：2026-11-09

## 目标

posting 记录带上 token 位置（token 序号，0 起、字段内单调递增），使短语查询
（相邻位置链校验）成为可能；同时保持与旧库 v1 posting（kind 0x15）的读兼容。

## 设计

- 新 kind `KIND_SEARCH_POSTING2 = 0x1A`（`src/ds/codec.rs`）：
  - `SEARCH_FAMILY` 上界 0x19 → 0x1A（家族整删自动覆盖）；
  - `family_of` 增补；`classify` 的 typed 判定上限改为 `<= 0x1A`；
  - `codec_tests.rs` 的「0x1A 未分配」断言改为 Typed(0x1A)，未分配示例换 0x1B。
- v2 值布局（`index_codec/posting.rs`）：
  `count ++ entries[docid_len ++ docid ++ tf ++ npos ++ Δ位置LEB128]`，
  位置增量编码 `Δ-1`（严格递增），空位置 = v1 语义（未知）。
- 位置来源：`doc_record_of` 对 TEXT 字段按 tokenize 序枚举 token 序号，
  以 `(field, term, positions)` 侧表传入 `build_add_batch`（DocRecord 布局不变，
  旧 doc 记录可继续解码）；tf 与 positions.len() 恒等。
- 写路径：一律写 0x1A 键并顺手 `batch.delete(0x15 键)`（touch 即迁移）；
  entries 清空时两种键 + termstat 一起删。
- 读路径：`read_posting` 先读 0x1A、miss 回落 0x15（positions 为空）。
- KEYWORD 字段无位置（短语在 keyword 上降级为 AND，见 WP2）。

## 测试

v2 codec 往返（含空位置/多位置/坏输入 None）；前缀范围 key 解码；
search e2e 不回归。
