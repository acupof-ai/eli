---
name: feishu-wiki
description: "Manage Feishu wiki spaces and nodes via lark-cli — list/create/move/copy nodes, resolve wiki tokens to actual document types."
---

# feishu-wiki

Wiki (知识库) 操作。**关键**：wiki URL (`/wiki/wikcnXXXX`) 里的 token 不能直接当 file_token 用 —— 节点背后可能是 docx/sheet/bitable，必须先 `wiki spaces get_node` 解析。

## Quick Reference

| Intent | Command |
|--------|---------|
| 列知识空间 | `lark-cli wiki spaces list --page-all` |
| 知识空间详情 | `lark-cli wiki spaces get --params '{"space_id":"<id>"}'` |
| 创建空间 | `lark-cli wiki spaces create --data '{"name":"...","description":"..."}'` |
| 列空间节点 | `lark-cli wiki nodes list --params '{"space_id":"<id>"}' --page-all` |
| 解析 wiki -> 真实 obj_token | `lark-cli wiki spaces get_node --params '{"token":"wikcnXXXX"}'` |
| 新建节点（自动解析空间） | `lark-cli wiki +node-create --parent <parent_token> --title "..." --type docx` |
| 移动节点 / 把 Drive 文档挪进 wiki | `lark-cli wiki +move --node-token <t> --target-space <space_id>` |
| 复制节点 | `lark-cli wiki nodes copy --params '{"space_id":"<id>","node_token":"<t>"}' --data '{...}'` |
| 删除整个空间 | `lark-cli wiki +delete-space --space-id <id>` |
| 列成员 | `lark-cli wiki members list --params '{"space_id":"<id>"}'` |
| 添加成员 | `lark-cli wiki members create --params '{"space_id":"<id>"}' --data '{"member_id":"ou_xxx","member_role":"admin"}'` |

## Wiki URL -> 真文档：标准三步

```bash
# 1. 拿到 obj_token + obj_type
lark-cli wiki spaces get_node --params '{"token":"wikcnZZZZ"}' \
  --jq '{obj_token,obj_type}'

# 2. 路由：
#    docx    -> lark-cli docs +fetch --doc-id <obj_token>
#    sheet   -> lark-cli sheets +read --spreadsheet-token <obj_token>
#    bitable -> feishu-bitable (app_token = obj_token)
#    其他    -> 告知不支持
```

## Examples

```bash
# 在某个空间下新建 docx 节点
lark-cli wiki +node-create --parent wikcnParent --title "新文档" --type docx

# 把一个 Drive docx 挪进 wiki
lark-cli wiki +move --node-token doxcnAAAA --target-space <space_id>
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 用 wikcn token 直接调 docs/sheets/bitable | 必须先 `+get-node` 拿 obj_token |
| 创建节点不指定 `--type` | 必须 docx/sheet/bitable/mindnote 之一 |
| 删整个空间不确认 | `+delete-space` 是异步任务 + 不可恢复；多次确认 |
| `node_token` 和 `obj_token` 混用 | node_token 是 wiki 节点 ID；obj_token 是真文档 ID |
| 用 wiki nodes list 当搜索 | 搜索走 `feishu-search` 的 `docs +search` |
