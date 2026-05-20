---
name: feishu-create-doc
description: "Create a Feishu cloud document from Lark-flavored Markdown via lark-cli, with optional folder or wiki placement."
---

# feishu-create-doc

`lark-cli docs +create` 是建 docx 的首选；`lark-cli markdown +create` 是 Drive 原生 Markdown 文件（不渲染为 docx）。

## Quick Reference

| Intent | Command |
|--------|---------|
| 从 Markdown 创建 docx | `lark-cli docs +create --title "..." --markdown-file ./content.md` |
| 创建到指定文件夹 | `lark-cli docs +create --title "..." --markdown-file ./x.md --folder-token fldcnXXXX` |
| 创建到 Wiki 节点 | `lark-cli docs +create --title "..." --markdown-file ./x.md --wiki-node wikcnXXXX` |
| 直接传 markdown 字符串 | `lark-cli docs +create --title "..." --markdown "# hello\n..."` |
| 创建 Drive 原生 .md 文件 | `lark-cli markdown +create --title "x.md" --content-file ./x.md --folder-token fldcnXXXX` |

## Parameters

| Flag | Required | Description |
|------|----------|-------------|
| `--markdown-file` 或 `--markdown` | 是 | Lark-flavored Markdown 内容 |
| `--title` | 否 | 文档标题；不给则由首行 H1 推断 |
| `--folder-token` | 否 | 父目录 token；留空创建在个人根目录 |
| `--wiki-node` | 否 | Wiki 节点 token；与 `--folder-token` 互斥 |

## Lark-flavored Markdown 扩展

| 元素 | 语法 |
|------|------|
| Callout | `<callout emoji="💡" background-color="light-blue">content</callout>` |
| 双栏 | `<grid cols="2"><column>左</column><column>右</column></grid>` |
| 增强表格 | `<lark-table header-row="true"><lark-tr><lark-td>...</lark-td></lark-tr></lark-table>` |
| 图片(URL) | `<image url="https://..." width="800" align="center" caption="..."/>` |
| 文件 | `<file url="https://..." name="document.pdf"/>` |
| Mermaid | ```` ```mermaid ```` 代码块 |
| @ 用户 | `<mention-user id="ou_xxx"/>` |
| 文字颜色 | `<text color="red">red</text>` |

## Examples

```bash
# 标准 case：从本地 md 创建到指定目录
lark-cli docs +create \
  --title "周会纪要 2026-05-20" \
  --markdown-file ./minutes.md \
  --folder-token fldcnAAAA

# 创建到 Wiki 下的某个节点
lark-cli docs +create \
  --title "Release Notes 0.5.2" \
  --markdown-file ./notes.md \
  --wiki-node wikcnBBBB
```

返回 JSON 含 `doc_token`、`doc_url`；`--jq '.doc_url'` 单取链接发到群。

## Pitfalls

| Wrong | Right |
|-------|-------|
| markdown 用三反引号嵌代码块被 shell 截断 | 用 `--markdown-file` 走文件，避免 shell 转义 |
| 同时给 `--folder-token` 和 `--wiki-node` | 互斥；二选一 |
| Markdown 开头 H1 与 `--title` 重复 | title 已是标题，正文从内容开始 |
| 手动加目录 | Feishu 自动生成 TOC |
| 一次性塞超长内容 | 分段：先建空文档，再用 `feishu-update-doc` append |
