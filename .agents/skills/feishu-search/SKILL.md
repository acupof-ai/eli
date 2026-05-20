---
name: feishu-search
description: "Search Feishu — users by keyword (contact), and docs/wiki/sheets unified search via lark-cli."
---

# feishu-search

Search across two domains:
1. **People** — `lark-cli contact +search-user`
2. **Docs/Wiki/Sheets** — `lark-cli docs +search` (Search v2 doc_wiki/search; `lark-cli drive +search` 等价)

## Quick Reference

| Intent | Command |
|--------|---------|
| 按姓名/邮箱/手机号搜人 | `lark-cli contact +search-user --query "<keyword>"` |
| 按 open_id 列表批量查 | `lark-cli contact +search-user --user-id-list ou_a,ou_b` |
| 搜文档（含 wiki） | `lark-cli docs +search --query "<keyword>"` |
| 限定文档类型 | `lark-cli docs +search --query "..." --filter-type docx,sheet,bitable` |
| 限定时间窗 | `lark-cli docs +search --query "..." --start "2026-04-01" --end "2026-05-20"` |
| 按创建人/owner | `lark-cli docs +search --query "..." --creator ou_xxx` |

## Examples

```bash
# 找叫"张三"的同事
lark-cli contact +search-user --query "张三" \
  --jq '.items[]|{name,department,open_id}'

# 搜近 30 天里的"周报"相关 doc/sheet
lark-cli docs +search --query "周报" --start "$(date -v-30d +%Y-%m-%d)" \
  --jq '.items[]|{title,type,url}'
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 用 `+search-user` 找文档 | 用 `lark-cli docs +search` |
| 搜聊天记录 | 走 `feishu-im-read` 的 `lark-cli im +messages-search` |
| 不带 `--as user` 搜人 | scope 限制；必须 user 身份 |
| 期望搜到自己没权限的资源 | search 受 ACL 限制，只返回当前身份可访问 |
| 已知 user_id 还去搜 | 直接 `feishu-get` 的 `+get-user --user-id ...` |
