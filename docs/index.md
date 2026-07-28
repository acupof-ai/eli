# Eli Documentation

This is the current documentation entrypoint for the repository.

## Repository Shape

- `crates/nexil` — provider-agnostic LLM toolkit
- `crates/eli` — hook-first AI agent framework (includes native Telegram + Feishu channels)

## Quick Start

```bash
git clone https://github.com/cklxx/eli.git
cd eli
cp env.example .env
cargo build --release
```

## Common Local Commands

```bash
just doctor
just check
just test-all
```

## Main Runtime Commands

```bash
eli chat
eli run "summarize this repo"
eli gateway
```

## Turn Pipeline

```text
resolve_session → load_state → build_prompt → run_model → save_state → render_outbound → dispatch_outbound
```

## Read Next

- [Repository review](REVIEW_2026-04-10.md)
- [DX improvement plan](DX_IMPROVEMENT_PLAN_2026-04-10.md)
- [Architecture review](ARCHITECTURE_REVIEW_2026-04-06.md)
- [Channels](channels/index.md)
- [Rust conventions](rust-conventions.md)
- [Plans](plans/)
- [Experience notes](experience/)

## Historical Snapshots

These are useful reference documents, but not the primary source of truth:

- [Architecture landscape](ARCHITECTURE_LANDSCAPE.md)
- [Workspace structure review](STRUCTURE_REVIEW.md)

## Source of Truth Order

When docs conflict, trust these in order:

1. current code in `crates/`
2. `README.md`
3. `AGENTS.md` and `CLAUDE.md`
4. current docs in `docs/`
5. historical snapshots
