---
name: feishu-channel-rules
description: Control message formatting and writing style for Lark/Feishu conversations.
alwaysActive: true
---

# feishu-channel-rules

Output formatting, writing style, and reply protocol for messages sent in Lark/Feishu conversations. Always active when the channel is Feishu.

## Reply Protocol (重要)

收到用户消息后**先用一条简短预告回复**告诉用户你打算做什么，**然后再开始正式工作**，最后用主回复输出结果。两条独立消息。

**第 1 步 — 立即用 bash 工具发一条短预告**：

```bash
lark-cli im +messages-reply \
  --as bot \
  --message-id "<刚收到那条消息的 message_id>" \
  --markdown "收到，<一句话说你接下来要做什么>"
```

- `message_id` 是用户那条消息的 `om_...` 形式，在系统给你的会话上下文 / inbound 信息里能拿到。
- 如果实在拿不到 `message_id`，退而求其次用 `--chat-id <从 session_id 里取的 oc_...>` 发新消息，不带 reply 引用。
- 预告**一句话**就够：「收到，我先 X 再 Y」「收到，我去查一下 X」等；不要长。
- 用 `--markdown` 而非 `--text`，让飞书渲染粗体/代码块/列表等。

**第 2 步 — 做正式工作**：调用工具、检索、推理，全套该干啥干啥。

**第 3 步 — 把最终结果作为主回复返回**（你这次 turn 的常规输出）。框架会把这个输出当作第二条消息发出去。

> 已经有 `Typing` 表情自动打到用户消息上做即时视觉反馈，不需要再发"👀 收到"之类的额外 ack。

## Writing Style

- Short, conversational, low ceremony — talk like a coworker, not a manual
- Prefer plain sentences over bullet lists when a brief answer suffices
- Get to the point and stop — no need for a summary paragraph every time

## Constraints

- Lark Markdown differs from standard Markdown in some ways; when unsure, refer to `$SKILL_DIR/references/markdown-syntax.md`

## Pitfalls

| Wrong | Right |
|-------|-------|
| 直接给最终结果，没有任何预告 | 先发一条预告，再开始干活，结果作为主回复 |
| 预告里写一大段计划 | 一句话；详细规划留到正式回复 |
| Use standard Markdown syntax in Feishu messages | Use Lark Markdown; consult `$SKILL_DIR/references/markdown-syntax.md` when unsure |
| Add a summary paragraph to every reply | Keep it short and direct, like a coworker conversation |
| Overuse bullet lists | Use plain sentences for brief answers |
