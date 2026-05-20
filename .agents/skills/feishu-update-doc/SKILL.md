---
name: feishu-update-doc
description: "Update a Feishu cloud document via lark-cli with targeted modes (append, replace, insert, delete) plus full overwrite. Prefer targeted updates over overwrite."
---

# feishu-update-doc

`lark-cli docs +update` 一个命令多个 mode。**优先精准操作**（append/replace/insert/delete），overwrite 会清掉整个文档（包括图片、评论、协作历史）。

## Modes

| Mode | 说明 | 需要 selection | 需要 markdown |
|------|------|---------------|---------------|
| `append` | 在文档末尾追加 | ❌ | ✅ |
| `overwrite` | ⚠️ 全量覆盖（破坏性） | ❌ | ✅ |
| `replace_range` | 替换匹配段落范围 | ✅ | ✅ |
| `replace_all` | find-and-replace（空字符串 = 删除） | ✅ | ✅ |
| `insert_before` | 在匹配项之前插入 | ✅ | ✅ |
| `insert_after` | 在匹配项之后插入 | ✅ | ✅ |
| `delete_range` | 删除匹配段落范围 | ✅ | ❌ |

可选 `--new-title`（≤ 800 字）与任意 mode 组合改标题。

## Selection 两种写法（择一）

### `selection-with-ellipsis` — 按内容选

- 范围匹配：`"start_text...end_text"` — 用 10–20 字符保证唯一
- 完整匹配：`"full_text"`（不带 `...`）
- 字面 `...` 写成 `\.\.\.`

### `selection-by-title` — 按标题选

- 形如 `"## 章节标题"`（带不带 `#` 都行）
- 自动选中从该标题到下一个同级/更高级标题之间所有内容

## Quick Reference

| Intent | Command |
|--------|---------|
| 末尾追加 | `lark-cli docs +update --doc-id <id> --mode append --markdown-file ./more.md` |
| 全量覆盖（慎用） | `lark-cli docs +update --doc-id <id> --mode overwrite --markdown-file ./new.md` |
| 替换匹配段 | `lark-cli docs +update --doc-id <id> --mode replace_range --selection-with-ellipsis "old anchor..." --markdown-file ./new.md` |
| find-and-replace 全部 | `lark-cli docs +update --doc-id <id> --mode replace_all --selection-with-ellipsis "TODO" --markdown "DONE"` |
| 在匹配前插入 | `lark-cli docs +update --doc-id <id> --mode insert_before --selection-by-title "## 结论" --markdown-file ./before.md` |
| 在匹配后插入 | `lark-cli docs +update --doc-id <id> --mode insert_after --selection-by-title "## 背景" --markdown-file ./after.md` |
| 删除某段 | `lark-cli docs +update --doc-id <id> --mode delete_range --selection-with-ellipsis "废弃内容..."` |
| 改标题 | `lark-cli docs +update --doc-id <id> --mode append --markdown "" --new-title "新标题"` |
| 更新白板 | `lark-cli docs +whiteboard-update --doc-id <id> --whiteboard-id <wb_id> --mermaid-file ./diagram.mmd` |
| 覆盖 Drive 原生 .md | `lark-cli markdown +overwrite --file-token <token> --content-file ./new.md` |

## Examples

```bash
# 追加一段 changelog
lark-cli docs +update --doc-id doxcnXXXX --mode append \
  --markdown $'## 2026-05-20\n- 新增 foo\n- 修复 bar'

# 把"待办"全替换成"已完成"
lark-cli docs +update --doc-id doxcnXXXX --mode replace_all \
  --selection-with-ellipsis "待办" --markdown "已完成"

# 在某一节后追加
lark-cli docs +update --doc-id doxcnXXXX --mode insert_after \
  --selection-by-title "## 背景" \
  --markdown-file ./extra.md
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 想小改直接 overwrite | 用 `replace_range`/`insert_*`；overwrite 会丢图片/评论 |
| selection 给得太短匹配多处 | 用足够独特的字符串，或加 `...` 范围匹配 |
| selection 跨段落含换行 | 拼到一行或用 `selection-by-title` |
| `selection-with-ellipsis` 字面 `...` 没转义 | 写成 `\.\.\.` |
| 大范围 replace 覆盖到含图片/白板的段 | 只针对纯文本段；token 引用不能 round-trip |
| 在白板上用 docx markdown 更新 | 白板用 `+whiteboard-update` 配 mermaid/plantuml/DSL |
