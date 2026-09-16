# SQL 数据面 W2：DECIMAL 贯通 + 复合主键 + 单机时钟下限

Commit: 本批次（81ff8d9、2c935db、5b67c47、8372d90、63192d8、45b8dc2）

## 背景

W2 迭代目标：SQL 数据面补齐精确数值类型与复合主键，并收口单机（无 raft 时钟栅栏）
场景下的可见性缺口。范围裁决先行：四项能力明确裁剪、大声拒绝（见"关键决策"），
不留半支持状态。

## 变更

1. **DECIMAL(p,s) 贯通**（`81ff8d9`）：`SqlType::Decimal{precision,scale}` /
   `Value::Decimal(i128, u8)`；存储 typed tag `0x08`（payload 与 key 同标签），key 用
   定宽保序编码（字节序=值序、含负数），可进二级/唯一索引与 ORDER BY。字面量与
   预编译参数精确解析；算术精确——`+`/`-` 对齐较粗 scale、`*` 尾数乘 scale 加、
   `%` 对齐，`/` 为长除（商 scale+4，余数 half-away-from-zero）；`abs()`；比较
   Decimal/Int 全程 i128 精确；SUM(decimal) 同 scale 定型、AVG 走 scale+4 精确除法；
   写入按列 scale 舍入、超界报 1292；wire `MYSQL_TYPE_NEWDECIMAL`。
2. **复合主键**（`2c935db`）：`PRIMARY KEY(a,b)` 多列按声明顺序保序拼接（变长成分
   NUL 转义 + 0x00 终止符，字节序=元组序）；catalog `TableSchema.pk` String→
   `Vec<String>`（旧 JSON `"pk":"id"` 字符串经 `de_string_or_vec` 反序列化为单元素
   向量）；索引/唯一/2PC 写集/latch/write probe 全多列化；拆出
   `parse/translate_type.rs`、`exec/write_probe.rs`（行数红线）。
3. **单机 kill -9 ts floor**（`5b67c47`）：每个打戳持久化批原子捎带保留键
   `\x00sql_ts_floor`（批内最大 ts，随批 fsync）；boot `advance_to` 恢复，normal 与
   backup 两类监听路径都覆盖。此前单机模式无任何时钟栅栏——kill -9 重启后 oracle
   从头计数，旧版本行以更高 ts 遮蔽自己的重写，已提交数据"消失"。
4. **旧版升级兜底扫描**（`8372d90`）：floor 键缺失（= 库由 pre-floor 二进制写入后
   原地升级）时，boot 一次性全键空间扫描（行版本键 slot 交叉校验 + columnar 段
   commit_ts）取最大 ts 并立即落键——只有升级后第一次 boot 付扫描代价。
5. **场景断言扩展 + result_type 元数据修复**（`63192d8`）：mysql_orders/
   starrocks_analytics 共 292 断言（DECIMAL 精确性/索引点查/1292 边界、复合 pk
   upsert/点查/PRI 标志/1075、StarRocks 多列 PK 模型、DOUBLE-in-composite 与
   columnar-DECIMAL 拒绝）。写场景时发现真缺陷：`result_type` 把 SUM(decimal)/
   AVG(decimal)/decimal 算术结果列标成 DOUBLE/INT，SDK 把精确文本 cell "0.60"
   读成 0.6、二进制编码器直接拒发；元数据改为镜像值形状（AVG(int)→DECIMAL(38,4)）。
6. **升级演练**（`45b8dc2`）：`scrtips/e2e_scenarios/upgrade_rehearsal.sh`——
   c22ff37→HEAD 同批共升（SIGTERM 全停→换二进制→同数据目录复启），94 断言 ×2 全绿；
   证据见同目录 `RESULTS.md`。

## 关键决策（裁剪四项，均 1235/1075 大声拒绝）

- DECIMAL 列不可作主键（含单列 pk）：key 编码尚未 decimal 化进 pk 路径，宁可拒
  不可乱序。
- DECIMAL 列不可进 columnar 表：段页无 decimal 编码。
- 复合 pk 列类型收窄为 Int/VarChar/Date/DateTime：只有定宽/可转义变长成分能无歧义
  拼接；Bool/Double/Blob/Decimal 拒绝。
- 复合 pk 不支持 AUTO_INCREMENT（1075）：自增列必须"就是"单列主键。

## 发现并修复的缺陷（3）

1. 单机 kill -9 后时钟回退 → 已提交行不可见（见变更 3；修复前任何单机重启都可能
   复现，只是 ts 回退幅度小于已写数据时侥幸不显）。
2. pre-floor 二进制原地升级：floor 键缺失，修复 3 对旧库不生效——补一次性 boot
   扫描（变更 4）。
3. SUM/AVG/decimal 算术结果列 wire 元数据错标（变更 5），SDK 客户端与二进制协议
   双双受害。

## 验证矩阵

- 单机全绿：全 workspace 测试通过（935 lib 用例 + 54 个套件）。
- 场景：mysql_orders + starrocks_analytics 扩展后共 292 断言，全绿（真实
  mysql 8.0.44 客户端）。
- 升级演练：upgrade_rehearsal.sh 94 断言 × 2 次连续全绿（含冷构建，自动清理
  worktree/scratch）。
- soak：运行中——待回填：时长、写次数、p99（ms）、慢请求数（>1s）、错误数、
  beacon 缺口数、kill -9 轮次恢复情况。
- 集群索引点查：仍为 scatter-gather 退化（`gatherable()` 结构性分流，EXPLAIN
  Gather banner）——维持现状并已在 `features/sql-dataplane.md` 限制清单明示，
  非 flag。

## 已知 flake

`sql_2pc_e2e` 3 节点 bootstrap join 竞态：偶发 `join raft cluster: head timeout`
（与既往高负载 stall 形态同族，本轮改动无关），复跑即绿。不作代码处理。
