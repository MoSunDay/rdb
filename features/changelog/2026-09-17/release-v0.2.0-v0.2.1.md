# Release v0.2.0 / v0.2.1 — commit mapping

## 背景
v0.2.0 tag 在 rebase 完成前被推送到远端（首次 push 分支被拒、tag 先行到达），
指向 rebase 前的孤儿提交 `f4ee01d`。其 CI release 产物与 tag 一致（可复现），
但不在 main 谱系上：不含 `8065e6e`（raft write guard fix）与后续 CI 调整。

## 映射
- **v0.2.0** = `f4ee01d`（rebase 前）。本目录既有文档中的提交哈希
  （`81ff8d9`/`2c935db`/`5b67c47`/`8372d90`/`63192d8`/`45b8dc2`）
  均为该谱系，与 v0.2.0 产物对应，**无需改写**。
- **v0.2.1** = rebase 后谱系（`7cc90b7` bump 0.2.0 → `933ba4d` ci 调整 → 版本 bump 0.2.1），
  内容 = v0.2.0 + `8065e6e` raft write guard fix + CI 失败用例注解/测试限时。
  等价 rebase 哈希：`7cc90b7`(版本) ← `11cc813`(changelog) ← `a75df96`(W2 docs) ← `763a801`(升级演练)。

## 建议
生产部署使用 **v0.2.1**；v0.2.0 仅作 W2 批次首次构建存档（本地全量验证树：
935 lib tests + 4/4 场景 + soak 900s + bench A/B 均在该树完成）。
