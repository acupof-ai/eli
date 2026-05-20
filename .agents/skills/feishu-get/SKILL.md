---
name: feishu-get
description: "Look up Feishu/Lark user info by ID (or self when no ID given) via lark-cli."
---

# feishu-get

单用户信息查询，走 `lark-cli contact`。按关键词搜人走 `feishu-search`。

## Quick Reference

| Intent | Command |
|--------|---------|
| 查自己 | `lark-cli contact +get-user` |
| 查指定用户 | `lark-cli contact +get-user --user-id ou_xxx` |
| 按手机号/邮箱反查 | `lark-cli contact +search-user --query "13xxxxxxxxx"` |

## Examples

```bash
# 当前登录人
lark-cli contact +get-user --jq '{name,email,department:.department_ids}'

# 指定用户
lark-cli contact +get-user --user-id ou_abc \
  --jq '{name,email,mobile,leader:.leader_user_id}'
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 拿不到 user_id 时直接问用户 | 先 `lark-cli contact +search-user --query "姓名"` |
| 期待拿到员工号 | scope 不含 `contact:user.employee_id` 时拿不到 |
| 不带 `--as user` 想搜人 | `+search-user` 必须 user 身份 |
| 用 `+get-user` 当搜索 | get-user 只接受已知 ID；模糊查找用 `+search-user` |
