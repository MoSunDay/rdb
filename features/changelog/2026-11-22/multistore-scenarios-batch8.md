# 多存储场景批次 8：#81–#90 十例 e2e（2026-11-22）

承接批次 1–7（2026-11-19/20/21），批次 8 再扩 10 例，编号 #81–#90，只读复用
`tests/scenario_common/` 夹具、不改 `src/`。覆盖此前 scenario 层未触达的命令面：
XPICK 三策略全量（scenario 首次）、XGROUP SETID 回拨重放 + DELCONSUMER 清 PEL +
XINFO CONSUMERS 花名册、SET 旗标矩阵（GET/NX/XX/KEEPTTL/互斥门）、EXPIRE
NX/XX/GT/LT 四围栏、ZADD GT/LT/CH/INCR/NX/XX 全旗标、SQL CASE WHEN 分档 +
GROUP BY/HAVING + LIMIT OFFSET + 多列 ORDER BY、GROUP_CONCAT（SEPARATOR/
DISTINCT/多参）+ CONCAT_WS/CONCAT NULL 语义 + REGEXP、ZREVRANGEBYLEX 倒序窗 +
ZREMRANGEBYLEX 前缀裁剪（与 #54 的中窗退役不同调用形）、ES bool must_not 排除
语义、以及**首个启用 `sql_rpc` 前端**（`FrontSpec{sql_rpc:true}`）的 scenario——
三节点跨槽 2PC 结算。`tests/` 净增 10 个文件、全部 ≤150 行（上限 400）。
回归：200 个 e2e 目标 + lib 1343 + bins 4 全绿，clippy `-D warnings` 零告警
（kafka 四套 + checkout_watch + sqlrpc_twopc 在 4 线程重载下抖动，单线程复跑全绿，
属环境负载非回归；sqlrpc_twopc 节点 spawn 偶超 30s 就绪窗同理）。

## 撞车消解（相对批次 8 初稿名单的两处替换）

- 初稿 #89 为「裸 SUNION 合并版图 + SMISMEMBER」，但 #73 `social_union_feed`
  已盖裸 SUNION/SINTER/SDIFF（备选 1「SINTER 裸命令」同死）；备选 3 的
  EXPIRETIME 在 src 未实现（PEXPIRE 亦已被 #71 盖）→ 取备选 2 的未盖面
  **ES bool must_not**（`_count` 本体已由 #78 盖，must_not 子句 scenario 零命中）。
- 初稿 #88 的 ZREMRANGEBYLEX 与 #54 `job_scheduler_cron` 部分撞车（后者中窗
  `[1 (4` 退役）；保留 #88，明星面为 scenario 零命中的 ZREVRANGEBYLEX，裁剪改用
  前缀形 `[a (b`，调用形不重复。

## 场景矩阵

| # | 文件 | 平面 | 断言要点 |
|---|---|---|---|
| 81 | `scenario_shard_route_xpick_e2e.rs` | RESP | XPICK round_robin 按发现序轮转（q0→q1）、hash 按 CRC16(key) mod n 稳定命中（与路由同函数对算）、least_backlog 落最闲分片且随灌满摆动、回执全形 `parent/child` bulk、三错误 verbatim（`hash strategy requires a shard key`/`unknown pick strategy (round_robin|hash|least_backlog)`/`invalid topic name`） |
| 82 | `scenario_replay_rewind_consumer_e2e.rs` | RESP | XINFO CONSUMERS 每消费者 `*6` 扁平（name/pending/idle）、XGROUP SETID 0-0 回拨后 `>` 重放已消费史、重放 PEL 行随条目换主（Redis 语义：carol 接管 alice 头两行）、DELCONSUMER 回 `:清除 pending 数`（空 PEL :0、二次 :0）、花名册收缩、NOGROUP 与 Invalid stream ID verbatim |
| 83 | `scenario_session_flag_matrix_e2e.rs` | RESP | SET GET 回旧值/新键 nil、XX+GET 过但回旧值、NX 否决 nil/带 GET 回旧值、NX+GET 新键不报错且落写、KEEPTTL 保 TTL（90..=100）而裸 SET 清 -1、NX+XX 与 KEEPTTL+EX 双 `-ERR syntax error` |
| 84 | `scenario_warranty_ttl_fences_e2e.rs` | RESP | EXPIRE NX 仅无 TTL 生效（二次 :0 不延）、LT 只缩（:1）不延（:0）、GT 只延不缩、XX 仅已设 TTL 生效（裸键 :0）、缺键 :0、未知旗标 `-ERR Unsupported option: supported options are NX, XX, GT and LT` |
| 85 | `scenario_score_guard_zadd_flags_e2e.rs` | RESP | ZADD GT 挡降分（:0 含 CH 形）、LT 挡升分、CH 计变更但同分重加不计、XX 不建键/NX 不改分、INCR 回新 score bulk（`$2\r\n15`）且 NX+INCR 否决回 null、NX+XX/GT+LT/INCR 多参三错误 verbatim |
| 86 | `scenario_grade_curve_case_having_e2e.rs` | MySQL | CASE WHEN over 表列分档（含 ELSE）、GROUP BY 别名 + HAVING COUNT(*)>=2 滤瘦桶、AVG 精确十进制（scale-4：`92.3333`/`83.5000`）、ORDER BY grade, student 双列、LIMIT 2 OFFSET 3 窗口 |
| 87 | `scenario_roster_groupconcat_e2e.rs` | MySQL | GROUP_CONCAT 默认逗号/`SEPARATOR ' \| '`/DISTINCT 首见序去重/多参 `name,'=',role`、CONCAT_WS 跳 NULL 而分隔符免疫、CONCAT NULL 传播 `<NULL>`、REGEXP 字节级区分大小写（`^a` 只中 ada） |
| 88 | `scenario_lexicon_prune_bylex_e2e.rs` | RESP | 同分字典 ZREVRANGEBYLEX `+ -` 全倒序、倒窗拼写 `(cherry [apple`、LIMIT 0 2 截头、ZREMRANGEBYLEX `[a (b` 前缀裁剪 :2（区别 #54 中窗）、裁后倒序窗验证幸存者 |
| 89 | `scenario_festival_program_mustnot_e2e.rs` | ES | bool must_not 裸排（非 press 4 篇 + total=4）、must+must_not 组合（feature 非 rejected）、filter+must_not 过 `_count`（3）、must_not 空值全集（6）；含槽位替换史注 |
| 90 | `scenario_sqlrpc_twopc_e2e.rs` | SQL-RPC+MySQL | `FrontSpec{sql_rpc:true}` 首启：三节点各自 RESP+MySQL+sql_rpc，cluster init + `sql_nodes` 注册表收敛轮询（含 `"sql_rpc":""` 空占位判据）、39 腿跨三槽带 INSERT 经 2PC Prepare/Decide 提交、三节点 gather 全账本一致、dup leg_no 远端 prepare 否决（dup: 随 1062 形错误出栈、零落地）、/sql2pc/status 401/unknown |

## 追加钉死（写进覆盖地图的语义点）

- XPICK 回执为**全形** `parent/child` bulk；round_robin 计数器按父主题持久（进程内），
  发现序=字节序；least_backlog 比较留存长度（XLEN 而非未消费数）。
- XGROUP SETID 回拨后 `>` 读者重放已消费史，且**重放条目的 PEL 行换主**（条目级 PEL，
  Redis 语义）；XINFO CONSUMERS 每消费者 6 元素扁平数组、空史消费者仍可见（注册行残留），
  idle 无 PEL 史记 :0。
- SET 否决 + GET 仍回旧值；NX+GET 新键写前回 nil；KEEPTTL 与任何 TTL 选项互斥。
- EXPIRE 围栏回 `:1/:0` 且 TTL 只朝围栏方向动；未知旗标错误文案含全列表。
- ZADD 同分重加在 CH 下也计 0；INCR 否决（NX 存在/GT 不升/LT 不降）回 null。
- GROUP_CONCAT DISTINCT 保首见序；AVG(int) 为 scale-4 十进制文本。
- ZREVRANGEBYLEX 界含拼写随方向反转（max 侧独占在前）。
- ES bool must_not 可与 must/filter 任意并置；_count 体同形。
- sql_nodes 注册表收敛前含空 `"sql_rpc":""` 占位，须轮询至全部 bind 就位；跨槽多行
  INSERT 走 sql_rpc 2PC，读侧为集群 gather（三节点同答案）。
