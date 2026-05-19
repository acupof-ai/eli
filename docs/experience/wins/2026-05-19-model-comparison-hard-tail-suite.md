# DSv4 vs GPT-5.5 Frontier Hard-Tail A/B Suite

Date: 2026-05-19

## Result

`scripts/run_model_comparison_suite.py --keep-eli-home` ran both models through
the same Eli CLI harness and wrote
`tests/snapshots/model_comparison_latest.json`.

| Model | Score |
|---|---:|
| DSv4 (`deepseek:deepseek-v4-pro`) | 79 / 100 |
| GPT-5.5 (`openai:gpt-5.5`) | 80 / 100 |

GPT-5.5 led by 1 point after fixing benchmark-local compatibility issues in
the runner. The strongest remaining signal was not a broad model-quality gap:
DSv4 returned an empty response on the Mac-only Metal design case in this run,
while GPT-5.5 still produced an incomplete Metal plan. DSv4 also produced a
truncated JSON answer on the BrowseComp-style exploration case.

## Coverage

- Frontier-science claim audit with leakage, ablation, and multiple-comparison
  traps
- Mac-only Metal kernel design that must beat an MLX baseline before merge
- Rust OpenAI payload patch sketching under the local tape `session_id` rule
- Decision-memo writing from noisy status text
- Simpson-style analysis of easy-case saturation versus hard-tail signal
- BrowseComp-style exploration planning without fabrication
- Frontier-style mathematical sanity check
- `fs.read` tool evidence reused on the next turn for Metal-vs-MLX triage
- 11-turn memory retention with corrected facts overriding stale facts
- Synchronous `agent` subagent orchestration plus `tape.handoff`

## Notes

- The old high-pass smoke-style cases were removed because they produced
  near-ceiling scores and weak model separation.
- Metal validation is constrained to macOS/Darwin only. The benchmark does not
  design or require Linux/CUDA Metal tests.
- Both models passed the core tool-call, next-turn replay, long multi-turn
  memory, and handoff/subagent infrastructure checks.
- The runner now records output diagnostics (`ok`, `empty_response`,
  `truncated_json`, `invalid_json`) and has a removable compatibility layer for
  fenced JSON, decimal percentage spellings, stdout truncation, and field-level
  scoring. Disable it with `ELI_AB_COMPAT=0` or `--no-output-compat` when
  auditing whether the compatibility fixes can be removed.
