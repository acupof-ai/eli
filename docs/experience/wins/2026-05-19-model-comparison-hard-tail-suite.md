# DSv4 vs GPT-5.5 Frontier Hard-Tail A/B Suite

Date: 2026-05-19

## Result

`scripts/run_model_comparison_suite.py --keep-eli-home` ran both models through
the same Eli CLI harness and wrote
`tests/snapshots/model_comparison_latest.json`.

Reported totals use the raw first answer. The bounded same-session repair turn
is recorded only under `compat_total`.

| Model | Raw score | Compat score |
|---|---:|---:|
| DSv4 (`deepseek:deepseek-v4-pro`) | 72 / 100 | 74 / 100 |
| GPT-5.5 (`openai:gpt-5.5`) | 81 / 100 | 83 / 100 |

GPT-5.5 led by 9 raw points. The previous 96/95 snapshot was removed because
the prompts were too explicit and the rubric rewarded keyword compliance more
than hard agent behavior.

## Coverage

- SWE-style issue triage from fixture evidence, preserving local tape state
  while stripping OpenAI-incompatible request-body state
- PaperBench-style reproduction audit with p50 arithmetic, p95 regressions,
  energy regression, seed selection, and ablation concerns
- Mac-only Metal-vs-MLX gate using fixture measurements, correctness tolerance,
  speedup math, and merge policy
- RE-Bench-style two-hour ML engineering decision under leakage, runtime, OOM,
  and seed-reproducibility constraints
- Frontier-math harmonic-sum trap
- BrowseComp-style source discovery plan with negative evidence and citation
  confidence
- Strict writing compression that decides whether to delete saturated cases
  while keeping the Eli E2E harness
- Fixed-time release planning with exact Asia/Shanghai deadline, CI/CodeQL, and
  push sequencing
- Fourteen-turn memory stress with corrections and synthetic-secret redaction
- Synchronous stale subagent result plus `tape.handoff` conflict resolution

## Notes

- The suite version is now `3`.
- The score gate checks that the snapshot is not saturated; no model may exceed
  `90%` in the checked snapshot.
- `ELI_EVOLUTION_DISABLED=1` is set inside the runner, and each model uses an
  isolated bench `ELI_HOME`; benchmark tapes and handoffs do not update global
  Eli evolution state.
- Metal validation is constrained to macOS/Darwin only. The benchmark does not
  design or require Linux/CUDA Metal tests.
- The output compatibility layer remains removable. Disable it with
  `ELI_AB_COMPAT=0` or `--no-output-compat` when auditing whether those fixes
  can be deleted.
