# Release v0.2.0 / v0.2.1 — commit mapping

## 背景
v0.2.0 tag 在 rebase 完成前被推送到远端（首次 push 分支被拒、tag 先行到达），
指向 rebase 前的孤儿提交 `f4ee01d`。其 CI release 产物与 tag 一致（可复现），
但不在 main 谱系上：不含 `8065e6e`（raft write guard fix）与后续 CI 调整。

## 映射
- **v0.2.0** = `f4ee01d`（rebase 前）。本目录既有文档中的提交哈希
  （`81ff8d9`/`2c935db`/`5b67c47`/`8372d90`/`63192d8`/`45b8dc2`）
  均为该谱系，**无需改写**。该 tag 的 CI release 条目已创建但
  **tarball 资产缺失**（asset 404，疑似上传失败），无可用产物。
- **v0.2.1** = rebase 后谱系（`7cc90b7` bump 0.2.0 → `933ba4d` ci 调整 → 版本 bump 0.2.1），
  内容 = v0.2.0 + `8065e6e` raft write guard fix + CI 失败用例注解/测试限时。
  等价 rebase 哈希：`7cc90b7`(版本) ← `11cc813`(changelog) ← `a75df96`(W2 docs) ← `763a801`(升级演练)。

## 建议
生产部署使用 **v0.2.1**（release 资产已验证可下载，14.5MB，kill -9 耐久冒烟通过）。
v0.2.0 tag 仅为 rebase 竞态的历史记录：如需产物可删除该 tag/release 后在
正确提交上重打，或直接忽略。
