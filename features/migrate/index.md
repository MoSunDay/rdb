Commit: d481b1d708c248f86be394189d01ca7305fc8528
# 数据迁移

## 能力概述
- 提供 `migrate task/list/help` 命令登记迁移任务（经 raft 复制）。
- 目标（README 待办）：无损数据迁移；当前仅任务登记，未实现实际数据搬迁。

## 触发方式
- `migrate task <startNode> <targetNode> <endNode>`；`migrate list` 查看。

## 行为与规则
- 任务以 `_` 拼接后写入 raft CM 的 `migrate_task` 键；`migrate list` 按 `,` 切分并把 `_` 还原为空格展示。
- 当前实现写入的是单条任务串（未累积历史任务），`list` 实际展示最近一次登记内容。
- `utils/bitmap.go` 为迁移预留的位图工具，当前流程未使用。

## 关键状态与异常
- 异常：参数不足返回 `migrate [ list | task ]` 提示；raft apply 失败返回 `Raft Apply failed`。

## 关联逻辑模块
- [command](../../agents/command/index.md)
- [rcache](../../agents/rcache/index.md)
