# 2026-05-18 · DSV4 real-world agent benchmark snapshot

## Context
The DeepSeek V4 provider path is wired through Eli, and the test target is now a
real API agent benchmark rather than short isolated capability prompts. The suite
uses real DeepSeek API calls plus local runtime checks for handoff and subagent
management.

## Suite
- Cases: `tests/benchmarks/dsv4_hard_tail_cases.json`
- Runner: `scripts/run_dsv4_capability_suite.py`
- Snapshot: `tests/snapshots/dsv4_capability_latest.json`
- Model: `deepseek-v4-pro`
- API base: `https://api.deepseek.com/beta`

The suite keeps 10 hard-tail API cases, 100 API points, and adds 20 local runtime
points. The prompts are synthetic and repo-safe, but shaped by real-world agent
benchmarks: SWE-bench Verified / SWE-bench Live for issue resolution,
AgentBench / AgencyBench for long-horizon planning and state tracking,
MLAgentBench for paper and experiment reading, BFCL V4 / MCP-Bench for tool use,
and PlanBench for structured planning.

## Score
Final live run:
- API: `89/100`
- Local runtime: `20/20`
- Combined: `109/120`

Capability split:
- Research planning: `10/10`
- Time planning: `16/20`
- Issue resolution: `19/20`
- Code analysis: `9/10`
- Memory handoff: `10/10`
- Subagent management: `10/10`
- Tool use: `15/20`

## Observations
- Paper interpretation and adoption planning were strong: the model identified
  cache-hit isolation, ablations, p95 latency, and production rollout concerns.
- Local handoff and subagent management checks passed: the cargo-level
  auto-handoff grace test and subagent tracker management test both scored full.
- The fixed-time tool call exposed a real weakness: it scheduled the correct
  local wall time but omitted the `+08:00` offset and used an empty recurrence
  field instead of `none`.
- The repo investigation tool case exposed another real weakness: it called
  `repo_search` repeatedly but did not call `read_file`, so it did not complete
  the required inspect-after-search workflow.
- Handoff plus subagent tool actions were mostly correct, but the handoff summary
  did not preserve the exact DeepSeek beta endpoint string.

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
