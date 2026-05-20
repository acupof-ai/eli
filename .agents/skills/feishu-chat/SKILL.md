---
name: feishu-chat
description: "List Feishu group chat members and resolve chat metadata via lark-cli."
---

# feishu-chat

Group chat 成员/元信息查询，走 `lark-cli im`。发消息走 `feishu-im`；按关键词找群走 `feishu-im` 里的 `+chat-search`。

## Quick Reference

| Intent | Command |
|--------|---------|
| 列群成员 | `lark-cli im chat.members get --params '{"chat_id":"oc_xxx"}' --page-all` |
| 群基础信息 | `lark-cli api GET /open-apis/im/v1/chats/oc_xxx` |
| 找群 by 关键词 | `lark-cli im +chat-search --query "..."` |
| 找群 by 成员 | `lark-cli im +chat-search --member-ids ou_xxx,ou_yyy` |
| 我所在的群 | `lark-cli im +chat-list` |

## Examples

```bash
# 列出群里所有人
lark-cli im chat.members get --params '{"chat_id":"oc_abc"}' --page-all \
  --jq '.items[]|{name,member_id}'

# 群基础信息（名称、群主、人数）
lark-cli api GET /open-apis/im/v1/chats/oc_abc \
  --jq '{name,owner_id,chat_mode,member_count:.user_count}'
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 期望返回机器人成员 | API 不返回 bot 成员 |
| 一次性想拿全部成员不翻页 | 默认分页；加 `--page-all` 自动翻页 |
| 把 user `ou_...` 当 `chat_id` 传 | chat_id 一定是 `oc_...` 开头 |
| 不知道 chat_id 就猜 | 先 `lark-cli im +chat-search --query "群名"` 拿到 |
