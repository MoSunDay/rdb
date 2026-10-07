# H1 协议与会话正确性

> 状态：proposed
> 依赖：无（可与 H0 后半并行）

## 工作包

0. 【已解决，记录备查】评审问五升级的 P0：列式表 RENAME/TRUNCATE 被 GC 静默清库
   （meta 键内表名在 RENAME 后不更新，按缺席名归类成 garbage）+ 非 leader 节点
   TRUNCATE 旧 id segment 泄漏（只有 leader 急切删 segment）——已由 2026-10-07 的
   H0 收口补丁解决：`src/sql/columnar/gc.rs` 改按 meta 键内 table_id 分类
   （live 优先保留 / dropped 回收 / unknown 保留），详见
   `01-h0-silent-wrong-results.md` 的收口记录；此项勿重开
1. 二进制协议编码器列元数据感知（值先 coerce 到 ColMeta 再编码），消灭数值绑文本列断连；翻转 funcs_control e2e 规避断言
2. USE 校验（不存在的库报 1049）+ db 注册面；CONNECTION_ID 持久单调（重启不碰撞）
3. NULL-first 修整：ROUND/TRUNCATE(x,NULL)、LOCATE(a,b,NULL)、FROM_UNIXTIME(NULL) 先判 NULL 再 int_arg；POW/大档位非有限值 → NULL（clamp ±30）
4. TRIM(LEADING FROM x) 默认 remstr=' '（合法 MySQL 形态）
5. REPEAT/LPAD/RPAD 结果长度上限 → NULL/响亮错（DoS 面）；like_match 递归改迭代 DP
6. 日期宽松读：1–2 位月日、>6 位小数秒截断（DATE('2020-1-1') 不再 NULL）
7. UPDATE/DELETE 的 ORDER BY ?/LIMIT ? 计数绑定；GROUP BY 引用别名；聚合 arity 1235→1582
## 验收

单测 + e2e 每项至少一条；收尾同步 COMPAT.sql.md / changelog / e2e-coverage；
全量 workspace 复跑无回归。
