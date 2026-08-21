Commit: (working-tree, 随本提交入库)

# Lite MQ 引擎补齐 + Kafka 协议兼容路线决策（rejected for now）

## 背景
Lite Mode（[2026-08-17](../2026-08-17/lite-mode.md)）落地后盘点 MQ 差距：全部为
**引擎级**（PEL、重投递、消费者管理、多流消费、基准），无一是协议级——协议换皮
一个都解决不了。据此固化路线：保留 Redis Streams 动词面为唯一 MQ 线路，Kafka 线协议
兼容现阶段明确拒绝。

## 变更摘要
- **引擎级缺口收口**（`rust/src/lite/`）：
  - PEL 持久化（kind 0x0F 窗口：`group ++ 0x00 ++ id16BE` pending 行 +
    `group ++ 0x01 ++ name` 消费者登记，值 = 信封 + JSON
    `{consumer, delivered_ms, times_delivered}`）；`XREADGROUP ... >` 投递在同一
    latched WAL 批次同步落 PEL（与条目同持久性）；XACK 与组水位持久化原子同批。
  - 重投递：XPENDING（摘要 + 范围形态，IDLE/consumer 过滤）、XCLAIM
    （min-idle/FORCE/JUSTID）、XAUTOCLAIM（游标、COUNT 默认 100、10×COUNT 扫描上限、
    Redis≥7 三元素回复含 deleted-ids）。
  - 消费者管理：XGROUP CREATECONSUMER / DELCONSUMER（DELCONSUMER 清该消费者全部
    pending 行，回复 = 清除数）；XGROUP DESTROY 范围删除整组 0x0F 窗口。
  - 多流 XREAD / XREADGROUP：配平 STREAMS 列表、每流 COUNT；阻塞只 park 一个跨全部
    `>` 流共享的 waiter。
  - 崩溃模型：at-least-once——重启回卷已提交水位重投未 ACK 条目（可能重复），客户端
    幂等是契约；200ms 水位刷盘窗口只会重投、不会丢。
- **可观测性与基准**：`rdb_lite_backlog` gauge（各组缓存 pending 计数之和，首载从盘
  重算）；bench 新增 xadd / xreadgroup / xack 工况。
- **路线决策落盘**：新增 [features/mq-lite.md](../../mq-lite.md)——Kafka wire 兼容
  （11 个 wire API + 内嵌 group coordinator 的 generation/epoch/rebalance 状态机 +
  RecordBatch v2 + 4 codec，以月计）为远期可选前置适配（kafka front：parent/child →
  topic/partition）。否决理由：引擎缺口与协议无关；半兼容是对节点本地非复制数据面的
  生产信任陷阱（Kafka 客户端预期 acks=all/ISR/幂等/事务）。`rust/COMPAT.md` Lite 条目
  同步：动词清单、Lite 偏差（XACK 回复计水位推进、XCLAIM 仅 FORCE/JUSTID、显式 id
  XREADGROUP 只读本消费者 PEL、idle 取自 delivered_ms、多流阻塞单 waiter）与决策指引。

## Impact Surface
- 新增可感知命令面：XPENDING / XCLAIM / XAUTOCLAIM / XGROUP CREATECONSUMER /
  DELCONSUMER；XREAD/XREADGROUP 从"恰好单流"放宽为配平多流。
- XACK 回复语义不变（水位推进计数），内部增删 PEL 行。
- 路线决策为文档性结论，不改既有命令语义；Lite 元数据仍不经 raft（节点本地）。

## Related Docs
- [features/mq-lite.md](../../mq-lite.md)
- [agents/rust](../../../agents/rust/index.md)
- [rust/COMPAT.md](../../../rust/COMPAT.md)
- [2026-08-17 lite-mode](../2026-08-17/lite-mode.md)
