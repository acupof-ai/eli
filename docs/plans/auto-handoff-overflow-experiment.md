# Auto Handoff Overflow Experiment Plan

**Date**: 2026-04-29
**Status**: Proposed

---

## Goal

Validate automatic handoff when context approaches or exceeds the model limit,
including success-path usage signals, provider overflow errors, timeout errors,
grace-period behavior, and tape side effects.

The experiment should avoid real LLM calls first. Context overflow behavior is
deterministic inside `agent_run.rs`, so unit tests can cover most risk without
API cost or nondeterministic provider responses.

---

## How This Would Fail

1. A test only checks synthetic token counts and misses the actual error path
   used when providers reject an oversized request.
2. Error-string matching is too broad, so unrelated timeout text creates a
   handoff anchor.
3. Error-string matching is too narrow, so a provider overflow message bypasses
   handoff and leaves the tape stuck.
4. Grace-period decrement hides repeated overflow and delays a needed second
   handoff.
5. Repeated handoffs in the same second collide on anchor names.
6. Tests leak `ELI_HANDOFF_THRESHOLD_PCT` across cases and become order
   dependent.
7. No-usage providers never trigger handoff because the fallback estimate is
   too weak or ignores tool output.
8. Tape assertions only check "something was written" and miss wrong anchor
   state, wrong summary, or wrong previous-anchor selection.

---

## Existing Behavior Under Test

Core functions live in `crates/eli/src/builtin/agent/agent_run.rs`.

- `should_handoff` chooses the maximum reported input token count across usage
  rounds. If usage is absent or zero, it estimates tokens from text and tool
  results.
- `maybe_auto_handoff` handles successful text responses and writes an
  `auto-handoff/*` anchor when the threshold is reached.
- `maybe_auto_handoff_on_error` handles context overflow and timeout errors.
- `place_handoff_anchor` writes an anchor, a system summary, and a
  `auto-handoff.grace` event with `remaining = 2`.
- `resolve_tape_context_override` uses the previous anchor while grace remains.

---

## Experiment Layers

### Layer 1: Pure Decision Tests

Add focused unit tests beside the private helpers in `agent_run.rs`.

| Case | Setup | Expected |
|---|---|---|
| Below threshold | window 1000, pct 40, input 399 | no handoff |
| Exact threshold | window 1000, pct 40, input 400 | handoff |
| Above threshold | window 1000, pct 40, input 401 | handoff |
| Multi-round peak | usage 100, 450, 200 | handoff uses 450 |
| Multi-round below | usage 100, 399 | no handoff |
| Invalid pct | `ELI_HANDOFF_THRESHOLD_PCT=abc` | default 40 |
| Low pct clamp | pct 0 | clamp to 1 |
| High pct clamp | pct 200 | clamp to 99 |
| No usage, ASCII text | long ASCII response | estimate can trigger |
| No usage, CJK text | long CJK response | estimate can trigger |
| Tool result only | empty text, large tool JSON | estimate includes tools |
| Empty output | no usage, no text, no tools | no handoff |

Use an environment-variable guard helper so each test restores
`ELI_HANDOFF_THRESHOLD_PCT`.

### Layer 2: Error Classifier Tests

Cover known provider messages and false-positive candidates.

Messages that should trigger:

- `context_length_exceeded`
- `maximum context length`
- `prompt is too long`
- `input too long`
- `context window`
- `context length`
- `context_length`
- `tokens exceeds`
- `too many tokens`
- `context limit`
- `request too large`
- `sse_stream_error`
- `timed out`
- `timeout`

Messages that should not trigger:

- authentication failures
- rate limits
- validation errors unrelated to size
- ordinary tool errors

Open question: the bare `timeout` match is intentionally broad today. The
experiment should expose whether that creates unacceptable false positives.

### Layer 3: Tape Side-Effect Tests

Use the existing `make_tape_service` helper and inspect tape entries.

| Case | Expected tape state |
|---|---|
| Success below threshold | no `auto-handoff/*` anchor, no grace event |
| Success at threshold | one auto anchor, one summary entry, one grace event |
| Existing bootstrap anchor | grace `prev_anchor` is `session/start` |
| No previous anchor | grace `prev_anchor` is empty and no context override is used |
| Long response | stored summary is capped at 500 chars |
| Error overflow | auto anchor summary is the fixed overflow marker |
| Non-context error | no auto anchor |
| Non-text result | records error event but does not auto-handoff |

Assertions should check exact fields:

- anchor name prefix: `auto-handoff/`
- anchor state: `reason`, `input_tokens`, `context_window`, `summary`
- system entry starts with `[Context summary from auto-handoff]`
- grace payload: `remaining`, `prev_anchor`
- `agent.run` save event is still recorded before handoff handling

### Layer 4: Grace-Period Transition Tests

Build a small sequence over one tape.

| Sequence | Expected |
|---|---|
| handoff, below threshold turn | grace 2 -> 1, no new anchor |
| another below threshold turn | grace 1 -> 0, no active grace |
| handoff, above threshold during grace | grace decrements and a new handoff is written |
| handoff, overflow error during grace | writes a new auto handoff and moves grace to the previous auto anchor |
| active grace with prior anchor | next turn uses `AnchorSelector::Named(prev_anchor)` |
| active grace without prior anchor | no override |

Overflow during grace means the current fallback anchor is still too old. The
handoff point must move forward so the next turn sees a shorter slice.

### Layer 5: Collision And Repeatability Tests

Trigger two handoffs rapidly on one tape.

Expected result to decide:

- Either anchor names must be unique even inside one second.
- Or current second-granularity names are accepted and the test documents the
  overwrite/duplicate behavior.

This is a likely edge case because anchor names use
`Utc::now().format("%Y%m%dT%H%M%S")`.

### Layer 6: Optional End-to-End Fake Provider

Only add this after Layers 1-5 pass.

Use a fake local provider or test transport that returns controlled usage and
controlled overflow errors. The goal is to exercise the full
`agent_loop -> run_tools_once -> process_agent_result` path without real API
cost.

Scenarios:

1. Primary response succeeds with high usage.
2. Primary request fails with context overflow.
3. Primary fallback path succeeds after an initial overflow.
4. Primary and fallback both overflow, causing the error-path handoff.

Keep payloads small by setting a tiny context window and threshold in the test
environment. Do not use real provider credentials.

---

## Consequential Decisions

1. **Unit-first vs fake-provider-first**: Unit-first is cheaper and isolates the
   handoff state machine. Fake provider can follow once deterministic behavior
   is covered.
2. **Timeout breadth**: Bare `timeout` may be correct for SSE failures but can
   over-trigger. Decide whether to keep it broad or require provider/network
   context.
3. **Grace error behavior**: Error-path overflow during grace must force a new
   handoff immediately, matching the success path and advancing the next
   fallback anchor.
4. **Anchor uniqueness**: Decide whether second-granularity anchor names are
   acceptable. If not, add milliseconds or a monotonic suffix before writing
   collision tests.
5. **End-to-end boundary**: Decide whether a fake provider belongs in `eli`
   tests or in `nexil` transport tests. Keep prompt shape stable to avoid
   changing KV-cache behavior in runtime paths.

---

## Proposed Sequence

1. Add Layer 1 and Layer 2 tests in `agent_run.rs`.
2. Add Layer 3 and Layer 4 tape tests using the existing temp tape helpers.
3. Run:
   `cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings && cargo test -p eli agent_run`
4. Triage findings:
   - P0/P1: false negatives for overflow, broken grace, corrupted tape state.
   - P2: broad timeout false positives, anchor-name collision risk.
5. Add Layer 6 only if the unit/tape tests leave uncertainty about the full
   model execution path.

---

## Non-Goals

- Do not call real LLM APIs.
- Do not modify prompt construction unless a failing test proves it necessary.
- Do not change tape serialization format.
- Do not alter context budgeting in `nexil` during this experiment.
