# 2026-05-18 · DSv4 Eli E2E benchmark snapshot

## Context
The benchmark target is Eli itself. DeepSeek V4 is only the configured backend
model; all scored work now goes through Eli CLI, Eli profile resolution, Eli
tape persistence, Eli tool execution, and Eli subagent plumbing.

## Suite
- Cases: `tests/benchmarks/dsv4_hard_tail_cases.json`
- Runner: `scripts/run_dsv4_capability_suite.py`
- Snapshot: `tests/snapshots/dsv4_capability_latest.json`
- Profile under test: `dsv4`
- Model: `deepseek-v4-pro`
- API base: `https://api.deepseek.com/beta`

The runner creates an isolated `ELI_HOME`, writes a `dsv4` profile plus a
DeepSeek auth entry copied from local credentials, builds the current Eli binary,
and executes 10 hard-tail cases through `eli status` or `eli run`. The fixture
`tests/fixtures/dsv4_e2e_tool_fixture.txt` proves real `fs.read` execution.

## Coverage
- Profile/config resolution for the `dsv4` alias.
- Paper-reading and production-adoption planning.
- Fixed-date/time planning with Asia/Shanghai persistence.
- Gateway issue-resolution planning and Rust async bug analysis.
- Tape handoff via internal command and model-driven tool call.
- Multi-turn tape memory across one Eli session.
- Model-driven `fs.read` tool execution.
- Subagent tool execution through a fake `codex` CLI in an isolated `PATH`.

## Score
Latest real Eli run:
- Eli E2E total: `97/100`
- Config resolution: `10/10`
- Research planning: `10/10`
- Time planning: `10/10`
- Issue resolution: `9/10`
- Code analysis: `8/10`
- Memory handoff: `20/20`
- Tool execution: `20/20`
- Subagent management: `10/10`

The first Eli E2E attempt exposed a real DSv4 integration bug: after a tool call,
DeepSeek rejected the next request with `reasoning_content` missing from the
thinking-mode assistant message. The fix preserves `reasoning_content` in tape
tool-call entries and restores it when Eli rebuilds context.

The remaining misses in the latest snapshot were model-behavior misses, not Eli
runtime failures: the issue-plan answer did not explicitly mention
dirty-worktree safety, and the async-code answer missed the small-function
rubric wording.

## Commands
```bash
scripts/run_dsv4_capability_suite.py --keep-eli-home
python -m py_compile scripts/run_dsv4_capability_suite.py
python -m pytest tests/test_dsv4_capability_suite.py
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

## Notes
This replaces the previous API-only capability score. If a future score mentions
this suite, it should report `scores.eli_e2e_total`, not `api_total`.
