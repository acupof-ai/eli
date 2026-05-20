---
name: feishu-drive
description: "Manage files in Feishu Drive via lark-cli — list, upload, download, copy/move/delete, comments, permissions, import/export, and bulk sync."
---

# feishu-drive

`lark-cli drive` 覆盖云盘文件操作。**Do not** 用本 skill 处理聊天里的图片/文件（用 `feishu-im-read +messages-resources-download`）；**Do not** 用本 skill 下载嵌入文档里的资源（用 `feishu-doc +media-download`）。

## Quick Reference

| Intent | Command |
|--------|---------|
| 列目录（最快路径） | `lark-cli drive +pull --folder-token fldcnXXXX --dry-run` |
| 文件元信息 | `lark-cli drive file get --params '{"file_token":"<t>","file_type":"docx"}'` |
| 上传文件 | `lark-cli drive +upload --file ./local.pdf --folder-token fldcnXXXX` |
| 下载文件 | `lark-cli drive +download --file-token <t> --output ./out.pdf` |
| 创建文件夹 | `lark-cli drive +create-folder --name "新目录" --parent-token fldcnXXXX` |
| 复制文件 | `lark-cli drive file copy --params '{"file_token":"<t>"}' --data '{"name":"...","type":"docx","folder_token":"fldcnXXXX"}'` |
| 移动 | `lark-cli drive +move --file-token <t> --target-folder fldcnYYYY` |
| 删除 | `lark-cli drive +delete --file-token <t> --type docx` |
| 导出 docx/sheet/bitable | `lark-cli drive +export --file-token <t> --type docx --format docx` |
| 取导出产物 | `lark-cli drive +export-download --file-token <returned> --output ./out.pdf` |
| 导入本地为云文档 | `lark-cli drive +import --file ./foo.docx --folder-token fldcnXXXX` |
| 评论 | `lark-cli drive +add-comment --url <doc_url> --content "..."` |
| 申请权限 | `lark-cli drive +apply-permission --file-token <t> --type docx --perm view` |
| 创建快捷方式 | `lark-cli drive +create-shortcut --target-token <t> --folder-token <fld>` |
| 整目录拉到本地 | `lark-cli drive +pull --folder-token fldcnXXXX --target ./mirror` |
| 整目录推到云 | `lark-cli drive +push --source ./mirror --folder-token fldcnXXXX` |
| 对比本地 vs 云 | `lark-cli drive +status --source ./mirror --folder-token fldcnXXXX` |
| 文件搜索 | `lark-cli drive +search --query "..."` |
| 轮询异步任务 | `lark-cli drive +task_result --task-id <id>` |

## Examples

```bash
# 把整个本地目录同步到云盘（远端不删多余文件）
lark-cli drive +push --source ./docs --folder-token fldcnXXXX

# 把云盘一整个目录拉到本地
lark-cli drive +pull --folder-token fldcnXXXX --target ./backup

# 导出 docx 为 pdf
lark-cli drive +export --file-token doxcnXXXX --type docx --format pdf
# 返回 task_token，下载产物
lark-cli drive +export-download --file-token <returned_token> --output ./out.pdf
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 用 drive 抓聊天里的图片/文件 | 走 `feishu-im-read` 的 `+messages-resources-download` |
| 用 drive 下嵌入文档里的图 | 走 `feishu-doc` 的 `+media-download` |
| copy/move/delete 不带 `--type` | 必须同时给 file_token + type (docx/sheet/bitable/...) |
| 大目录用 for-loop 单文件 download | 用 `+pull` 整目录同步，带断点续传 |
| 操作 wiki 节点 | wiki 用 `feishu-wiki`；wikcn token ≠ file_token |
| import 大文件忘了轮询 | `+import` 返回 task；用 `+task_result` 查状态 |
