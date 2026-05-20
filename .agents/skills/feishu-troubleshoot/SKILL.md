---
name: feishu-troubleshoot
description: "Troubleshoot Feishu/Lark issues — auth, scopes, identity, callback subscription, and CLI health checks via lark-cli doctor."
---

# feishu-troubleshoot

排查飞书集成问题的第一站。任何 401/403/scope/permission 错误，先从这里查。

## First-line diagnostics

```bash
# 一键体检（auth + 网络 + 配置）
lark-cli doctor

# 看 token 详情
lark-cli auth status

# 看某个 API 需要什么 scope
lark-cli schema im.messages.create
```

## FAQ

### `401 Unauthorized` / `token expired`
- `lark-cli auth refresh`；refresh 也失败再 `lark-cli auth login`

### `403 permission denied` / scope 缺失
- `lark-cli schema <service.resource.method>` 看 required scopes
- 在 https://open.feishu.cn/app 给 app 加 scope，重新 `auth login` 勾上
- 用 `--as bot` 时要保证 app 自身 scope（不是 user 借的）

### `99991663` / `99991661` 等数字错误码
- 通常是 access_token 类型错配：换 `--as user` ↔ `--as bot` 再试

### 卡片按钮点了没反应
- 在 https://open.feishu.cn/app 选你的 app → **事件与回调**
- 订阅方式改成 **长连接**
- 添加事件 `card.action.trigger`（卡片回调交互）
- 提交审核 / 发布版本

### 群里 bot 收不到消息
- 把 bot 加入群
- 检查 bot **可用范围**是否含当前租户/部门
- 检查 `im:message` 等 scope 是否开启

### 同样的接口 user 通 bot 不通（或反之）
- IM/Doc 类 API 大量场景要求特定身份：
  - 读写"我的"私聊/收藏 → user
  - 服务端长跑后台 → bot
- 换身份重试 + 看 `lark-cli schema` 里的 `supportedIdentities`

### Wiki 链接拿不到内容
- wikcn token ≠ file_token；先 `lark-cli wiki spaces get_node --params '{"token":"wikcnXXX"}'` 拿 obj_token

### Bitable 写入报 `125406x`
- 字段类型/值格式不匹配；先 `lark-cli base +field-list` 看 type/ui_type，再按规范构造（见 `feishu-bitable`）

## CLI 升级

```bash
lark-cli --version
lark-cli update     # 升级到最新
```

## 调试技巧

| 想看什么 | 怎么做 |
|----------|--------|
| 请求体（不执行） | 加 `--dry-run` |
| 易读的响应 | 加 `--format pretty` |
| 自动翻页 | 加 `--page-all` |
| 过滤 JSON | 加 `--jq '<expr>'` |
| 原始 HTTP 日志 | `LARK_DEBUG=1 lark-cli ...` |
