---
name: feishu-fetch-doc
description: "Fetch Feishu cloud document content as Lark-flavored Markdown via lark-cli. Media (images/files/whiteboards) are token references that must be downloaded separately."
---

# feishu-fetch-doc

读 docx 文档内容；正文转 Markdown，媒体保留 token 引用，需要时另调下载。Sheet/Bitable/Wiki 走对应 sibling skill。

## Quick Reference

| Intent | Command |
|--------|---------|
| 拉文档 Markdown | `lark-cli docs +fetch --doc-id <docx_id>` |
| 用 URL 拉 | `lark-cli docs +fetch --url <doc_url>` |
| 拉 Drive 原生 .md | `lark-cli markdown +fetch --file-token <token>` |
| 解析 wiki -> 真实 obj_token | `lark-cli wiki spaces get_node --params '{"token":"wikcnXXXX"}'` |
| 下载文档里的图片/附件 | `lark-cli docs +media-download --token <media_token> --output ./out.png` |
| 读电子表格 | `lark-cli sheets +read --url <sheet_url>` |
| 读多维表格 | `feishu-bitable` (`lark-cli base +record-list ...`) |

## Markdown 里的媒体 tags

| Tag | 提取的 token | 下载 |
|-----|--------------|------|
| `<image token="boxcn..." width="..."/>` | image token | `lark-cli docs +media-download --token boxcn...` |
| `<view type="1"><file token="..." name="..."/></view>` | file token | 同上 |
| `<whiteboard token="..."/>` | whiteboard token | `+media-download` 拿缩略图；要改内容用 `feishu-update-doc +whiteboard-update` |

## Wiki URL 标准三步

```bash
# 1. 拿到 obj_token + obj_type
lark-cli wiki spaces get_node --params '{"token":"wikcnZZZZ"}' --jq '{obj_token,obj_type}'

# 2. 按 obj_type 路由
#    docx    -> lark-cli docs +fetch --doc-id <obj_token>
#    sheet   -> lark-cli sheets +read --spreadsheet-token <obj_token>
#    bitable -> lark-cli base +record-list --app-token <obj_token> --table-id <tid>
#    其他    -> 告知用户类型不支持
```

## Examples

```bash
# 拉 docx 内容
lark-cli docs +fetch --url "https://example.feishu.cn/docx/doxcnXXXX" \
  --jq '.content' > doc.md

# 拉到 markdown 里出现 token，下载对应图片
lark-cli docs +media-download --token boxcnIMG --output ./img.png
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| `wiki/wikcnXXXX` 当 doc_id 直接 fetch | 必须先 `wiki spaces get_node` 解析 obj_token + obj_type |
| 把 `<image token="boxcn..."/>` 当 URL 访问 | 那是 token，必须 `docs +media-download` |
| 拉非 docx (sheet/bitable) 用 `docs +fetch` | sheet → `sheets +read`；bitable → `feishu-bitable` |
| 忽略输出里的 `<image>`/`<file>` tags | 提取 token 按需下载，或告知用户该处有媒体 |
