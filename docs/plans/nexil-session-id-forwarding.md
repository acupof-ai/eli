# Plan: Forward `session_id` through nexil to OpenAI-compatible inference servers

**Owner:** ckl · **Date:** 2026-05-03 · **Status:** proposed
**Track:** B (G4 follow-through to agent-infer agent-workload-api §3.4 / §3.5)

## 1. Goal & acceptance gates

Make Eli's session identity reach the upstream inference server as a typed
`session_id` field on every chat / tool-loop request, so agent-infer can do
sticky routing for KV-prefix reuse.

**Functional gate**
- `scripts/bench_eli_agent.sh smoke-real` (in `agent-infer`) reports
  `session_affinity_hit > 0` on a multi-turn replay of one conversation.
  Today that counter is `0` because nexil never sends `session_id`.
  The infer-side counter is described in
  [`agent-infer/docs/experience/wins/2026-05-02-bench-agent-load-a1-session-affinity-admission.md`](../../../agent-infer/docs/experience/wins/2026-05-02-bench-agent-load-a1-session-affinity-admission.md).

**Backwards-compat gate**
- A bare `cargo run --example basic_chat -- "..."` against stock OpenAI
  produces request bytes identical to today (no `session_id` key) — verified
  by snapshotting `LLMCore::build_completion_body` with
  `ChatRequest { session_id: None, .. }`.
- Anthropic adapter: when `session_id = None`, the body is byte-identical to
  today. When set, the only added field is `metadata.user_id` —
  Anthropic's own published field, so it cannot 400.

**Coverage gate**
- New nexil unit test asserts the openai adapter inserts a top-level
  `session_id` matching the field name parsed at
  `agent-infer/infer/src/http_server/openai_v1.rs:574`, `:838`, `:1142`
  (all three: `/v1/completions`, `/v1/chat/completions`, `/v1/responses`).
- New nexil unit test asserts the anthropic adapter maps `session_id`
  to `metadata.user_id` and never to a top-level `session_id`
  (Anthropic's `/v1/messages` would 400 — `deny_unknown_fields` analogue).

---

## 2. Current state evidence

Track B's peer Claude on tmux 6:1 grep'd nexil and confirmed:

- **`nexil::ChatRequest`** has no `session_id` field —
  [`crates/nexil/src/llm/mod.rs:60-83`](../../crates/nexil/src/llm/mod.rs).
  The struct is `Default`-derivable, lifetime-parameterised over `'a`,
  with all fields optional. Every other request-level hint (model,
  provider, tools, tape, cancellation, context_window) lives here.

- **`TransportCallRequest`** has no `session_id` field —
  [`crates/nexil/src/core/request_builder.rs:165-178`](../../crates/nexil/src/core/request_builder.rs).
  This is the per-attempt struct adapters consume.

- **OpenAI adapter** assembles the body without ever consulting a session
  field — [`crates/nexil/src/providers/openai.rs:40-75`](../../crates/nexil/src/providers/openai.rs).
  It does loop `for (key, value) in kwargs` at `:71-73`, which is what
  Path A (band-aid) would exploit.

- **Anthropic adapter** likewise has no session handling — only `kwargs`
  pass-through at [`crates/nexil/src/providers/anthropic.rs:57-62`](../../crates/nexil/src/providers/anthropic.rs).

- **Eli's `Agent::run`** receives `session_id: &str`, derives a tape name
  from it via `TapeService::session_tape_name(session_id, &workspace)`,
  and **drops the value on the floor** before calling `agent_loop` —
  [`crates/eli/src/builtin/agent/mod.rs:74-117`](../../crates/eli/src/builtin/agent/mod.rs).

- **`agent_loop`** signature already takes 10 arguments and never sees
  `session_id` —
  [`crates/eli/src/builtin/agent/agent_run.rs:537-585`](../../crates/eli/src/builtin/agent/agent_run.rs).
  It calls `run_tools_once` ([`agent_request.rs:253-321`](../../crates/eli/src/builtin/agent/agent_request.rs))
  which constructs the `ChatRequest` literal at `:305-316`.

**Upstream confirmation: infer parses `session_id` as a top-level field.**
[`agent-infer/infer/src/http_server/openai_v1.rs:574`](../../../agent-infer/infer/src/http_server/openai_v1.rs)
defines on `CompletionRequest`:
```rust
#[serde(default, alias = "user")]
pub(super) session_id: Option<String>,
```
The same definition is repeated for `ChatCompletionRequest` (`:838`) and
`ResponsesRequest` (`:1142`). Normalisation lives in
`fn normalize_session_id` at `:14`. So:

- The wire field nexil must emit is the **top-level JSON key `session_id`**,
  string-typed, omitted when null.
- Infer also accepts `user` as an alias, but emitting `user` would collide
  with downstream OpenAI semantics if a user ever points nexil at real
  OpenAI. Stick with `session_id`.

**Read-side signal already exists in Eli.** [`framework.rs:303`](../../crates/eli/src/framework.rs)
already serializes `session_id` into outbound greeting envelopes; the
value is plumbed end-to-end through the turn pipeline (see
[`framework.rs:156-219`](../../crates/eli/src/framework.rs)). The hop
that doesn't carry it is the Eli→nexil call boundary.

The downstream-impact audit:

- `ChatRequest` is constructed in 8 places — all listed in §5.
- `ChatRequest` is exported from the public crate root —
  [`crates/nexil/src/lib.rs:43`](../../crates/nexil/src/lib.rs).
- nexil is a published crate (`name = "nexil"`, `version = "0.8.0"`,
  `repository = "https://github.com/cklxx/eli"` in
  [`crates/nexil/Cargo.toml:2-7`](../../crates/nexil/Cargo.toml)). See §8
  for ABI risk.

---

## 3. Proposed API surface

### 3.1 `ChatRequest` — add one field

```diff
 #[derive(Default)]
 pub struct ChatRequest<'a> {
     pub prompt: Option<&'a str>,
     pub user_content: Option<Vec<Value>>,
     pub system_prompt: Option<&'a str>,
     pub model: Option<&'a str>,
     pub provider: Option<&'a str>,
     pub messages: Option<Vec<Value>>,
     pub max_tokens: Option<u32>,
     pub tools: Option<&'a ToolSet>,
     pub tool_context: Option<&'a ToolContext>,
     pub tape: Option<&'a str>,
     pub tape_context: Option<&'a TapeContext>,
     pub cancellation: Option<CancellationToken>,
     pub context_window: Option<usize>,
     pub max_tool_iterations: Option<usize>,
+    /// Opaque routing hint forwarded to the upstream provider. The OpenAI
+    /// adapter inserts this as the top-level `session_id` field; the
+    /// Anthropic adapter maps it to `metadata.user_id`. Other adapters
+    /// drop it silently. `None` keeps the request body byte-identical to
+    /// today.
+    pub session_id: Option<&'a str>,
 }
```

**Decision: `Option<&'a str>`, not `Option<String>`.** ChatRequest is
already lifetime-parameterised; every other string field uses borrowed
references; the value flows through `Agent::run(session_id: &str, ...)`
([`agent/mod.rs:77`](../../crates/eli/src/builtin/agent/mod.rs)) which
already binds the lifetime. Cloning would be cargo-cult.

### 3.2 `TransportCallRequest` — add one field

```diff
 #[derive(Debug, Clone)]
 pub struct TransportCallRequest {
     pub client: Arc<Client>,
     pub provider_name: String,
     pub model_id: String,
     pub api_base: Option<String>,
     pub messages_payload: Vec<Value>,
     pub tools_payload: Option<Vec<Value>>,
     pub max_tokens: Option<u32>,
     pub stream: bool,
     pub reasoning_effort: Option<Value>,
     pub kwargs: serde_json::Map<String, Value>,
     pub is_anthropic_oauth: bool,
+    /// Routing hint to forward to the provider when the adapter knows
+    /// how to map it. See `OpenAIAdapter`/`AnthropicAdapter`.
+    pub session_id: Option<String>,
 }
```

`TransportCallRequest` is `Clone` and owned (it's recreated per retry
inside `LLMCore::run_chat`'s loop at
[`execution.rs:601`](../../crates/nexil/src/core/execution.rs)), so this
field is `Option<String>`.

### 3.3 `LLMCore::run_chat` / `run_chat_stream` signatures

Both already carry `#[allow(clippy::too_many_arguments)]`
([`execution.rs:573`, `:732`](../../crates/nexil/src/core/execution.rs)).
Add `session_id: Option<&str>` immediately after `kwargs`:

```rust
pub async fn run_chat<T, F>(
    &mut self,
    messages_payload: Vec<Value>,
    tools_payload: Option<Vec<Value>>,
    model: Option<&str>,
    provider: Option<&str>,
    max_tokens: Option<u32>,
    stream: bool,
    reasoning_effort: Option<Value>,
    kwargs: serde_json::Map<String, Value>,
    session_id: Option<&str>,           // ← new
    on_response: F,
) -> Result<T, ConduitError>
```

Same insertion in `run_chat_stream`. `session_id` is plumbed into
`prepare_attempt` → `build_transport_request` → `TransportCallRequest`
at [`execution.rs:362-388`](../../crates/nexil/src/core/execution.rs)
(adding one parameter to `build_transport_request`).

`run_tools` and `run_tools_stream` are *not* methods on `LLMCore`; they
live on `LLM` ([`llm/tool_loop.rs:106`](../../crates/nexil/src/llm/tool_loop.rs))
and read fields off `ChatRequest`. They need to thread `session_id` from
the destructured `ChatRequest` into each `self.core.run_chat(...)` call
(four sites, listed in §5).

### 3.4 Adapter contract

Documented on `TransportCallRequest::session_id`: each adapter is
responsible for translating the hint into its provider's wire shape.
Adapters that do not recognise it MUST drop it silently — the
abstraction is a **hint**, not a contract.

---

## 4. Adapter behavior

### 4.1 OpenAI (`crates/nexil/src/providers/openai.rs`)

`build_completion_body` ([`openai.rs:40-75`](../../crates/nexil/src/providers/openai.rs)):
after the `kwargs` loop at `:71`, insert:

```rust
if let Some(ref sid) = request.session_id {
    body.entry("session_id".to_owned())
        .or_insert_with(|| Value::String(sid.clone()));
}
```

Use `entry().or_insert_with` so that an explicit `kwargs["session_id"]`
override (rare but possible for tests) wins — same precedence rule the
adapter already uses for every other kwarg.

`build_responses_body` ([`openai.rs:77-123`](../../crates/nexil/src/providers/openai.rs)):
identical insertion before the final `Value::Object(body)` return.
Confirmed wire-shape against
[`agent-infer/.../openai_v1.rs:1142`](../../../agent-infer/infer/src/http_server/openai_v1.rs)
(`ResponsesRequest` accepts the same top-level `session_id`).

**Why top-level, not nested under `extra_body`:** infer's
`ChatCompletionRequest` is `#[serde(deny_unknown_fields)]`
([`openai_v1.rs:583`](../../../agent-infer/infer/src/http_server/openai_v1.rs))
and explicitly opts `session_id` into the surface. There is no
`extra_body` parser. This is precisely the discipline
[`agent-infer/docs/plans/agent-workload-api.md:71-74`](../../../agent-infer/docs/plans/agent-workload-api.md)
G7 demands: "Each P0/P1 below either adds a field with `#[serde(default)]`
or relaxes `deny_unknown_fields` — pick one canonical path." Infer chose
the typed `#[serde(default)]` path; nexil must match it.

### 4.2 Anthropic (`crates/nexil/src/providers/anthropic.rs`)

Anthropic's `/v1/messages` accepts a published `metadata.user_id` field
(the only stable opaque-identity hop they expose). After the kwargs
pass-through at [`anthropic.rs:57-62`](../../crates/nexil/src/providers/anthropic.rs):

```rust
if let Some(ref sid) = request.session_id
    && !body.contains_key("metadata")
{
    body.insert(
        "metadata".to_owned(),
        serde_json::json!({ "user_id": sid }),
    );
}
```

Guard on `!body.contains_key("metadata")` so a caller-provided
`kwargs["metadata"]` wins (consistent with the existing `entry().or_insert`
pattern in the openai adapter). Do not set top-level `session_id` —
Anthropic would 400.

### 4.3 Other adapters

There are exactly two provider adapters in the tree:
[`crates/nexil/src/providers/`](../../crates/nexil/src/providers/) →
`openai.rs`, `anthropic.rs`, `mod.rs`. No silent-drop branch needed —
either adapter handles it explicitly.

---

## 5. Eli call-site changes

### 5.1 `Agent::run` → `agent_loop` → `run_tools_once`

Thread `session_id` end-to-end. Five touch points in `eli`:

| # | File:line | Change |
|---|---|---|
| 1 | `crates/eli/src/builtin/agent/mod.rs:104-116` | Pass `session_id` as a new arg to `agent_loop`. |
| 2 | `crates/eli/src/builtin/agent/agent_run.rs:536-548` | Add `session_id: &str` to `agent_loop` signature, pass to `run_tools_once`. |
| 3 | `crates/eli/src/builtin/agent/agent_request.rs:252-321` | Add `session_id: &str` to `run_tools_once` signature; set `session_id: Some(session_id)` on the `ChatRequest` literal at `:305-316`. |
| 4 | `crates/eli/src/builtin/subagent/fallback.rs` | Subagent fallback also calls `agent_loop` (per [`fallback.rs:1`](../../crates/eli/src/builtin/subagent/fallback.rs)); thread the parent session_id through here too — confirms the new signature isn't quietly bypassed by the subagent path. |
| 5 | `crates/nexil/src/tape/session.rs:34-44` | `TapeSession::chat` accepts `mut req: ChatRequest`; nothing required if the field stays at default `None`, but worth a one-line note in the docstring that `session_id` is forwarded as-is. |

### 5.2 nexil-internal call sites of `LLMCore::run_chat[_stream]`

Each must be updated to pass `session_id` (read off the `ChatRequest`
when one exists, `None` for the lower-level chat-client API):

| File:line | Source of `session_id` |
|---|---|
| `crates/nexil/src/llm/mod.rs:344` (`chat_async`) | destructure `session_id` from `ChatRequest` (already destructures other fields at `:311-321`) |
| `crates/nexil/src/llm/stream.rs:76` (`stream`) | destructure from `ChatRequest` at `:19-30` |
| `crates/nexil/src/llm/tool_loop.rs:87` (`tool_calls`) | destructure from `ChatRequest` at `:64-74` |
| `crates/nexil/src/llm/tool_loop.rs:386` (`_execute_tool_round`) | thread through `RoundParams` at `:28-35` |
| `crates/nexil/src/llm/tool_loop.rs:427` (retry on empty) | same `RoundParams` value |
| `crates/nexil/src/clients/chat.rs:639, :689, :736, :799` | low-level chat client API has no `ChatRequest` — pass `None`. These are not the hot path; documented as "session-unaware client". |

`RoundParams` ([`tool_loop.rs:28-35`](../../crates/nexil/src/llm/tool_loop.rs))
gains one field: `pub session_id: Option<&'a str>`. Lifetime is already
`'a` so nothing else changes.

---

## 6. Test strategy (specifications, not implementations)

### 6.1 nexil unit tests

Co-locate with existing adapter tests (the pattern at
[`execution.rs:1000-1039`](../../crates/nexil/src/core/execution.rs)
constructs a `TransportCallRequest` literal and asserts on the JSON):

1. `test_openai_completion_body_includes_session_id_when_set`
   - Build `TransportCallRequest { session_id: Some("sess-42".into()), .. }`
   - Call `OpenAIAdapter.build_request_body(&req, TransportKind::Completion)`
   - Assert `body["session_id"] == "sess-42"`.

2. `test_openai_completion_body_omits_session_id_when_none`
   - Same as above with `session_id: None`.
   - Assert `body.get("session_id").is_none()`.
   - Snapshot the entire body bytes, equal to today's output → backwards-compat gate.

3. `test_openai_responses_body_includes_session_id_when_set` — same as
   (1) for `TransportKind::Responses`.

4. `test_anthropic_messages_body_maps_session_id_to_metadata_user_id`
   - Build with `session_id: Some("sess-42".into())`.
   - Call `AnthropicAdapter.build_request_body(&req, TransportKind::Messages)`.
   - Assert `body.get("session_id").is_none()`.
   - Assert `body["metadata"]["user_id"] == "sess-42"`.

5. `test_anthropic_metadata_kwargs_takes_precedence_over_session_id`
   - `kwargs["metadata"] = { "user_id": "from-kwargs" }`, `session_id: Some("sess-42")`.
   - Assert `body["metadata"]["user_id"] == "from-kwargs"` (caller wins,
     mirrors openai entry-or-insert semantics).

6. `test_chat_request_default_has_no_session_id`
   - `ChatRequest::default().session_id == None`. One-liner; guards
     against future field-renames silently breaking the public API.

### 6.2 eli integration test (Python)

Add `tests/test_session_affinity.py` per the new-feature rule in
[`CLAUDE.md`](../../CLAUDE.md#integration-tests-python):

- Spin up a local `agent-infer` (or skip if `AGENT_INFER_URL` unset).
- Configure `eli use agent-infer`.
- Issue two `eli run --session sess-test "..."` turns with the same
  session id.
- `curl ${AGENT_INFER_URL}/v1/stats?format=json` and assert
  `sessions["sess-test"].session_affinity_hit > 0`.
- Skip cleanly when prerequisites are missing — same fuzzy / skip
  pattern the vision tests use.

Do **not** write the tests in this plan — the spec above is the contract.

### 6.3 Manual verification (the named smoke gate)

`cd ~/code/agent-infer && ./scripts/bench_eli_agent.sh smoke-real`. Pre-change
baseline (already on disk in
[`agent-infer/docs/experience/wins/2026-05-02-bench-agent-load-w3-harness-pending.md`](../../../agent-infer/docs/experience/wins/2026-05-02-bench-agent-load-w3-harness-pending.md)):
`session_affinity_hit: pending-remote / 0`. Post-change: `> 0`.

---

## 7. Phased implementation

This is **one commit**. The API change must land coherently — splitting
struct-extension from call-site-update would leave the workspace in a
"new field exists, never populated" state where `cargo clippy` warns on
unused field on the nexil side and the integration test silently
regresses to all-`None`.

### Commit: `feat(nexil): forward session_id to provider adapters`

Files (8):
- `crates/nexil/src/llm/mod.rs` — add `session_id` to `ChatRequest`; thread through `chat_async`.
- `crates/nexil/src/llm/stream.rs` — destructure + thread.
- `crates/nexil/src/llm/tool_loop.rs` — destructure + thread (3 sites + `RoundParams`).
- `crates/nexil/src/core/request_builder.rs` — add field to `TransportCallRequest`.
- `crates/nexil/src/core/execution.rs` — add param to `run_chat` / `run_chat_stream`, plumb into `build_transport_request`.
- `crates/nexil/src/clients/chat.rs` — pass `None` at 4 sites (low-level API stays session-unaware).
- `crates/nexil/src/providers/openai.rs` — emit top-level `session_id`.
- `crates/nexil/src/providers/anthropic.rs` — emit `metadata.user_id`.
- `crates/eli/src/builtin/agent/mod.rs`, `agent_run.rs`, `agent_request.rs` — thread `session_id` from `Agent::run` to `ChatRequest`.
- `crates/eli/src/builtin/subagent/fallback.rs` — same.
- `crates/nexil/src/llm/tests.rs` (or co-located in `providers/openai.rs` and `providers/anthropic.rs` per existing house style) — six unit tests from §6.1.

Verify steps (from [`AGENTS.md §1.6`](../../AGENTS.md)):
```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

Per [`CLAUDE.md` Integration Tests](../../CLAUDE.md#integration-tests-python),
follow up with the Python integration test in a separate commit once
agent-infer is confirmed reachable in the test env.

---

## 8. Risk register

| # | Risk | Likelihood | Mitigation |
|---|------|------------|------------|
| R1 | Anthropic 400 on `metadata.user_id` shape — Anthropic actually wants `metadata: {"user_id": "<hash>"}` per their docs and may reject other shapes. | Low | Use the documented exact shape (verified against the public Anthropic API ref). Unit test 4 pins it. If Anthropic ever rejects, fall back to dropping silently — cost is one extra unit-test branch. |
| R2 | nexil is published to crates.io (`name = "nexil"`, `version = "0.8.0"` per [`Cargo.toml:2-3`](../../crates/nexil/Cargo.toml)) → adding a public field to `ChatRequest` is a **non-breaking change** because the struct is `#[derive(Default)]` and all existing fields stay typed-the-same. Adding a public field to `TransportCallRequest` is similarly additive. | Low | Bump to `0.9.0` per semver "additive public API". The `Default` derive guarantees existing `ChatRequest { ..Default::default() }` constructions keep compiling. |
| R3 | Downstream users that constructed `ChatRequest` with the struct-update syntax (`ChatRequest { prompt: ..., model: ..., ... }`) without `..Default::default()` would fail to compile. | Low | Audit shows exactly two such sites outside `nexil`: [`crates/eli/src/builtin/agent/agent_request.rs:305-316`](../../crates/eli/src/builtin/agent/agent_request.rs) (uses `..Default::default()` — safe) and the two `nexil/examples/*.rs` (uses `..Default::default()` — safe). External crates: unknown but the published examples in the README all use `..Default::default()`. Worst case: a `0.9.0` minor bump is justified. |
| R4 | `TransportCallRequest` is `pub` and re-exported. Adding a field requires every constructor to update. | Medium | Audit (`grep TransportCallRequest`): exactly two construction sites — [`execution.rs:375-388`](../../crates/nexil/src/core/execution.rs) and the test at `:1002-1027`. Both internal. The `Debug, Clone` derive carries through. |
| R5 | Path-A leakage: someone in the future adds `session_id` to `kwargs` directly, double-emitting it. | Low | The `body.entry("session_id").or_insert_with(...)` pattern in §4.1 ensures kwargs win; the test in §6.1.5 pins the precedence. |
| R6 | Non-OpenAI compatible servers that don't recognise `session_id` may 400 if they `deny_unknown_fields`. | Low | Real OpenAI, Anthropic, Ollama, vLLM, LM Studio all accept-or-ignore unknown request fields. agent-infer is the one that rejects unknown fields *and* the one we're targeting. The risk is theoretical for now. If discovered: gate the emission on a per-provider policy in [`provider_policies.rs`](../../crates/nexil/src/core/provider_policies.rs). |
| R7 | Anthropic OAuth Claude.ai backend strips unknown `metadata` keys — already our [`anthropic.rs:58-60`](../../crates/nexil/src/providers/anthropic.rs) drops `temperature` for OAuth. | Low | Mirror that pattern: `if request.is_anthropic_oauth { skip metadata.user_id }`. One extra branch. |

**Path A vs Path B recommendation reconfirmed:** Path B is the right call.
The combined evidence — nexil is a published crate but the only
construction sites of `ChatRequest` use `..Default::default()`, the
agent-infer plan §3.4 / §3.5 will need this same boundary again for
`cache_strategy` and `agent.kind`, and Path A's string-typed back-channel
fails Anthropic — makes the API change cheap and the band-aid expensive.

---

## 9. Open questions

1. **Should the openai adapter also emit `session_id` for stock OpenAI
   requests?** OpenAI's `user` field is similar (anti-abuse hint) but
   semantically different (per-end-user, not per-conversation). Today's
   answer: yes, emit `session_id` always when present; stock OpenAI
   ignores unknown fields. If a future user complains the field leaks
   to OpenAI, gate it on a `provider_policies.rs` flag.
2. **Anthropic `metadata.user_id` cardinality limit:** Anthropic docs
   suggest `user_id` should be stable per end-user (hash of email).
   `session_id` is per-conversation, which means we may emit a unique
   value per turn-thread. Anthropic has not historically rate-limited
   on this, but worth flagging.
3. **Other backends:** are there any session-aware backends besides
   openai-compatible and anthropic-messages? Audit suggests no, but
   the question becomes load-bearing the moment we add a third
   adapter (e.g. Gemini, Bedrock).
4. **Response correlation:** does Eli want to assert the `session_id`
   we sent matches the `session_id` infer echoes back in `/v1/stats`?
   The current plan doesn't read it back — observation is via
   `session_affinity_hit` aggregate counter only. Worth a follow-up
   plan if we ever need per-request prefix-cache-hit telemetry.
