# Feishu (Lark)

Native Feishu channel — shells out to [`lark-cli`](https://github.com/larksuite)
for both directions. No sidecar, no Node runtime. Implemented in
`crates/eli/src/channels/feishu.rs`.

## Prerequisites

- `lark-cli` on `PATH`, authenticated as your bot (`lark-cli` handles app
  credentials and token refresh).
- The channel starts automatically under `eli gateway` when `lark-cli` is
  detected.

## Configuration

```bash
# Optional — account id passed to lark-cli; defaults to "default".
ELI_FEISHU_ACCOUNT=default
```

## Inbound

A supervised subprocess streams events:

```
lark-cli event consume im.message.receive_v1 --quiet --as bot
```

- NDJSON stdout, stdin held open, SIGTERM on shutdown.
- On crash, respawns with exponential backoff (`min(60s, 3s·2^(n-1))`).
- Dedup LRU (2000 ids / 24h) drops repeats; app/self senders and stickers are dropped (anti-loop).
- Group chats require an `@`-mention of the bot; direct chats are always processed.
- A 1500ms debounce coalesces bursts into one batch; the last 5 messages of history are prepended as context.

## Outbound

- A `Typing` reaction is added on receipt and cleared when the final reply is sent.
- The first chunk is sent as a quote-reply to the triggering message; long replies are split (`chunk_text`, 25000-byte cap) with continuations sent fresh.
- Error envelopes are rewritten into friendly Chinese before sending.

## Session Semantics

- Session id is `feishu:<account>:<chat_id>`.
- `<account>` comes from `ELI_FEISHU_ACCOUNT` (default `default`).
