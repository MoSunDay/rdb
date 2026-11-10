# WP6：互操作矩阵 + 台账（Batch 2）

> 状态：landed（2026-11-10，Batch 2）
> 日期：2026-11-09

Batch 1 已覆盖部分矩阵（见 05 文档测试节）。Batch 2 收尾项：

- `tests/es_search_e2e.rs` 扩矩阵：`FT.ADD ↔ ES match_phrase/prefix`、
  `ES _bulk ↔ FT.SEARCH 短语/前缀`、RESP/ES 混写同索引一致性（含 TTL 家族整删）。
- 回写 `COMPAT.md`（FT.* 节 + ES 前端节）、`features/es-front.md`；
  落地摘要 `features/changelog/2026-11-09/`；`plans/index.md` 登记。
