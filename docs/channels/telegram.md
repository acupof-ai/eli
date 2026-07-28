# Telegram

Native Telegram channel — long-polling over the Bot API via `reqwest`, no
external runtime. Configured by `TelegramSettings`
(`crates/eli/src/channels/telegram.rs`).

## Configuration

Required:

```bash
ELI_TELEGRAM_TOKEN=123456:token
```

Optional allowlists (comma-separated; `SIDECAR_TELEGRAM_*` accepted as aliases):

```bash
ELI_TELEGRAM_ALLOW_USERS=123456789,your_username
ELI_TELEGRAM_ALLOW_CHATS=123456789,-1001234567890
```

The channel starts automatically under `eli gateway` when `ELI_TELEGRAM_TOKEN`
is set.

## Message Behavior

- Session id is `telegram:default:<chat_id>`.
- `/start` is handled by builtin channel logic.
- A leading `/eli` (or `eli`) prefix is accepted and normalized to plain prompt content.
- In group chats, only messages that address the bot (mention / `eli` prefix / reply-to-bot) are processed; a media-only message must reply to the bot.
- Non-command messages are ingested; debounce batches bursts per session.

## Outbound Behavior

- Outbound is sent back to the chat via the Bot API, MarkdownV2 with a plain-text fallback.
- Empty outbound text is ignored.
- If outbound content is JSON, the `"message"` field is used when present.

## Access Control

- If `ELI_TELEGRAM_ALLOW_CHATS` is set, non-listed chats are ignored.
- If `ELI_TELEGRAM_ALLOW_USERS` is set, non-listed users are denied.
- In group chats, keep allowlists strict for production bots.
