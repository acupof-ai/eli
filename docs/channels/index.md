# Channels

Eli uses channel adapters to run the same pipeline across different I/O endpoints. Hooks don't know which channel they're in.

## Builtin Channels

- `cli`: local interactive terminal — see [CLI](cli.md)
- `telegram`: Telegram bot — see [Telegram](telegram.md)
- `feishu`: Feishu/Lark bot via `lark-cli` — see [Feishu](feishu.md)

## Run Modes

Local interactive mode:

```bash
eli chat
```

Channel listener mode — starts every channel whose credentials are present:

```bash
eli gateway
```

Telegram needs `ELI_TELEGRAM_TOKEN`; Feishu needs `lark-cli` on PATH,
authenticated as the bot.

## Session Semantics

- `run` command default session id: `<channel>:<chat_id>`
- Telegram channel session id: `telegram:default:<chat_id>`
- Feishu channel session id: `feishu:<account>:<chat_id>`
- `chat` command default session id: `cli_session` (override with `--session-id`)

## Debounce Behavior

- `cli` does not debounce; each input is processed immediately.
- Telegram and Feishu debounce and batch inbound messages per session.
- Comma commands (`,` prefix) always bypass debounce and execute immediately.
