---
name: feishu
description: "Feishu core operations via lark-cli — group chat search/info, spreadsheet read/write, and OAuth management. Use when the user wants to look up chat metadata, work on Sheets, or manage authorization."
---

# feishu

Core Feishu/Lark capabilities routed through the official **`lark-cli`** binary (https://github.com/larksuite/cli). For domain-specific skills (calendar, im, docs, etc.) see the sibling `feishu-*` skills.

Prereq: `lark-cli auth status` to confirm login; `lark-cli auth login` if expired.

## Quick Reference

| Intent | Command |
|--------|---------|
| Search group chats | `lark-cli im +chat-search --query "<name>"` |
| List my chats | `lark-cli im +chat-list` |
| Get chat details | `lark-cli api GET /open-apis/im/v1/chats/<chat_id>` |
| Read spreadsheet | `lark-cli sheets +read --url "<url>"` (or `--spreadsheet-token`) |
| Append rows | `lark-cli sheets +append --spreadsheet-token <token> --sheet-id <sheet> --data '[[...]]'` |
| Create spreadsheet | `lark-cli sheets +create --title "<name>"` |
| Export spreadsheet | `lark-cli sheets +export --spreadsheet-token <token> --format xlsx` |
| Revoke authorization | `lark-cli auth logout` |
| Refresh token | `lark-cli auth refresh` |

## Examples

```bash
# Find a chat_id by group name
lark-cli im +chat-search --query "Eli 运维" --jq '.items[]|{name,chat_id}'

# Read a sheet by URL
lark-cli sheets +read --url "https://example.feishu.cn/sheets/shtcnXXXX"

# Append a row
lark-cli sheets +append \
  --spreadsheet-token shtcnXXXX --sheet-id 0 \
  --data '[["2026-05-20","新需求","ckl"]]'
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 用 sidecar tool `feishu_chat` / `feishu_sheet` | 用 `lark-cli im` / `lark-cli sheets` |
| 凭直觉拼路径调 `api` | 先 `lark-cli schema <service.resource.method>` 看签名 |
| 不带 `--as` | 默认 `user`；需要 bot 身份时显式 `--as bot` |
| 把 Sheet 和 Bitable 混用 | Bitable 用 `feishu-bitable` (`lark-cli base`) |

## Identity

`--as user` 走 user_access_token，`--as bot` 走 tenant_access_token；某些 API 仅其中之一可用，遇到 403/permission_denied 优先换身份再试。
