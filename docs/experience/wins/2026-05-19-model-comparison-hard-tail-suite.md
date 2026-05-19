# DSv4 vs GPT-5.5 Frontier Hard-Tail A/B Suite

Date: 2026-05-19

## Result

`scripts/run_model_comparison_suite.py --keep-eli-home` ran both models through
the same Eli CLI harness and wrote
`tests/snapshots/model_comparison_latest.json`.

| Model | Score |
|---|---:|
| DSv4 (`deepseek:deepseek-v4-pro`) | 96 / 100 |
| GPT-5.5 (`openai:gpt-5.5`) | 95 / 100 |

DSv4 led by 1 point after fixing benchmark-local compatibility issues in the
runner and making hidden rubric requirements explicit in the hard-tail prompts.
The remaining DSv4 misses were narrow: the Metal task omitted the literal
`1.15x` acceptance wording, the OpenAI payload sketch omitted the local
`kwargs` term, and the math answer used a valid non-integral derivation that did
not hit the integral-based rubric terms.

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
  fenced JSON, decimal percentage spellings, stdout truncation, field-level
  scoring, and one bounded same-session JSON repair turn. Disable it with
  `ELI_AB_COMPAT=0` or `--no-output-compat` when auditing whether the
  compatibility fixes can be removed.
