---
name: feishu-im-read
description: "Read and search Feishu IM messages via lark-cli — conversation history, thread expansion, cross-chat search, and resource download."
---

# feishu-im-read

Inbound/read-side messaging routed through `lark-cli im`. For sending use `feishu-im`.

## Prerequisites

- 读消息需要对该 chat 的访问权 — `--as user` 走用户身份；`--as bot` 仅能读 bot 在群里的消息。
- ID 映射：chat `oc_xxx`，message `om_xxx`，thread `om_xxx` 或 `omt_xxx`。
- `--chat-id` 和 `--user-id` 二选一；`--relative-time` 和 `--start/--end` 二选一。
- 消息含 `thread_id` 时主动展开 thread。

## Quick Reference

| Intent | Command |
|--------|---------|
| 群/单聊历史消息 | `lark-cli im +chat-messages-list --chat-id oc_xxx` |
| P2P 历史（用 user_id） | `lark-cli im +chat-messages-list --user-id ou_xxx` |
| 限定时间窗 | `... --start "2026-05-20T00:00:00+08:00" --end "2026-05-20T23:59:59+08:00"` |
| 相对时间 | `... --relative-time 24h` |
| Thread 展开 | `lark-cli im +threads-messages-list --thread-id <om_or_omt>` |
| 全局搜索消息 | `lark-cli im +messages-search --query "<keyword>"` |
| 限定群/人/时间搜 | `lark-cli im +messages-search --query "..." --chat-id oc_xxx --sender ou_yyy --start ... --end ...` |
| 批量拉消息 by id | `lark-cli im +messages-mget --message-ids om_a,om_b,om_c` |
| 列我所在群 | `lark-cli im +chat-list` |
| 找群 by 关键词/成员 | `lark-cli im +chat-search --query "周会" --member-ids ou_xxx` |
| 下载消息中的图片/文件 | `lark-cli im +messages-resources-download --message-id om_xxx --file-key <key> --output ./x.jpg` |

## Thread strategy

| 场景 | 取多少 |
|------|--------|
| 默认理解上下文 | 最新 10 条 (`page_size 10`, `sort create_time_desc`) |
| "完整对话"/"详细讨论" | 全部 (`page_size 50`, `sort create_time_asc`)，分页拉完 |
| 仅浏览概览 | 跳过 thread 展开 |

Thread 不支持时间过滤（API 限制），只能分页。

## Examples

```bash
# 看某个群最近一天聊了什么
lark-cli im +chat-messages-list \
  --chat-id oc_abc \
  --relative-time 24h --page-all

# 跨群搜"线上故障"
lark-cli im +messages-search --query "线上故障" --start "2026-05-13" \
  --jq '.items[]|{chat,sender_name,text:.content|@text}'

# 展开一个 thread
lark-cli im +threads-messages-list --thread-id omt_xxx --page-all
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 同时给 `--chat-id` 和 `--user-id` | 二选一 |
| 同时给 `--relative-time` 和 `--start/--end` | 二选一 |
| 看到 `thread_id` 不展开 | 主动用 `+threads-messages-list` 拿 thread 全文 |
| 把 image key 当 URL 访问 | 必须 `+messages-resources-download` 下载 |
| 拉大量历史不分页 | 加 `--page-all` 自动翻页 |
