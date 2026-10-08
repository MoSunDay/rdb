# H1 协议与会话正确性

> 状态：landed（2026-10-08，摘要 `features/changelog/2026-10-08/mysql-h1-protocol-session.md`）
> 依赖：无（H0 后半并行完成）

## 工作包与落点

0. 【已解决，记录备查】评审问五升级的 P0：列式表 RENAME/TRUNCATE 被 GC 静默清库
   （meta 键内表名在 RENAME 后不更新，按缺席名归类成 garbage）+ 非 leader 节点
   TRUNCATE 旧 id segment 泄漏（只有 leader 急切删 segment）——已由 2026-10-07 的
   H0 收口补丁解决：`src/sql/columnar/gc.rs` 改按 meta 键内 table_id 分类
   （live 优先保留 / dropped 回收 / unknown 保留），详见
   `01-h0-silent-wrong-results.md` 的收口记录；此项勿重开
1. 二进制协议编码器列元数据感知 —— **前置于本包提交**（`b9bf6c6`，
   `src/sql/front/conv_bin.rs`：值先 coerce 到公告列型再编码 + preflight 预检，
   funcs_control e2e 规避断言已翻转）
2. USE 校验 + CONNECTION_ID：未知库报 1049（`front/shim.rs::is_known_db`，
   接受默认 `rdb`，大小写不敏感；prepared-USE 一律 1235）；连接 id 为
   epoch_secs+原子序号（跳 0 槽，重启 ≥1s 不碰撞，无持久化）
3. NULL-first/非有限值修整：ROUND/TRUNCATE(x,NULL)、LOCATE(a,b,NULL) 先判 NULL
   （`func/numeric.rs`、`func/string_more.rs`）；POW 指数出 [-30,30] 或非有限
   → NULL（`func/numeric_more.rs`）
4. TRIM 无 remstr 形态默认 `' '`（`parse/trim_default.rs` 预解析注入，
   sqlparser 0.62 本身要求 remstr）
5. REPEAT/LPAD/RPAD 结果字节数精确预检（≤1<<24 允许，超出 → NULL，判定先于
   分配，`func/string.rs`/`func/string_more.rs`）；like_match 递归改迭代 DP
   （`exec/expr_like.rs`，54,684 对 exhaustive 等价验证）
6. 日期宽松读：1–2 位月日、>6 位小数秒截断（`temporal.rs`）
7. UPDATE/DELETE 的 ORDER BY ?/LIMIT ? 按 SET→WHERE→ORDER BY→LIMIT 文本序绑定
   （`parse/translate_dml.rs`，AST `LimitValue`）；GROUP BY 引用别名（列名优先，
   `exec/select.rs::group_keys_resolving_aliases`）；聚合 arity 1235→1582
   （`parse/func_forms.rs`，仅 arity，unsupported 形态保持 1235）

## 验收

单测 + e2e 每项至少一条（已满足）；COMPAT.sql.md 台账已同步（USE 1049 / conn id /
POW / REPEAT 上限 / TRIM / 宽松日期 / GROUP BY 别名 / DML 占位符七处）；
31 个 SQL e2e 套件两轮全绿（期间 `string_negative_matrix` 在 18 套件并发下出现
一次未复现失败，归因为 H4 已登记的负载 flake 类）；FROM_UNIXTIME(NULL) 复核
本就正确（不在改动面）。
