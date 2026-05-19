# DSv4 vs GPT-5.5 Hard-Tail A/B Suite

Date: 2026-05-19

## Result

`scripts/run_model_comparison_suite.py --keep-eli-home` ran both models through
the same Eli CLI harness and wrote
`tests/snapshots/model_comparison_latest.json`.

| Model | Score |
|---|---:|
| DSv4 (`deepseek:deepseek-v4-pro`) | 92 / 100 |
| GPT-5.5 (`openai:gpt-5.5`) | 98 / 100 |

GPT-5.5 led by 6 points. The clearest separator was fixed-time planning across
the 2026 Pacific DST transition: DSv4 scored 5 / 10, GPT-5.5 scored 10 / 10.
GPT-5.5 also gained 1 point on the 9-turn state-retention case.

## Coverage

- Paper-method critique and adoption planning
- Fixed-date scheduling with timezone/DST conversion
- Gateway issue-resolution planning under dirty-worktree constraints
- Rust async shutdown/code-analysis planning
- OpenAI `session_id` failure triage
- 9-turn local memory retention
- `fs.read` tool evidence reused on the next turn
- `tape.handoff` creation and snapshot preservation
- Synchronous `agent` subagent orchestration through a fake `codex`
- Bounded tool-budget planning with KV-cache constraints

## Notes

- OpenAI GPT-5.5 works after keeping Eli's `session_id` local instead of
  forwarding it as a public OpenAI top-level request field.
- Both models passed the real tool-call and next-turn replay cases.
- The old DSv4-only score suite was removed because it measured single-model
  capability rather than A/B discrimination.
