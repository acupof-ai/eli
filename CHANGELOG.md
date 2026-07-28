# Changelog

All notable changes to the **eli** workspace are documented here.

Format based on [Keep a Changelog](https://keepachangelog.com/).

---

## [Unreleased]

### Breaking
- **Sidecar removed** — the TypeScript `sidecar/` bridge (OpenClaw host, MCP tool bridge, HTTP/Webhook channel, WeChat/Slack/Discord/DingTalk plugins) is deleted. Channels are now native Rust inside the single `eli` binary.
- **Feishu** — reimplemented natively via `lark-cli` (`crates/eli/src/channels/feishu.rs`): supervised `event consume` subprocess for inbound, `im`/`reactions` commands for outbound. Requires `lark-cli` on PATH.
- **Telegram** — reimplemented natively as hand-rolled Bot API long-polling over `reqwest` (`crates/eli/src/channels/telegram.rs`), replacing the sidecar plugin. `SIDECAR_TELEGRAM_*` env vars still accepted as aliases.
- **WeChat channel dropped** — personal WeChat had no official API/CLI to build a native path on; removed this round.
- **Removed env vars** — `ELI_WEBHOOK_PORT` and all `ELI_SIDECAR_*` no longer exist.
- **Removed tooling** — `just test-sidecar` / `just dev-sidecar`, `scripts/test_sidecar.sh`, and the node/npm/bun prerequisites.

### Added
- `ELI_FEISHU_ACCOUNT` — Feishu account id passed to `lark-cli` (default `default`).
- `docs/channels/feishu.md` — native Feishu channel docs.

---

## [0.5.1] — 2026-04-28

### Added
- **Volcano Coding Plan setup** — `eli login coding-plan` and `eli login volcano` configure the Coding Plan OpenAI-compatible endpoint, API key, and supported model list.

---

## [0.5.0] — 2026-04-08

### Breaking
- **HookPoint enum** — `HookError::Plugin.hook_point` changed from `&'static str` to `HookPoint` enum
- **nexil v0.8.0** — `apply_context_budget()` now accepts `context_window: Option<usize>` parameter

### Added
- **Model-aware context budget** — tape trimming uses model's actual context window instead of hardcoded 400K/200K char limits
- **Tool loop context budget** — stops at 80% of context window; iteration cap configurable via `ChatRequest.max_tool_iterations`
- **Streaming cancellation** — SSE streaming supports `CancellationToken`; `/stop` actually stops mid-stream
- **Runtime provider registration** — `ProviderRegistry` allows custom LLM providers without source modification
- **OAuth auto-refresh** — automatic token refresh on 401 with single-flight guard
- **Integration tests** — 5 Rust integration tests for full framework pipeline
- **Context truncation telemetry** — `tracing::warn!` emitted when conversation history is trimmed

### Changed
- **parking_lot** — all 47 `std::sync` lock-poisoning sites replaced with `parking_lot` (never poisons)
- **Hook panic payloads** — `catch_unwind` handlers extract and log panic messages
- **OnceLock tool cache** — lock-free reads via `OnceLock` replacing `Mutex` + clone per turn
- **SSE byte buffer** — `SseDecoder` uses `Vec<u8>` internally, fixing multibyte UTF-8 corruption
- **Arc tape entries** — `InMemoryTapeStore` uses `Arc<TapeEntry>` for O(1) clone on read
- **HookPoint enum** — stringly-typed hook names replaced with type-safe enum
- **State merge docs** — precedence documented and tested (last-registered wins)

### Fixed
- SSE multibyte UTF-8 characters split across chunks no longer corrupted
- `.env` loaded once in `main()` instead of 4 redundant call sites
- `eli_home()` consolidated to single source in `config.rs`
- `populate_model_tools_cache()` now wired at startup (was never called)

---

## [0.4.1] — 2026-03-26

### Added
- **Channel join greeting** — configurable static greeting on new session or bot added to group
  - Telegram: detects `ChatMemberUpdated` when bot is added to a group
  - Webhook/Discord: accepts `Join` message kind from sidecar plugins
  - CLI: prints greeting after welcome banner
  - Framework: dispatches greeting on first message in a new session (empty tape)
- **Greeting config** — `[greeting]` section in `config.toml` with built-in default; env override via `ELI_GREETING_MESSAGE`
- **Discord channel** via `@openclaw/discord` sidecar plugin
- **Subagent tool descriptions** enriched with scenario triggers

### Changed
- System prompt and personality prompt polished for clarity
- Tool parameter descriptions shortened for token efficiency
- `browser-use` skill replaced with `opencli`

### Fixed
- OpenClaw exports patched for Discord plugin compatibility
- Empty `image_path` treated as `None` in `message.send`
- Useful system prompt directives restored after polish pass

---

## [0.4.0] — 2026-03-26

eli 0.4.0 · nexil 0.7.0

Lazy context management, universal media pipeline, parallel tool execution, and control plane foundations.

### Added
- **Lazy context** — spill large tool results and arguments to disk; strip images from tape to keep context lean
- **message.send tool** — mid-turn user messaging so the model can communicate progress before finishing
- **Universal outbound media pipeline** — skills can send images to any channel (CLI, Telegram, etc.)
- **Parallel tool execution** — run independent tool calls concurrently; cache model-tools schemas
- **Tool feedback signals** — structured notices for better LLM comprehension of tool outcomes
- **Auto-generated tool notices** — notice text derived from schema, manual description param removed
- **save_state / dispatch_outbound hooks** — two new hook points wired into BuiltinImpl
- **Feature flags** — `telegram`, `gateway`, `tape-viewer` for conditional compilation
- **Token usage display** — show token counts in `eli chat` and `eli run`; write usage to tape events
- **Control plane Phase 0** — turn context, cancellation tokens, budget ledger
- **Autoresearch skill** — autonomous experiment loop for research workflows
- **Security hardening** — subagent sandboxing, sensitive field redaction in Debug impls

### Changed
- **Elegance sweep** — SRP splits, iterator pipelines, dead code removal across both crates
- **All 31 SKILL.md files** standardized to English with uniform structure
- **Gateway internals** — JoinSet + bounded channel, model.rs SRP, envelope lifetime fix
- **tool_notices** setting moved from env var to `config.toml`
- Command prefix changed from `,` to `/`

### Fixed
- Telegram gateway replies silently dropped due to `output_channel("null")`
- Duplicate replies from `message.send` on simple questions
- Spill path canonicalization, char-count thresholds, image restore on current turn
- `run_tools` now uses tape history + full current-turn context
- Outbound control flow — `sendText` errors propagate correctly
- HTML stripping regex handles arbitrary closing tag content
- CodeQL data-flow chain in login account masking
- `express-rate-limit` static import + direct dependency

---

## [0.3.2] — 2026-03-25

eli 0.3.2 · nexil 0.6.2 (formerly conduit)

WeChat channel support, agent module refactor, crate rename, and a full integration test suite hitting real LLM APIs.

### Added
- **WeChat channel** — `openclaw-weixin` plugin via sidecar, supports text messaging through WeChat Work (企业微信)
- **Integration test suite** — 31 Python tests across CLI + gateway, hitting real OpenAI and Anthropic APIs
  - `test_basic.py` (15 tests): smoke, text chat, provider switching, unicode, error handling
  - `test_vision.py` (7 tests): multimodal single/multi-image, hallucination detection
  - `test_gateway.py` (9 tests): full IM pipeline via sidecar mock channel — InboundEnvelope → sidecar → eli → LLM → sidecar → mock plugin
- **Sidecar test harness** — mock channel plugin + `/test/*` endpoints for end-to-end gateway testing
- **Integration test rules** in CLAUDE.md — new features require CLI integration tests

### Changed
- **Crate renamed: `conduit` → `nexil`** — the LLM toolkit crate was renamed to avoid crates.io name collision. nexil = nexus + silicon (硅基连接体)
- **Agent module split** — monolithic `agent.rs` (1400+ lines) refactored into `agent_request`, `agent_run`, `agent_command` modules
- **5 `unwrap()` calls eliminated** across nexil core (anthropic_messages, error_classify, message_norm, response_parser)
- **`ValueExt` trait** — envelope helper functions refactored from free functions to trait methods

---

## [0.3.1] — 2026-03-25

eli 0.3.1 · conduit 0.6.1

Full P0-P2 architecture hardening across both crates. 20 fixes, 8 new tests, 603 total passing.

### Fixed
- **Production panic removed** — `panic!()` in OpenAI adapter replaced with `Result<Value, ConduitError>`
- **Unsafe code eliminated** — `unsafe` pointer cast in CircuitBreaker middleware replaced with `Arc<Mutex>` clone
- **OOM protection** — 10MB response limit on `web.fetch`, 50MB file limit on `fs.read`
- **Tape memory cap** — `InMemoryTapeStore` capped at 10K entries per tape with oldest-first eviction
- **Orphan tool-call pruning** — strips individual orphaned calls instead of dropping entire assistant messages with valid content
- **ChannelManager CPU waste** — busy-poll loop (50ms `is_finished()`) replaced with direct `JoinHandle` await
- **Shell memory leak** — finished shells auto-cleaned from `ShellManager` HashMap on output read
- **Telegram shutdown hang** — 5-second poll timeout for responsive cancellation
- **`from_batch()` panic** — returns `Option<ChannelMessage>` instead of panicking on empty input
- **Anthropic release-mode guard** — `debug_assert_eq!` replaced with real transport validation returning `Result`
- **Media download silence** — failed Telegram media downloads now surface error messages in conversation
- **API key leakage** — `mask_sensitive()` sanitizer strips Bearer tokens and key prefixes from error logs
- **Sidecar startup speed** — exponential backoff (200ms→3s) replaces fixed 1-second health check delays

### Changed
- Removed 4 unused dependencies: `fuzzy-matcher`, `glob`, `which` (eli); `schemars` (conduit)
- Removed dead sync `TapeManager` field from `LLM` struct — only `AsyncTapeManager` is active
- Documented hook panic safety policy (chain-aborting vs best-effort)
- Subagent tool marked as `[EXPERIMENTAL]`

---

## [0.3.0] — 2026-03-20

eli 0.3.0 · conduit 0.6.0 · eli-sidecar 0.2.0

### Added
- **MCP server mode** — sidecar exposes tools over stdio JSON-RPC for Claude Code / Cursor (`--mcp` flag)
- **Auto-handoff** — automatic tape branching with grace period at 70% context window
- **Structured trace logging** — `ELI_TRACE=1` writes structured logs to `~/.eli/logs/`
- **Scenario-based tool descriptions** — concrete use cases in tool help text, bash `description` parameter
- **User-facing sidecar tool notices** — visibility into sidecar tool execution
- **Progressive disclosure** — sidecar tools surfaced as discoverable skills
- **Feishu multimedia** — photo, audio, video, document support in sidecar channel

### Fixed
- SSE data buffering across chunk boundaries in `LLM::stream()`
- Bearer auth missing from embedding client requests
- Lock ordering in `InMemoryTapeStore::reset()` preventing deadlock
- Sidecar process group cleanup on gateway shutdown
- OpenAI tool call delta merging by index instead of appending
- Consecutive assistant messages after aggressive tape trim
- Inbound context propagation for typing cleanup
- Feishu typing reaction cleanup on empty replies
- Tool errors fed back to LLM instead of aborting run
- `remaining==0` no longer permanently blocks future auto-handoffs
- UTF-8 safe truncation in trace output
- Sidecar auth, error classification, inbound sanitization hardened
- Tape persistence and hook runtime hardened

### Changed
- Response parser extracted into composable per-transport functions
- `build_chat_entries` extracted for DRY sync/async `record_chat`
- `aggressive_trim` helpers extracted for testability
- Data-driven model spec table for context window and max output tokens
- Sidecar made plugin-agnostic with standard SKILL.md protocol
- Hardened abstractions across conduit and eli crates

---

## [0.2.0] — 2026-03-10

### Added
- **Webhook channel** — generic HTTP bridge for external services
- **Node.js sidecar** — loads OpenClaw plugins (Feishu, DingTalk, Discord, Slack)
- **One-command gateway** — `eli gateway` starts all enabled channels + sidecar
- **Sidecar tool bridge** — external plugin tools callable from eli pipeline
- **Tape system** — append-only history with anchors, search, fork, and handoff
- **Decision system** — persistent cross-turn memory (`eli decisions`)
- **Skills system** — `SKILL.md` discovery with project/global/sidecar precedence
- **Subagent tool** — spawn subprocess agents for parallel work
- **Embedding support** — `LLM::embed()` for vector operations
- **GitHub Copilot provider** — OAuth-based authentication

### Fixed
- Anthropic OAuth adaptive thinking + Claude Code identity
- Telegram shutdown via `CancellationToken` + `abort()`

### Changed
- Crate renamed from `republic` to `conduit`
- Provider runtime centralized in conduit
- System prompt loaded from Markdown files
- Message normalization layer for cross-provider compatibility

---

## [0.1.0] — 2026-03-01

### Added
- **eli** — hook-first agent framework with 12-point pipeline
- **conduit** — provider-agnostic LLM toolkit (OpenAI, Anthropic)
- **CLI channel** — interactive REPL with streaming output
- **Telegram channel** — bot with user/chat whitelisting
- **21 builtin tools** — bash, filesystem, web fetch, tape operations
- **Profile system** — `eli login`, `eli use`, `eli status`
- **Tape storage** — file-based and in-memory stores
