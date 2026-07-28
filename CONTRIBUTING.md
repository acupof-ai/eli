# Contributing to `eli`

`eli` is a multi-language AI project:

- `crates/nexil` — provider-agnostic LLM toolkit
- `crates/eli` — hook-first AI agent framework
- `tests/` — Python smoke and integration tests

## Prerequisites

Have these available locally:

- Rust toolchain
- `cargo`
- Python 3
- `pytest`

Start with:

```bash
just doctor
```

## Common Commands

```bash
just check
just test-rust
just test-py
just test-all
just release-check
```

## Change Expectations

### Rust changes

For changes in `crates/**`:

- run `just check`
- add or update Rust tests where practical
- update docs if behavior changes

### Python test changes

For changes in `tests/**`:

- run `just test-py`
- keep smoke paths cheap and deterministic where possible

### Docs changes

For changes in `README.md` or `docs/**`:

- keep docs aligned with current code
- label historical documents clearly when they are not current guidance

## Before Opening a PR

1. run the smallest meaningful validation for the surface you changed
2. update tests if behavior changed
3. update docs if user-facing or maintainer-facing behavior changed
4. avoid unrelated cleanup in the same PR

Recommended commit prefixes:

- `feat:`
- `fix:`
- `docs:`
- `chore:`

## Extra Care Areas

Call out contract changes clearly in the PR when touching:

- hook semantics or precedence
- session or turn pipeline behavior
- tool schemas or result conventions
- tape behavior

For these changes, state:

- what changed
- what remains compatible
- what tests were added or updated
- what docs were updated

## Where to Start Reading

- `README.md`
- `docs/index.md`
- `AGENTS.md`
- `CLAUDE.md`
