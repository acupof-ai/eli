---
name: feishu-oauth
description: "Authenticate, refresh, inspect, and revoke Feishu/Lark OAuth credentials via lark-cli."
---

# feishu-oauth

通过官方 CLI 管理认证。`lark-cli auth` 自带交互式 OAuth，不再需要老的 sidecar `feishu_oauth_batch_auth` 流程。

## Quick Reference

| Intent | Command |
|--------|---------|
| 一次性走完所有 scope 授权 | `lark-cli auth login` |
| 查当前 token 状态 | `lark-cli auth status` |
| 强制刷新 | `lark-cli auth refresh` |
| 撤销 / 重置 | `lark-cli auth logout` |
| 列已授权 scope | `lark-cli auth status --jq '.scope|split(" ")'` |
| 列 profiles | `lark-cli profile list` |
| 切换 profile | `lark-cli profile use <name>` |
| 看某个 API 需要的 scope | `lark-cli schema <service.resource.method>` |
| CLI 健康自检 | `lark-cli doctor` |

## Examples

```bash
# 第一次使用 / token 过期了
lark-cli auth login

# 看 token 还有多久过期
lark-cli auth status \
  --jq '{identity,brand,expiresAt,refreshExpiresAt,tokenStatus}'

# 切换到企业 profile
lark-cli profile list
lark-cli profile use enterprise-bot
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| token `needs_refresh` 就重新 login | 通常 `auth refresh` 即可 |
| 多 profile 时忘了当前是谁 | `lark-cli auth status` 看 `appId`/`identity` |
| 调 API 报 scope 不足直接放弃 | `lark-cli schema <method>` 看要哪些 scope；重新 login 时勾选 |
| 把 `auth logout` 当作"重新登录" | logout 是撤销；想重新登录直接 `auth login` |
