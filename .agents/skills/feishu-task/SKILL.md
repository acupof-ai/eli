---
name: feishu-task
description: "Manage Feishu tasks and task lists via lark-cli — create, assign, complete, reopen, comment, reminders, attachments, search, and tasklist membership."
---

# feishu-task

`lark-cli task` 覆盖任务和任务列表全生命周期。

## Conventions

- 时间格式：ISO 8601 with TZ，如 `2026-05-22T17:00:00+08:00`
- task vs tasklist：tasklist 是容器，task 是单条；二者各有 search/CRUD
- 成员角色：`assignee`（负责人，可编辑）/ `follower`（关注，仅通知）

## Quick Reference

| Intent | Command |
|--------|---------|
| 创建任务 | `lark-cli task +create --summary "..." --members ou_xxx:assignee --due "2026-05-22T17:00:00+08:00"` |
| 查我的任务 | `lark-cli task +get-my-tasks` |
| 查与我相关 | `lark-cli task +get-related-tasks` |
| 搜任务 | `lark-cli task +search --query "..."` |
| 改任务字段 | `lark-cli task +update --task-guid <guid> --summary "..."` |
| 完成任务 | `lark-cli task +complete --task-guid <guid>` |
| 取消完成 | `lark-cli task +reopen --task-guid <guid>` |
| 分配/移除成员 | `lark-cli task +assign --task-guid <guid> --add ou_a:assignee --remove ou_b` |
| 关注者管理 | `lark-cli task +followers --task-guid <guid> --add ou_a` |
| 评论 | `lark-cli task +comment --task-guid <guid> --content "..."` |
| 提醒 | `lark-cli task +reminder --task-guid <guid> --time "2026-05-22T09:00:00+08:00"` |
| 上传附件 | `lark-cli task +upload-attachment --task-guid <guid> --file ./x.pdf` |
| 设父任务 | `lark-cli task +set-ancestor --task-guid <child> --parent-guid <parent>` |
| 新建 tasklist | `lark-cli task +tasklist-create --name "..."` |
| tasklist 加任务 | `lark-cli task +tasklist-task-add --tasklist-guid <gid> --task-guids <a>,<b>` |
| tasklist 成员 | `lark-cli task +tasklist-members --tasklist-guid <gid> --add ou_xxx` |
| 搜 tasklist | `lark-cli task +tasklist-search --query "..."` |
| 订阅任务事件 | `lark-cli task +subscribe-event --types task.created,task.updated` |

## Examples

```bash
# 创建一个有截止 + 派给 alice 的任务
lark-cli task +create \
  --summary "5/22 发版检查清单" \
  --members ou_alice:assignee \
  --due "2026-05-22T17:00:00+08:00" \
  --description "覆盖前端/后端/部署三块"

# 标记完成
lark-cli task +complete --task-guid <guid>

# 把任务加进列表
lark-cli task +tasklist-task-add --tasklist-guid <list_guid> --task-guids <task_guid>
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 创建任务不加自己 → 之后改不了 | 创建者建议作为 follower（很多场景 `+create` 已自动） |
| `task_guid` vs 老 `task_id` 混用 | `+update/+get` 系列都用 `task-guid` |
| 拿 tasklist 命令操作 task | 单条用 `+create/+update/+complete`；列表用 `+tasklist-*` |
| 创建 tasklist 把 owner 也写进 members | 创建者自动成 owner；不要再加 |
| 用秒时间戳 | 必须 ISO 8601 with TZ |
| 想查"谁完成了" | 历史走 `lark-cli task tasks get` 看 `completed_at`；实时走 `+subscribe-event` |
