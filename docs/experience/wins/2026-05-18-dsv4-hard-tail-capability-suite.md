# 2026-05-18 · DSV4 hard-tail capability E2E snapshot

## Context
The DeepSeek V4 provider path now exists in Eli, so the next check was not only
wire compatibility. The user asked for an end-to-end score across code,
planning, and tool-calling ability, using the hardest tail of mainstream
benchmarks as the shape of the local test collection.

## Suite
- Cases: `tests/benchmarks/dsv4_hard_tail_cases.json`
- Runner: `scripts/run_dsv4_capability_suite.py`
- Snapshot: `tests/snapshots/dsv4_capability_latest.json`
- Model: `deepseek-v4-pro`
- API base: `https://api.deepseek.com/beta`

The suite uses 10 repo-safe synthetic cases rather than copying external
benchmark items verbatim. It is shaped by:
- SWE-bench Verified / BigCodeBench-Hard: production bug diagnosis and patch
  quality.
- LiveCodeBench: code execution and output prediction.
- AgentBench / PlanBench: multi-step operational planning under constraints.
- BFCL V4: function-calling, nested arguments, and safe no-call behavior.

## Score
Final live run:
- Total: `91/100`
- Code: `38/40`
- Planning: `23/30`
- Tool calling: `30/30`

## Observations
- Tool calling was clean: multi-call weather lookup, nested log query, and
  clarify-before-deploy all scored full marks.
- Code performance was strong, but the batch-mutation case still lost points
  for not using the expected aggregate/coalesce strategy.
- Planning exposed the real weakness: the dirty-worktree CI/push case scored
  `4/10` because the model proposed unsafe workflow elements such as broad
  staging/stashing instead of the stricter no-destructive, rebase-first path.

## Commands
```bash
scripts/run_dsv4_capability_suite.py
cargo run -p eli --quiet -- run 'Return exactly OK and no other text.' --session-id dsv4-smoke-$(date +%s)
python -m py_compile scripts/run_dsv4_capability_suite.py
python -m pytest tests/test_dsv4_capability_suite.py
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

The Eli CLI smoke returned `OK` through the active `deepseek:deepseek-v4-pro`
profile.
