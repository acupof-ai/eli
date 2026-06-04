# 2026-06-04 · Ground-truth the code before applying research recommendations

## Context

Applied a 13-item agent best-practices roadmap (from a multi-agent research
workflow) to eli/nexil, eval-driven. The roadmap was produced by sub-agents that
explored the codebase but didn't have the full picture. Shipped 11/13 to main.

## What Worked

1. **Ground-truth every recommendation against the live code before building.**
   Reading the actual code first repeatedly showed eli *already had* the machinery
   the synthesis treated as missing — and the real gap was smaller and cleaner:
   - #6 "no compaction" → eli already auto-handoffs (anchor + injected summary
     System message that survives slicing); the only gap was the summary being a
     500-char prefix instead of an LLM distillation. Fix = upgrade the stub, reuse
     everything else.
   - #7 "add response_format enum to every tool" → eli already had fs.read
     limit/offset, bash spill, tape.search limit. The real gap was web.fetch
     lacking spill. Fix = generalize the bash-spill helper, apply to web.fetch.
   - #8 "memory.* file tool + recitation" → Decisions already cover persistent
     notes (redundant); the only non-redundant piece was tail recitation of the
     existing taskboard.
   Result: smaller, more elegant diffs that *reuse* existing systems instead of
   bolting on parallel ones. Each scope-down was surfaced to the user explicitly.

2. **Eval-driven before/after per item.** Write the test that captures the
   capability first (red for bug fixes, or asserts the new contract), implement,
   re-run. Every change shipped with a concrete before→after, not vibes.

3. **Default-off for every risky/cross-cutting feature** (prompt-cache opt-out,
   budget breaker, compaction, plan mode, verify loop). Existing tests exercise
   the off path, so regression risk to normal operation is ~zero — which made it
   safe to land invasive changes (verify loop in the core agent loop) late.

4. **Factor shared logic while touching it.** Adding the budget stop to the tool
   loop, I found 3 near-identical "finish with text" blocks and extracted one
   `text_result()` helper — the change became a net simplification.

5. **Respect the load-bearing invariant.** The user flagged "infinite context"
   (tape = unbounded source of truth, window = lossy view). Every optimization
   stayed view-layer or additive; this re-scoped #6 (view-layer summary) and
   killed #13 (spill GC would delete the recoverable store). See
   [[project_eli_infinite_context]].

## Rule

Before applying any externally-sourced recommendation (research, sub-agent,
review), read the live code it targets. Half the time the system already has 80%
of the machinery — the right change is to find the genuinely-missing 20% and
reuse the rest, not to build the recommendation verbatim. Surface every scope-down
to the user with the reason. Ship each change with a before/after eval, and keep
cross-cutting features default-off so the off path (all existing tests) proves no
regression.
