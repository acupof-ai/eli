---
name: feishu-im
description: "Send Feishu messages and replies via lark-cli (user or bot identity), download chat resources, and manage threads/reactions."
---

# feishu-im

Outbound messaging routed through `lark-cli im`. For reading/search use the sibling `feishu-im-read` skill.

## Identity

- `--as user` (default for user-scoped scenarios): runs as the logged-in user; only chats the user is in.
- `--as bot`: runs as the bot identity; only chats the bot is in.
- 同一个 API 在两种身份下成败可能不同，遇到 403 先换身份。

## Safety

`--as user` 发消息时**接收方看到的发件人是用户本人**。调用前确认：(1) 收件人是谁；(2) 发什么内容。未经用户明确确认绝不要发。

## Quick Reference

| Intent | Command |
|--------|---------|
| 发文本到群 | `lark-cli im +messages-send --chat-id oc_xxx --text "hi"` |
| 发文本到人 (P2P) | `lark-cli im +messages-send --user-id ou_xxx --text "hi"` |
| 发 markdown | `lark-cli im +messages-send --chat-id oc_xxx --markdown "**title**\nbody"` |
| 发 post (富文本) | `lark-cli im +messages-send --chat-id oc_xxx --post-file ./post.json` |
| 发图片/文件 | `lark-cli im +messages-send --chat-id oc_xxx --file ./pic.png` |
| 回复某条消息 | `lark-cli im +messages-reply --message-id om_xxx --text "..."` |
| 在 thread 中回复 | `lark-cli im +messages-reply --message-id om_xxx --text "..." --in-thread` |
| 下载消息附件 | `lark-cli im +messages-resources-download --message-id om_xxx --file-key file_xxx --output ./out.jpg` |
| 加表情 | `lark-cli im reactions create --params '{"message_id":"om_xxx"}' --data '{"reaction_type":{"emoji_type":"THUMBSUP"}}'` |
| 撤回消息 | `lark-cli im messages delete --params '{"message_id":"om_xxx"}'` |

## Examples

```bash
# 群里发 markdown
lark-cli im +messages-send \
  --chat-id oc_abc123 \
  --markdown $'**周会提醒**\n时间：14:00\n会议室：4F-A'

# 回复某条消息并在 thread 内
lark-cli im +messages-reply \
  --message-id om_xyz \
  --text "已收到，处理中。" --in-thread

# 给客服群发图片
lark-cli im +messages-send \
  --chat-id oc_support \
  --file ./screenshot.png
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| `--chat-id` 和 `--user-id` 同时给 | 二选一；想给个人就用 `--user-id`（P2P 会自动建/复用 chat） |
| post 富文本拼成 JSON 字符串字段塞 `--text` | 用 `--post-file path.json` 走文件 |
| bot 身份发到没拉过的群 | 先把 bot 加进群；或换 `--as user` |
| 期望 Lark Markdown ≡ GitHub Markdown | 子集有差异（callout/grid/lark-table 等扩展）|
| 用 `feishu-im` 读消息 | 读消息走 `feishu-im-read` |
