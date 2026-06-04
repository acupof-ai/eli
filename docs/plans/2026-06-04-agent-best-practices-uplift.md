# 2026-06-04 · Agent Best-Practices Uplift (eval-driven)

Source: multi-agent research workflow (`wf_fbfa2c2f-07c`, 14 agents, ~978k tokens) —
2024-2026 agent best-practice evolution mapped against the eli/nexil codebase.

## Method

Eval-driven development: for each item we (1) write an eval that captures the
capability and records a **baseline** (pre-change number), (2) implement, (3)
re-run the same eval and record the **after** number. Deterministic items get
Rust unit tests; capability items get Python integration tests against the real API.

## HARD CONSTRAINT — preserve the infinite-context design

eli is an **infinite-context** system: the append-only **tape** (`nexil/tape/store.rs`)
is the unbounded source of truth; `apply_context_budget()`/`aggressive_trim()` in
`nexil/tape/context.rs` only produces a **lossy VIEW** over it (it mutates the
per-request `Vec<Value>`, never the tape). Full history is always recoverable via
`tape.search` / spill refs. **Every optimization must operate at the view/projection
layer or be purely additive — never delete, cap, or make past tape content
unrecoverable.** This re-scopes #6 (view-layer summary, not lossy trim), #7 (concise
view must still spill full output to disk), and #13 (spill GC conflicts — redefine).

## What eli already does well (do NOT re-build)

- Append-only **tape** with anchoring/forking (durable event log)
- Clean **14-point hook** system, last-registered-wins, panic isolation, ArcSwap hot path
- **dot-namespaced tools**, concurrent-by-name execution, circuit-breaker + metrics middleware
- **SKILL.md** with project>global>builtin precedence + frontmatter validation
- **Decisions** persisted in tape, survive trimming, injected every turn, revocation tombstones
- **CJK-aware** char budgeting, spill-to-disk for large outputs
- **Cache-friendly system-prompt layout** (volatile date in trailing Runtime section)
- nexil: provider-agnostic transport (Completion/Messages/Responses), streaming delta assembly,
  250-iter tool loop, OAuth single-flight, 7-kind error taxonomy

## Roadmap (best-first, by impact / (effort+risk))

### Tier 1 — cheap, high-ROI, low-risk, reversible

1. **Fix `MODEL_TOOLS_CACHE` staleness** — `OnceLock` populated once, never invalidated;
   plugin tools registered post-init silently vanish. Replace with `ArcSwap<Vec<Tool>>` or
   len/version-compare rebuild. *(impact H / effort L / risk L)*
   Files: `crates/eli/src/tools.rs`, `crates/eli/src/builtin/tools.rs`
   Eval: Rust unit test — register tool post-init, assert it appears in `model_tools_cached()`.

2. **Prompt caching (`cache_control`)** — nexil emits no cache breakpoints, so the already
   cache-friendly prefix re-pays full input cost every turn (~10x). Add `ephemeral` breakpoints
   on last system block + end of tool-schema array (Anthropic default-on, builder-gated). Surface
   cache-read/write tokens via `UsageEvent`. *(impact H / effort M / risk L)*
   Files: `crates/nexil/src/core/request_builder.rs`, `crates/nexil/src/providers/anthropic.rs`, `crates/nexil/src/core/results.rs`
   Eval: integration — fixed multi-turn convo, assert cache-read tokens > 0 and input cost drops on turn 2+.

3. **Pre-call budget enforcement (cost circuit breaker)** — `BudgetLedger.try_spend()` exists but
   only records after the fact; a runaway tool loop blows budget with no gate. Wire pre-call
   `try_spend(estimate)` into run_model; on reject terminate cleanly with `BudgetExceeded`.
   `ELI_MAX_TURN_BUDGET` config, default unlimited. *(impact H / effort L / risk L)*
   Files: `crates/eli/src/control_plane.rs`, `crates/eli/src/framework.rs`, `crates/nexil/src/llm/tool_loop.rs`, `crates/eli/src/builtin/settings.rs`
   Eval: Rust unit test — loop with tiny budget stops at threshold.

4. **Backoff jitter + transient/persistent classification** — backoff is `0/1/2/4/8s` with no
   jitter; concurrent sessions retry in lockstep (real, given documented prolite-429→anthropic
   failover). Add full jitter; only retry transient kinds. *(impact M / effort L / risk L)*
   Files: `crates/nexil/src/core/execution.rs`
   Eval: Rust unit test — backoff samples spread within `[0, base*2^n]`; persistent kind not retried.

5. **Structured tool-result signal (`isError`-style)** — results are bare JSON/error; caller can't
   tell empty-success from silent-failure; `ErrorKind::Tool` is a catch-all. Add status enum
   `Ok|Empty|Error`, split ErrorKind, echo failing args + fix suggestion into the model-facing
   tool_result. *(impact H / effort M / risk L)*
   Files: `crates/nexil/src/core/results.rs`, `crates/nexil/src/core/errors.rs`, `crates/nexil/src/llm/tool_loop.rs`, `crates/eli/src/builtin/tools.rs`
   Eval: integration — induce a tool error, assert recovery in fewer turns; unit test on status mapping.

### Tier 2 — context engineering (medium-high effort)

6. **Compaction/summarization at the budget** — `aggressive_trim()` keeps only last 2 user rounds
   and hard-drops the rest *from the view* (one-line `TRIM_NOTICE`). **Infinite-context-safe framing:**
   the tape already keeps everything; improve only the *projection* — when the view exceeds budget,
   synthesize the to-be-dropped span (objective, decisions, modified paths, outcomes, open Qs) into a
   summary message injected into the view, and persist it as an **additive** `TapeEntryKind::Summary`
   for cheap re-projection. Originals stay on the tape, `tape.search`-able. Opt-in, hard-trim fallback.
   *(impact H / effort M / risk M)*
   Files: `crates/nexil/src/tape/context.rs`, `crates/nexil/src/tape/entries.rs`, `crates/nexil/src/llm/tool_loop.rs`, `crates/eli/src/builtin/tape.rs`
   Eval: integration — long convo past budget, assert a turn-1 fact is recalled at turn N.

7. **Per-tool `response_format` (concise|detailed) + tighter default caps** — no agent-controllable
   verbosity. Add enum to high-volume tools (tape.search, fs.read, web.fetch, bash), default concise,
   steering truncation message. **Infinite-context-safe:** concise trims only the *view* — full output
   must still spill to disk (recoverable), as the existing spill system already does. *(impact M / effort M / risk L)*
   Files: `crates/eli/src/builtin/tools.rs`, `crates/nexil/src/tape/spill.rs`

8. **File-backed memory tool + recitation** — only persistent memory is plain-text Decisions. Add
   `memory.*` over sandboxed `{workspace}/.eli/memory`, "check memory at start" instruction, and a
   recitation slot surfacing active todo/progress near prompt tail. *(impact H / effort H / risk M)*
   Files: `crates/eli/src/builtin/tools.rs`, `crates/eli/src/prompt_builder.rs`, `crates/nexil/src/llm/decisions.rs`, `crates/nexil/src/tape/entries.rs`

### Tier 3 — loop & safety (higher effort/risk)

9. **Verify/self-correct phase (Reflexion, hard-signal-gated)** — model runs once/turn, recovery
   nudge capped at 1. Add opt-in verify stage running a per-skill `verify` command (cargo test/clippy)
   and feeding failures back for a bounded correction loop. *(impact H / effort H / risk M)*
   Files: `crates/eli/src/framework.rs`, `crates/eli/src/hooks.rs`, `crates/eli/src/skills.rs`, `crates/eli/src/builtin/agent/agent_run.rs`

10. **Tool behavior annotations (read-only/destructive) + plan-mode gate** — all tools look equal;
    no per-call approval point. Add `read_only`/`destructive` to Tool, a pre-exec policy gate enabling
    a real enforced plan/read-only mode. *(impact M / effort M / risk L)*
    Files: `crates/nexil/src/tools/schema.rs`, `crates/eli/src/builtin/tools.rs`, `crates/eli/src/tool_middleware.rs`, `crates/eli/src/hooks.rs`

11. **Structured per-step observability (OTel GenAI)** — no per-phase token/cost spans. Start with a
    per-turn structured summary (tokens in/out, cache r/w, tool counts/durations) on the tape; later
    the tracing-opentelemetry bridge. *(impact M / effort M / risk L)*

12. **Deferred/retrieval tool loading** — 40 builtin tools loaded unconditionally (>30 hurts accuracy).
    Keep a small core + `tools.search` meta-tool injecting top-k schemas. *(impact M / effort H / risk M)*

13. **Spill GC / lifecycle** — ⚠️ **CONFLICTS with infinite-context** (spills ARE the recoverable
    durable store; deleting referenced content breaks `tape.search` recovery). **REDEFINE:** never
    delete referenced content — only *compress/archive* spills in place (e.g. gzip cold spills), or GC
    only spills whose owning tape entries were themselves removed (which never happens in append-only).
    Likely the lowest priority / may be dropped. *(impact L / effort L / risk L)*

## Deliberately NOT recommended (near-term)

- **Multi-agent orchestration** inside eli: its channel-driven, coherence-critical workload matches
  the single-thread "don't build multi-agents" posture; tape + sub-agent tools already cover read-heavy
  fan-out. (Workflow-level orchestration like this research run stays at the tooling layer.)
