# 2026-06-05 · Real-model A/B verifies a cache change (and catches net-negative risk)

## Context

Adding a 3rd Anthropic prompt-cache breakpoint to cache conversation history
(token P0 from the arch/token audit). The static estimate said "#1 token lever,
~0.1x history re-reads." But prompt caching has an asymmetric cost model
(write 1.25x, read 0.1x), so a *mis-placed* breakpoint — e.g. on the ephemeral
tail reminder that changes every round — is **net-negative** (pay 1.25x to write
a prefix that's never re-read). The adversarial review flagged exactly this:
"误放=净负 1.25x write / 0 read; 一字节漂移=整轮 miss."

## What Worked

1. **The review's net-negative warning shaped the design, not just the code.**
   Instead of "mark the last message" (naive, self-invalidating), the fix anchors
   the *second-to-last* conversation message — correct-by-construction because
   `append_tail_reminder` always leaves the volatile content as the last message
   (merged on round 1, pushed afterwards). No index threading, no provider leak,
   no tool-loop change. Reading the actual `append_tail_reminder` /
   `split_system_and_conversation` code is what revealed this invariant.

2. **A real-model A/B settled the net-positive question that static reasoning
   could not.** Same prompt (read-3-files tool loop), `claude-sonnet-4-6`,
   `git stash` the change → rebuild → re-run:
   - full-price input **55,519 → 1,181 tokens**
   - `cache_hit_ratio` 0.74, cache read:write ≈ **3:1**
   The read:write ratio is the key signal: ~3:1 means each written segment is
   re-read ~3×, so the 1.25x write is amortized → net-positive. A ratio near 1:1
   would have meant re-writing every round (the net-negative trap). The static
   estimate could not have told me which regime I was in.

3. **Output-neutral by construction → no full quality-suite needed.** cache_control
   markers don't change generation; both A/B runs produced identical correct
   summaries. The unit tests + the functional real-model run cover it; the
   9-scenario model-comparison suite would have spent $ to confirm a near-certain
   no-op.

## Rule

For any prompt-cache / context-window / token change: **verify with a real-model
A/B and read `cache_read` vs `cache_write`, not just `cache_hit_ratio`.** A high
hit ratio with read:write ≈ 1:1 is re-writing every round (net-negative despite
looking cached); you want read:write ≥ ~2:1. The full-price `input_tokens` drop
is the headline, but the read:write ratio is what proves it's not a 1.25x-write
trap. Isolate the change with `git stash` + rebuild; the same-binary internal
ratio is robust even when model nondeterminism makes round counts differ between
the two runs. See [[2026-06-04-ground-truth-before-applying-research]].
