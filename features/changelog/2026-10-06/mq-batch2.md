# MQ Batch 2：延迟消息、RocksMQ HTTP 对齐、kafka admin/SASL（含 Batch 1.5 评审修复）

Commit: 5ae5f07（延迟消息 WP2）、fb77a24（rocksmq HTTP parity WP4）、59ed6eb（kafka
admin+SASL WP3）；Batch 1.5 评审修复：fda8098（引擎可靠性收口）、80a3bc2（kafka
headers 存储形态字节精确）+ e39fbd2（配套文档）、09628ff（conf 旋钮）、d043311（组
语义四项）、17d1eb9（覆盖补齐）、6c1ddc6（Batch 1 changelog 回填 shas）

## 背景
`plans/2026-10-06-mq-gap/` 的 B 级（Batch 2）三条车道全部落地：WP2 延迟消息
（`02-delay-messages.md`）、WP4 RocksMQ HTTP 面对齐（`04-http-parity.md`）、WP3
kafka 面余项（`03-kafka-parity.md` scope two：组管理 API + SASL token）。Batch 1
评审暴露的 Batch 1.5 修复（引擎一致性 + 组语义 + headers 字节保真 + 覆盖缺口）
随批收口。状态板见 `plans/2026-10-06-mq-gap/README.md`。

## 变更（Batch 2）
1. **延迟消息**（`src/lite/delay.rs`，5ae5f07）：`XADD <stream> [<id>] DELAY <ms>
   <field> <value>...` 前置选项把消息体暂存进 kind-0x1D 行——键 due-major
   （`<slot> 0x1D <due_ms> <stream> <locked id>`），到期扫描是有界提前停止的窗口
   前缀扫，永不全库扫。到期交换在流 latch 内单 WAL 批完成（行删除 + 条目写入 +
   meta 维护），kill -9 只有"行还在（重启重换）/条目已写（不再换）"两态，不丢不重。
   **交换追加全新 id**（XADD 回复 id 只是预约令牌：被锁 id 可能落在组 delivered
   水位之下而永久不可见，`delay.rs` ID POLICY），并按 XADD 同款唤醒 BLOCK 读者。
   P0 强制登记项全落地：`family_delete_entries` 折叠 0x1D（XIDLE 惰性/主动、DEL/
   EXPIRE、RENAME 覆写、FLUSHDB）、`move_family` 随搬 0x1D 段——已删/改名流绝不
   被残留行复活投递。扫描器由 `lite.delay_sweep_ms` 门控（**默认 0 = 不 spawn**）。
   选项负矩阵：非整数/悬空选项报 Redis 风格错误、`DELAY 0` 走纯路径、到期溢出
   （now+ms 超 u64）拒绝而非回绕。
2. **RocksMQ HTTP parity**（`src/rocksmq/`，fb77a24）：四路由 `POST /produce`/
   `/consume`/`/ack`/`/pending`。`/produce` 增 `delay_ms` 透传（映射 XADD DELAY，
   缺省/0 与旧字节一致；`delay_ms>0` 时回的 id 是 XADD 时点的预约令牌）；
   `/consume` 增 `wait_ms` 长轮询——映射 lite BLOCK park 路径（WakeHub，延迟交换
   的 notify 同时唤醒 tail/group park；NOGROUP-on-empty 现在 park 到首次写入而非
   立即报错）；`/pending` 为 XPENDING summary 透传（pending/min/max/consumers
   JSON）；`rocksmq_token`（**空 = 不设防，默认**）非空时全路由单点 Bearer 门
   （401）。`api.rs` 消费流程外移净 -146 行。
3. **kafka admin + SASL PLAIN**（`src/kafka/admin.rs`、`sasl.rs`，59ed6eb）：
   ListGroups(16) v0-v1 = coordinator runtime ∪ 仅账本组（只 OffsetCommit 过的组
   仅以 0x20 行存在，报稳定占位态 Empty）；DeleteGroups(42) 走 XGROUP DESTROY
   同一条公开 lite 拆除路径（折 0x20 账本行 + lite 组记录 + PEL 窗 + 缓存）并逐出
   runtime 态——**XTRIM/XDEL 账本守卫获得产品级出口**；未知组报 69、批内其余组照常。
   SASL PLAIN 由 `kafka_token` 门控（**空 = SASL 面不存在，字节等价旧 broker**）：
   启用时广告集 +17/36，SaslHandshake 选机制、SaslAuthenticate 常时比较口令，失败
   回固定文案 58 + 关连接（不泄露 token 片段/长度）；pre-auth 白名单 = ApiVersions
   + SASL 对，其余未认证流量不回包直接断。广告 API 集 13 → 15（16/42 恒在，
   17/36 仅武装时），key 校验对拍真实 librdkafka 2.15 帧。

## 变更（Batch 1.5 评审修复）
- **fda8098**：DLQ 转移水位**提交后**才结算（`offset::mark/restore` 快照，失败提交
  保持条目可重投——修静默毒消息丢失）；sweep 转移失败整批作废；`XTRIM MINID ...
  LIMIT 0` 删 0 回 `:0`（差一错误）；OffsetCommit 取流 meta latch（commit-vs-trim
  TOCTOU）+ read/claim/autoclaim/sweep 各点 latch 下复验；空排空轮回 nil 而非
  `[[s,*0]]`；共享 DLQ 重复探测先于 len bump；文件按 400/800 行预算拆分
  （redeliver→redeliver_loop、dlq→dlq_depth、read→read_xread）。
- **80a3bc2 + e39fbd2**：headers **名字**亦字节精确——存储形态 "h" JSON 内名字走
  `"x"` hex 键（任意 wire 字节含非法 UTF-8 往返字节精确，旧 `"n"` 字符串名可读），
  envelope 回退值内 `{"fields":[[hex,hex]...]}` 同理；`rdb-envelope` 撞名规则
  （真用户头胜出、原样回放、永不打标）钉死；COMPAT 同步。
- **09628ff**：conf 旋钮 `lite.delay_sweep_ms`、`kafka_token`、`rocksmq_token`
  （全部默认关闭/空）。
- **d043311**：XGROUP CREATE 拒 DLQ==源流/跨 slot DLQ/空 DLQ 名（修原地覆写、
  错节点 DLQ、静默缺省）；ordered 接管在组锁下校验 owner 租约——只罢免僵尸/失踪
  owner，活持有者永不刷新/罢免（修僵尸租约永久续期）；sweep 逐组 strictly-after
  续读游标（修 16 行后的头饥饿）；XGROUP DESTROY 折叠 0x20 账本行（守卫获得
  lite 侧出口，文案点名）。
- **17d1eb9**：第四个 DLQ 触发面（`>` 重投越限 + XAUTOCLAIM，JUSTID 钉死无副
  作用）；spawned 重投 sweep 进程级 e2e；boot() 自清理根目录（死 pid 残根剪除）。

## 行为变更说明
新面默认全部关闭：`delay_sweep_ms=0`（无扫描器）、`kafka_token=""`（无 SASL 面，
广告集不含 17/36）、`rocksmq_token=""`（HTTP 不设防）、`delay_ms`/`wait_ms` 缺省
时 HTTP 路径字节不变。**唯一默认可见增量**：kafka 广告 API 集 13 → 15（新增此前
不存在的 ListGroups/DeleteGroups 两个 key，纯增量，存量客户端不受影响）与 HTTP
新路由 `POST /pending`。

## 验证
- Batch 2 新增 4 个 e2e 文件共 **20 用例**：`lite_delay_e2e`（9：到期前全读路径
  不可见→到期交换、乱序 due 序交换、BLOCK 唤醒、XIDLE/RENAME/FLUSHDB 三连泄漏
  回归、负矩阵、两态持久）、`lite_delay_proc_e2e`（3：spawn 扫描器进程级、
  spawned 交换唤醒 BLOCK、kill -9 双态）、`rocksmq_wait_pending_e2e`（4：wait_ms
  超时空回/唤醒/延迟可见、pending 汇总与错误面、Bearer 矩阵）、`kafka_admin_e2e`
  （4：union 列表、守卫释放、69 矩阵、SASL 全矩阵）。
- Batch 1.5 补 4 个文件共 **11 用例**：`lite_dlq_triggers_e2e`（3）、
  `lite_redeliver_proc_e2e`（1）、`lite_group_dlq_e2e`（4）、
  `kafka_headers_fidelity_e2e`（3）。
- 场景：`scenario_kafka_sdk.sh` 6/6 OK；`scenario_lite_mq.sh` 增 (i) 延迟消息段
  （DELAY 0 即时可见、到期前 XLEN/XRANGE 盲、到期交换 + 全新 id、BLOCK 读者被
  交换唤醒、RENAME 随搬到新名交换、负矩阵，12 断言），全量 99 断言 PASS。

## 后续
P3 按需池（`plans/2026-10-06-mq-gap/05-p3-pool.md`）整体悬置，触发条件未变；
A/B 级车道至此全部落地，计划转入 landed 状态（状态板：同目录 `README.md`）。
