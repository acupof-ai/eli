# Sidecar → Native Channels (Feishu / Telegram)

**Date:** 2026-07-28 · **Status:** signed off — executing Phase 0

## Decisions locked
- D1 delete WebhookChannel + sidecar_contract · D2 Telegram = reqwest (not teloxide)
- D3 delete sidecar tool bridge, keep progress notices (repoint to in-channel `kind:notice`)
- D4 Feishu = shell out lark-cli · D5 debounce inside FeishuChannel
- **D6 Weixin: REMOVED this round.** It is personal WeChat via closed-source `@tencent-weixin/openclaw-weixin`
  (OpenClaw plugin, reverse-engineered protocol, NO official API/CLI) → not natively reimplementable.
  Deleted with sidecar; revisit as a separate project if a reliable interface (WeCom official API, or a
  standalone CLI) becomes available.
- Rhythm: **delete-clean first (Phase 0)**, then build native channels.

## Goal
Delete the TypeScript `sidecar/` bridge and reimplement Feishu + Telegram as native
Rust `Channel` impls. Weixin is currently sidecar-only (see D6).

## Verified ground truth
- Deps present: `reqwest{json,stream}`, `regex`, `tokio{full}`, `tempfile`, `base64`,
  `async-trait`, `rand`, `parking_lot`. **Missing:** `reqwest` `multipart` (Telegram media upload).
  No `teloxide` anywhere.
- `lark-cli` v1.0.77 on PATH → Feishu = shell-out is viable.
- `Channel` trait (`channels/base.rs:14`) + `ChannelMessage`/`MediaItem`/`DataFetcher`
  (`channels/message.rs`) are provider-agnostic seams — implement unchanged.
- Wiring is a **single site**: `gateway_command()` `gateway.rs:341`, hand-rolled
  `ingress_tx → forwarder → tx(256) → rx.recv → fw.process_inbound`. Does **not** use
  `ChannelManager` (that path is dead).
- Outbound routes by name: `dispatch_outbound` (`mod.rs:569`) → `channels.get(name).send(reply)`.
  Reply `context` carries `source_channel/account_id/channel_target/reply_to_id`,
  `_eli_cleanup_only` (`CLEANUP_ONLY_CONTEXT_KEY` mod.rs:40), `_eli_mid_turn` (tools.rs:3037).
- Only **Feishu** debounces (1500ms). Telegram does not (getUpdates coalesces). So
  `needs_debounce()` stays false; debounce lives inside FeishuChannel — no need to revive `ChannelManager`.

## Decisions (sign-off required)
- **D1** Delete `WebhookChannel` + `sidecar_contract` entirely (no generic HTTP escape hatch). *Rec: delete — recoverable from git.*
- **D2** Telegram = hand-rolled reqwest (IPv4 pin, Conflict-stop), **not** teloxide. *Rec: reqwest.*
- **D3** Drop the `sidecar` MCP tool bridge (feishu doc/calendar/CI reached via native `lark-*` skills instead). Sub: keep in-channel progress notices (`kind:notice`) or drop. *Rec: drop bridge, keep+repoint notices.*
- **D4** Feishu inbound stays a `lark-cli event consume` subprocess (no native Feishu webhook server). *Rec: keep shell-out → `gateway` feature no longer needs axum.*
- **D5** Debounce inside FeishuChannel; `ChannelManager` stays dead; Telegram doesn't batch. *Rec: as stated.*
- **D6** Weixin QR login (`eli channel login`) dropped, no native replacement. *Confirm out of scope.*

## New files
- `channels/media.rs` — lift generic helpers from webhook.rs (`parse_media_type`,
  `default_mime_type`, `inbound_mime_type`, `base64_data_fetcher`, `path_data_fetcher`,
  `mime_from_path`) + `download_to_temp(url,ext)` + `url_data_fetcher`.
- `channels/text.rs` — pure, unit-tested: `chunk_text` (UTF-8 byte cap 25000, `\n\n`>`\n`>space
  in upper half, fence-safe close+carry lang), `strip_mentions`, `normalize_escaped_whitespace`,
  `friendlyize_error`, `combine_envelopes`. Port every TS `test_*.ts` case.
- `channels/feishu/{mod,consumer,outbound}.rs` — `FeishuChannel` + `FeishuSettings`, supervised
  `lark-cli event consume` loop, `Arc<Mutex<FeishuState>>` (dedup LRU, per-chat debounce timers,
  per-chat inflight `VecDeque`, bot identity `OnceCell`), outbound reply/send/reaction.
- `channels/telegram.rs` — `TelegramChannel` + `TelegramSettings`, long-poll getUpdates,
  access control, media download, MarkdownV2→plain fallback, multipart send, IPv4 pin.

## Delete
- Files: `sidecar_contract.rs` (+lib.rs:35), `channels/webhook.rs` (after lifting media),
  `builtin/cli/sidecar_support.rs` (+mod.rs:13), `builtin/cli/channel.rs` (D6).
- `gateway.rs` fns: `prompt_line`, `ensure_sidecar_config`, `infer_channel_id`,
  `build_sidecar_channel_config`, `start_sidecar`, `spawn_sidecar_process`, `sidecar_retry_delay`,
  `sidecar_is_ready`, `wait_for_sidecar`, sidecar_child spawn/wait/pgid-kill blocks.
- `builtin/tools.rs`: `tool_sidecar`, `build_sidecar_request_payload`, `normalized_*`,
  `SIDECAR_URL` gate (:348), `send_notice`/`extract_notice_params`/`maybe_send_user_facing_notice`
  (+callsites) unless D3b keeps them, `use sidecar_contract` (:26), sidecar test (:3701).
- Static `SIDECAR_URL` (tools.rs:81) + readers.
- CLI: `CliCommand::Channel`, `ChannelAction`, dispatch arm, test.
- Env: `ELI_WEBHOOK_PORT/CALLBACK_URL`, `ELI_SIDECAR_TOKEN/DIR`, `SIDECAR_ELI_URL/PORT/TELEGRAM_TOKEN/SKILLS_DIR`.
- Cargo: `gateway = ["dep:axum"]` → `gateway = []`; add `multipart` to reqwest; axum stays via tape-viewer.

## Adapt
- `gateway_command()` — replace sidecar+webhook block with native construction guarded by
  `TelegramSettings::from_env()` / `FeishuSettings::detect()`; keep ingress forwarder, recv loop,
  `resolve_image_media`, `reconstruct_context_media`, PID lock, ctrl_c, injector/set_channels.
- `channels/mod.rs` — drop webhook mod/exports; add media/text/feishu/telegram mods + exports.
- `builtin/settings.rs` — add `TelegramSettings`/`FeishuSettings::from_env()`; doc env vars.
- `cli/mod.rs` — remove Channel subcommand.
- `builtin/mod.rs` — no change; verify render_outbound tests still pass (channel-name-agnostic).

## Keep untouched
`Channel` trait, `ChannelMessage`/`MediaItem`/`MediaType`/`MessageKind`/`DataFetcher`,
`resolve_image_media` + `MAX_IMAGE_BYTES=20MB`, `reconstruct_context_media`,
`acquire_gateway_lock`, `control_plane::{drain_outbound_media,set_inbound_injector}`.

## Phases
0. **Green tree (atomic delete):** delete all sidecar code, lift media helpers, fix Cargo features,
   delete dead tests. Gateway temporarily 0 channels. `cargo build && test` green. ~1d.
1. **Pure fns + tests:** text.rs + media download. Port all TS test cases. ~1d.
2. **TelegramChannel:** long-poll, access control, group gate, media, multipart, IPv4 pin,
   Conflict-stop. Wire gateway. `tests/test_telegram.py`. ~2d.
3. **FeishuChannel inbound:** consumer supervisor (spawn/respawn/backoff/ready/SIGTERM), NDJSON,
   dedup LRU, anti-loop, sticker drop, @-mention gate, 1500ms debounce, history enrich. ~3d.
4. **FeishuChannel outbound:** inflight FIFO VecDeque (30min TTL), Typing reaction lifecycle,
   quote-reply-first chunking, notice/final/cleanup state machine, routeArgs. `tests/test_feishu.py`. ~2-3d.
5. **Finalize:** `git rm sidecar/`, clippy -D warnings, full test, live smoke, env docs. ~1d.

## Behavior changes flagged
- SIDECAR_ token precedence dropped → single `ELI_TELEGRAM_TOKEN`.
- ECONNREFUSED inbound retry gone (no HTTP hop, in-process).
- HTTP-hop context anti-spoof gone (native channel sets fields itself; no untrusted plugin).
