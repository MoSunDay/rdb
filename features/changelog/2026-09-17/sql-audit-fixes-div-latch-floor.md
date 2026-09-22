# 静态复审三缺陷修复：除法判号 / 复合 pk 锁键 / 2PC 段尾 floor

Commit: 本批次（fc5de4c 之后）

## 背景

W2 收官上线审计的静态复审在既有全绿回归（935 lib + 54 套件 + 4 场景 + soak）之外
发现三处窄边角缺陷，据此给出"有条件放行"（0.2.2 前须修 latch 与除法判号）。本批
兑现该前置条件，并补齐两个盲区交叉面（复合 pk × 锁、混合 2PC × columnar）的测试。

## 变更

1. **DECIMAL 除法判号**（`src/sql/exec/expr_decimal.rs`）：half-away-from-zero 的
   bump 方向由截断商 `q.is_negative()` 改为精确商符号 `(d<0)!=(mb<0)`——截断商
   恰为 0 时不携带符号，`-0.00001/15000` 曾得 `+0.000000001`。与
   `rescale_decimal` 的被除数判号（除数恒正 pow10）语义对齐。新增单测：q==0 的
   四种符号组合 + 精确平局（±5/20000 → ±0.0003，half away from zero）。
2. **复合 pk 锁键错位**（`src/sql/tx/latch.rs` `lock_matched`）：原按 pk 声明序
   收集短向量直接传 `pk_encode_row`，而后者按 schema 列位索引 `values`——pk
   非前导列时越界 panic（报错兜底），前导但乱序时静默编码错键（锁失效）。现按
   schema 列位散射为全宽向量再编码（非 pk 槽 Null 占位，永不被读取）。新增 e2e
   （`tests/txn_semantics_e2e.rs`）：`PRIMARY KEY(c,a)` 非前导 + 乱序，B 会话对
   同行 FOR UPDATE 撞 1205（证明两侧编码出同一物理键）、异行成功、A COMMIT 后
   重试成功；修复前该测试实证为红。
3. **2PC participant DECIDE 的 ts floor 段尾缺口**（`src/sql/dist/participant.rs`）：
   `hi` 原仅由行版本键与 marker.commit_ts 提升，columnar 段 meta 不计入——混合
   切片（远端行 + 本地段）的段 commit_ts 可高于 marker.commit_ts，kill -9 后持久
   floor 低于存活段 ts，段短暂不可见直至下次写自愈（违反 `tx/floor.rs` 自述
   不变量）。段 meta 解码成功后 `hi = hi.max(meta.commit_ts)`，floor 戳与返回给
   协调者的读点同源修复。新增单测：段 ts 100 / marker 90 → 返回 hi=100、
   `floor::recover` ≥ 100、段翻 Live；段 50 / marker 90 → hi 不回退。
4. **COMPAT.md 补记**：`Int / Int` `/` 为整除（MySQL 返回小数商）的已知偏差，
   原先仅记于 `features/sql-dataplane.md` 限制清单。

## 未随批（复审次级项，转后续）

- `expr_decimal` 大被除数静默回退 f64 但结果元数据仍 NEWDECIMAL（客户端碰 1292）
- `expr.rs` Decimal `Neg` 用 `wrapping_neg`（i128::MIN 时符号静默错）
- `ci.yml` release 步骤幂等化（`cancel-in-progress` 砍同 ref 重推的 release，为
  v0.2.0 资产缺失候选机制，流程侧改进）
