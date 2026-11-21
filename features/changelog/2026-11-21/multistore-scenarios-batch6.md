# 多存储场景批次 6：#61–#70 十例 e2e（2026-11-21）

承接批次 1–5（2026-11-19/20/21），批次 6 按用户要求**再扩 10 例**，
编号 #61–#70，仍只读复用 `tests/scenario_common/` 夹具、不改 `src/`。
覆盖此前未触达的 ZADD 旗标语义全矩阵、位图告警墙、 deli 叫号队列、
JSON 组织架构文件、流内省（XINFO 分型字段）、双节点槽迁移演习、
向量定宽仪表与受众扫描分页。`tests/` 净增 10 个文件、全部 ≤190 行
（上限 400）。回归：180 个 e2e 目标 + lib 1343 + bins 4 全绿，
clippy `-D warnings` 零告警。

## 场景矩阵

| # | 文件 | 平面 | 断言要点 |
|---|---|---|---|
| 61 | `scenario_sports_playoff_seeding_e2e.rs` | RESP+MySQL | ZADD GT/LT/CH/NX 旗标语义（GT 只接受更差→优的分数、NX 拒已存在成员、NX+GT 组合是 rdb 特有文案 `GT, LT, and/or NX options at the same time are not compatible`）、ZINCRBY 轮次分、ZPOPMIN/ZPOPMAX 平铺 `[member,score]` 对、ZREVRANGEBYSCORE+LIMIT 切争冠窗、ZUNIONSTORE 双组积分聚合、SQL 终榜 |
| 62 | `scenario_url_shortener_slug_e2e.rs` | RESP+MySQL | SETNX 一次性占住 vanity slug、INCR 铸短 id、GETDEL 烧一次性预览令牌、SETEX 热点缓存 + TTL/PERSIST 续命、SETRANGE/GETRANGE 补丁并切片域名白名单、同名 hash 拒 GET（WRONGTYPE）、SQL slug 注册表 |
| 63 | `scenario_sensor_alarm_bitmap_e2e.rs` | RESP | SETBIT/GETBIT 每车道告警位图、BITPOS 带/不带字节偏移找首个告警、BITCOUNT 窗口计数、SSCAN 游标分页传感器注册表、SINTERSTORE/SDIFFSTORE 预计算车道汇总、SMOVE 隔离区转移 |
| 64 | `scenario_deli_counter_queue_e2e.rs` | RESP+MySQL | LPUSH/LRANGE 叫号板、LINSERT BEFORE/AFTER（失锚 -1/缺键 0）、LSET `+OK`/越界/缺键、LINDEX 双端与越界 nil、LTRIM 保服务窗、LPUSHX/RPUSHX 拒建缺键（:0）、LREM 撤号、SQL 号同步 |
| 65 | `scenario_org_chart_json_e2e.rs` | RESP+MySQL | JSON.SET 根对象、OBJLEN/OBJKEYS 内省、ARRINDEX **排他 stop**（`[0,2)` 不含 idx2，COMPAT.md 偏差）、ARRTRIM/ARRPOP/ARRAPPEND、STRAPPEND 数值路径报 `wrong value type: expected string`、STRLEN、NUMINCRBY 紧凑形式、JSON.DEL 子树、HGETALL/HKEYS/HVALS/HSTRLEN 部门名录、SQL 编制 rollup |
| 66 | `scenario_parcel_backlog_xinfo_e2e.rs` | RESP | XADD 裸父 topic 回 `*2 [解析后队列流名, id]`、后续命令全用回执里的流名（`parcels/q0`）、XINFO STREAM 8 字段平铺且**值分型**（length/groups/idle-ms `:int`、last-generated-id bulk）、XGROUP CREATE `0-0` +OK/重名 -BUSYGROUP、XINFO GROUPS 含 last-delivered-id、XREAD `0-0` 全量重放、XTRIM MAXLEN/XDEL 点退、XINFO 裸父名与缺失全名两种错态 |
| 67 | `scenario_slot_migration_drill_e2e.rs` | RESP（双节点） | 真两节点集群（node1 经 raft-http join）：`cluster init` 等分槽带后 owner 探测过 -MOVED、`SETSLOT IMPORTING` 门（非 ASKING 回 -MOVED）、**MIGRATE ≥6 参**（单键无 COPY 须 `MIGRATE h p "" 0 5000 KEYS <key>` 形式）、手搓单帧 ASKING 读落地副本、STABLE 复位门、COPY 保源副本、MIGRATE KEYS 批量迁移+缺席键静默跳过、RESTORE 对已存在键 BUSYKEY 先于载荷解析 |
| 68 | `scenario_vector_capacity_gauge_e2e.rs` | RESP+ES | VADD `VALUES <dim> <elem> <f...>`、VDIM/VCARD 定宽 vs 活数、VSETATTR/VGETATTR 路由标签（未设 nil）、VSIM **VALUES 尾不带 dim 参数**（存储宽度即 arity）、WITHSCORES 配对、VREM 只减计数不减宽度、ES range(gte/lte) 带宽 + size 封顶分页 |
| 69 | `scenario_energy_settlement_ledger_e2e.rs` | RESP+MySQL | INCRBYFLOAT 紧凑回复（1.5+2.5→"4"、+0.25→"4.25"）、非浮点/WRONGTYPE 拒、SETRANGE/GETRANGE 电表改名、HINCRBYFLOAT 费率表 + HSTRLEN 字段宽、GETDEL 结算出户、SQL GROUP BY 汇总结算（**复合主键** `(meter, seq)`） |
| 70 | `scenario_audience_scan_retire_e2e.rs` | RESP | SINTERSTORE/SDIFFSTORE 交/差集受众、SISMEMBER 抽查、SRANDMEMBER 正数去重封顶基数/负数独立重复/缺键 nil、SSCAN MATCH+COUNT 游标分页、SCAN MATCH+COUNT 全键空间分页（每键恰一次、游标归零）、**UNLINK 同 hash tag 家族**过 CROSSSLOT 门批量退役、KEYS 确认清场 |

## 本批钉死的新语义（写前验证 + 实测确认）

- **MIGRATE arity ≥6**：`host port key db timeout` 之后必须还有至少一个选项，
  5 参形式回 `-ERR wrong number of arguments for 'migrate' command`；单键无
  COPY 语义用 `MIGRATE host port "" 0 5000 KEYS <key>`（KEYS 模式下第三参必须
  空串）。带 COPY 的单键 6 参形式合法。
- **裸父 topic XADD 回执**：首个 bulk 是**解析后的队列流名**（`parent/q0`，
  `rocksmq/api.rs` 固定选 q0），不是原样父名；后续 XLEN/XINFO/XGROUP/XREAD/
  XTRIM/XDEL 都要用回执里的这个名字。
- **XGROUP CREATE 起始 id 须全形** `0-0`（裸 `0` 报 `Invalid stream ID
  specified as stream command argument`）；XINFO STREAM 8 字段平铺且值分型
  （计数 `:int`、id bulk）→ 按字段名取行，不能 bulk_at；XINFO 拒裸父名
  （`a full stream name 'parent/child' is required`），缺失全名才报
  `no such key`。
- **VSIM 的 VALUES 尾不带 dim 参数**：`VSIM key VALUES <f...>` 以存储宽度做
  arity；`VADD key VALUES <dim> <elem> <f...>` 才带 dim——两者形状不同。
- **多键命令 CROSSSLOT 门**：不同 slot 的键报 `-ERR CROSSSLOT Keys in request
  don't hash to the same slot`；同 hash tag 家族（`{a7}exp:N`）可通过批清。
- **SQL DDL 强制主键**：无 PRIMARY KEY 建表报 `ERROR 1235 (42000): not
  supported in this version: need a PRIMARY KEY`；批量插入行用复合主键
  `(meter, seq)` 保持 GROUP BY 语义。
- **join 不写路由表**：raft-http join 只入 raft 成员，`cluster_slots_stable_
  instances` 仍空——所有节点对一切 slot 都 Local（没有任何 -MOVED）；必须向
  leader 发 `cluster init <resp0,resp1>` 并等 3s topology tick 收敛（用
  `wait_cluster_nodes_list_all`）。IMPORTING 门：非 ASKING 连接回 -MOVED 指回
  源、ASKING 是连接级**单帧**放行（MIGRATE 自己就是这个协议）；`SETSLOT
  STABLE` 复位回带所有权路由。
- **双节点夹具**：`world_dir(tag)` 同 tag 二次调用会 remove_dir_all——node1
  必须换 tag；`MIGRATE` 目标连的是 dst 的 RESP 端口。
- **GETRANGE 越界/缺键**回 `$0\r\n\r\n`（`rt` 剥一个 CRLF 后断言 `rb()==""`）。

## 回归与收尾

- 180 个 e2e 目标（170 旧 + 10 新）全量分片回归绿；kafka_topics/kafka_wire/
  s3_e2e 三例在 `--test-threads=4` 负载下抖动（s3 单线程 12/12 绿、kafka 复跑
  绿），另清理了一个早前遗留的 probe36 rdb 进程后复验通过；一次回归清单拼写
  坑：目标名漏 `scenario_` 前缀 → `no test target named ...`，修正清单后 10 例
  全绿。`cargo test --lib` 1343 过、`--bins` 4 过。
- `cargo clippy --all-targets -- -D warnings` 零告警（本批修 7 处：/// 文档注释
  挂闭包、4 处多余 `mut`、2 处 `from_str_radix`→`parse`）。
- 文档：`features/e2e-coverage.md` 60→70 例 + 批次 6 小节；`agents/rust/index.md`
  计数同步。
