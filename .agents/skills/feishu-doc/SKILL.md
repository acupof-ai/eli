---
name: feishu-doc
description: "Comment management and media (image/file) operations on Feishu cloud documents via lark-cli."
---

# feishu-doc

文档评论和媒体文件操作。新建/读取/更新文档分别走 `feishu-create-doc` / `feishu-fetch-doc` / `feishu-update-doc`。

## Quick Reference

| Intent | Command |
|--------|---------|
| 列文档评论 | `lark-cli drive file.comments list --params '{"file_token":"<token>","file_type":"docx"}' --page-all` |
| 加文档级评论（任何文档类型） | `lark-cli drive +add-comment --url <doc_url> --content "评论文本"` |
| 解决/恢复评论 | `lark-cli drive file.comments patch --params '{"file_token":"...","file_type":"docx","comment_id":"..."}' --data '{"is_solved":true}'` |
| 回复评论 | `lark-cli drive file.comment.replys create --params '{...}' --data '{"content":...}'` |
| 文档里插入本地图片 | `lark-cli docs +media-insert --doc-id <docx_id> --file ./pic.png` |
| 在指定锚点插入 | `lark-cli docs +media-insert --doc-id <id> --file ./pic.png --selection-with-ellipsis "anchor text..."` |
| 上传 media（不插入） | `lark-cli docs +media-upload --doc-id <id> --file ./x.png` |
| 下载文档媒体 | `lark-cli docs +media-download --token <media_token> --output ./out.png` |
| 预览文档媒体 | `lark-cli docs +media-preview --token <media_token>` |

## Examples

```bash
# 加一条 doc 级评论
lark-cli drive +add-comment \
  --url "https://example.feishu.cn/docx/doxcnXXXX" \
  --content "Need a follow-up on section 3."

# 在文档末尾插入图片
lark-cli docs +media-insert --doc-id doxcnXXXX --file ./chart.png

# 下载文档里的某张图（从 fetch 输出的 token 拿到）
lark-cli docs +media-download --token boxcnYYYY --output ./asset.png
```

## Pitfalls

| Wrong | Right |
|-------|-------|
| 把 wiki url 当 doc_token 用 | 先 `lark-cli wiki spaces get_node` 解析出 obj_token |
| `+media-insert` 给 URL 图片 | 只支持本地路径；URL 图用 `<image url="..."/>` 走 update/create-doc |
| 评论手动拼 @mention/链接结构 | `+add-comment` 自动构造 content |
| 同 token 多次 `+media-insert` 并发 | 内部 4 步编排带 rollback，但请串行调用 |
| 评论 Sheet/Bitable 用 docs 命令 | Sheet/Bitable 也走 `lark-cli drive +add-comment` 即可（按 file_token+type 路由） |
