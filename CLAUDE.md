# Eli — Claude Code Config

Assisting **ckl**. Greet by name at conversation start. Agent contract: **AGENTS.md**.

---

## Execution Workflow

Non-trivial tasks follow phases. **Each phase runs in its own Agent context** to prevent cognitive bleed.

### Phase 1: Explore

**Posture**: Cartographer.

- See a relevant file → trace its callers and dependents before reading it
- Want to write new code → Grep for existing implementations first
- Involves a trait change → list all implementors, mark blast radius
- Ready to conclude → stop. List unread related files. Read them first
- Uncertain about something → surface as open question, don't guess

### Phase 2: Plan

**Posture**: Architect. Output decisions, not options.

- Start planning → write "how would this fail?" before "how should this work?"
- Plan touches >5 files → question if there's a simpler path
- Hit an irreversible decision (public API, serialization format, trait redesign) → stop, flag it, wait for user confirmation
- Choosing between A and B → check memory rules; no match → pick the reversible one; both reversible → ask user
- Before coding → identify the 3-5 most consequential decisions specific to THIS task, show choices and alternatives, wait for user confirmation
- Plan done → align with user before proceeding

### Phase 3: Implement

**Posture**: Contractor. Build to spec. Spec wrong → fix spec first.

- Before writing code → WebSearch for how others solved this. Prior art > invention. Skip for trivial/project-specific changes
- Want to change something outside the plan → stop. Update plan or note it for later
- Completed a logical unit → `cargo check` immediately
- Writing new code → match adjacent code style (error handling, logging, naming)
- Need a new file → find a similar file for structural reference
- Plan is wrong → stop implementing, update plan, then continue
- All changes done → `cargo clippy --workspace -- -D warnings`

### Phase 4: Verify

**Posture**: Adversary. Assume bugs exist until proven otherwise.

- Code changed → `cargo test --workspace`. Fix before proceeding
- Each diff line → does this serve the goal? No → remove
- New `unwrap()` → can this panic? Under what input?
- Async code → cancel-safe? Race conditions?
- `clone()` → necessary? Can a reference work?
- Unplanned changes in diff → revert or split into separate commit

### Phase 5: Reflect

**Posture**: Retrospector. Extract rules, not narratives.

- Bug took >1 attempt → write `docs/experience/errors/YYYY-MM-DD-slug.md`
- Approach worked well → write `docs/experience/wins/YYYY-MM-DD-slug.md`
- User corrected you → write feedback memory before resuming
- Something surprised you → surprise = wrong mental model, worth recording
- Task done → update or archive the plan file

### Skip rules

- **Trivial** (typo, one-liner): Implement + Verify only.
- **Exploration** ("how does X work?"): Phase 1 only.
- **"Just do it"**: Implement + Verify. Note skipped exploration.

---

## Memory

**Always-load**: auto memory (feedback, user, project) + latest 3 from `docs/experience/errors/` and `docs/experience/wins/`.

**On-demand**: `docs/plans/`, full experience entries, `AGENTS.md`.

---

## Editing Rules

- **Preserve by default**: When modifying existing content (prompts, config, docs), NEVER delete content that wasn't explicitly asked to change. Only add/modify what was requested.
- **Diff before delete**: Before committing any deletion of existing content, show what's being removed and get confirmation.
- **Approach-first for complex features**: For changes touching >3 files or involving architectural decisions, outline the approach in 3–5 bullets (files to modify, key decision, tradeoffs) and wait for approval before writing code.

---

## Behavior Rules

- **Self-correction**: On ANY user correction → codify a preventive feedback memory before resuming.
- **Auto-continue**: Same decision ≥2 times in memory → proceed with inline note. Ask when ambiguous, irreversible, or no match.
- **Opportunistic cleanup**: Reading code and spot something inelegant (dead code, unnecessary clone, unclear naming, redundant logic, etc.) → fix it in a separate commit, report the change inline, and log to `docs/experience/wins/YYYY-MM-DD-cleanup-slug.md`.
- **Release safety**: Before bumping a version, check the latest published version first. Never assume a version number is available. If publish fails due to version conflict, bump patch and retry (max 3 attempts).

---

## Experience Entries

**Error** (`docs/experience/errors/YYYY-MM-DD-slug.md`):
```
# YYYY-MM-DD · Title
## Context
## Root Cause
## Fix
## Rule
```

**Win** (`docs/experience/wins/YYYY-MM-DD-slug.md`):
```
# YYYY-MM-DD · Title
## Context
## What Worked
## Rule
```

Trigger: bug took >1 attempt, user correction reveals systemic issue, or approach worked notably well.

---

## Build & Run

```bash
cargo build --release                     # build
cargo test --workspace                    # all tests
cargo test -p eli -- <test_name>          # single test
cargo fmt --all -- --check                # format check
cargo clippy --workspace -- -D warnings   # lint (CI = warnings-as-errors)
```

```bash
eli chat                    # interactive REPL
eli run "prompt"            # one-shot
eli gateway                 # channel listener (Telegram)
```

### Integration Tests (Python)

```bash
python3 -m pytest tests/ -v              # all integration tests (requires API keys)
python3 -m pytest tests/test_basic.py -v # basic: smoke, text chat, provider switch
python3 -m pytest tests/test_vision.py -v # vision: multimodal image tests
```

**Prerequisites:** `eli` binary in PATH, authenticated providers (`eli login`).

**Rules:**
- Tests hit **real LLM APIs** — they cost money and take time (~1min for vision suite).
- Each test switches provider explicitly via `eli use <profile>` — no shared state.
- Vision tests write temp PNG files, reference them in prompts, and verify the model describes the correct color.
- Assertions are fuzzy (keyword lists, not exact match) because LLM output is nondeterministic.
- Add new tests in `tests/test_<feature>.py`. Use `conftest.py` helpers (`run_eli`, `switch_profile`, `assert_response_contains`).
- **New feature = new integration test.** Every user-facing capability should have a CLI test that exercises the real API path.

---

## Key References

[Turn pipeline](crates/eli/src/framework.rs) · [Hook contract](crates/eli/src/hooks.rs) · [Builtin plugins](crates/eli/src/builtin/) · [LLM client (nexil)](crates/nexil/src/llm.rs) · [Channel trait](crates/eli/src/channels/base.rs) · [Rust coding conventions](docs/rust-conventions.md)

---

## Architecture

Primary languages: **Rust** (core) and **Python** (integration tests).

Two-crate Cargo workspace (edition 2024):

**`nexil`** (crate dir: `crates/nexil`) — Provider-agnostic LLM toolkit. Transport, streaming, tool schema, tape storage, OAuth auth. Entry: `LLM` + `LLMBuilder` in `llm.rs`.

**`eli`** — Hook-first agent framework. Turn pipeline:

```
resolve_session → load_state → build_prompt → run_model → save_state → render_outbound → dispatch_outbound
```

Every stage is a hook (`EliHookSpec`, 12 points). Builtins register first. **Last-registered wins**.

---

## Patterns

- **Envelope** = `serde_json::Value`. Helpers via `ValueExt` trait: `.field()`, `.content_text()`, `.normalize_envelope()` in `envelope.rs`.
- **Channels**: `Channel` trait → CLI + Telegram. Shared `InboundProcessor` path. `ChannelManager` handles debounce and shutdown.
- **Tools**: dot-named (`fs.read`), underscores for LLM APIs. Global `REGISTRY`.
- **Skills**: `SKILL.md` with YAML frontmatter. Precedence: project > global > builtin.
- **Tape**: append-only history, anchoring, forking.
- **Telegram shutdown**: `CancellationToken` + `abort()`. No graceful teloxide shutdown.

---

## Config

Env: `ELI_` prefix — `ELI_MODEL`, `ELI_API_KEY`, `ELI_API_BASE`, `ELI_TELEGRAM_TOKEN`. `.env` loaded via `dotenvy`. Profiles: `~/.eli/config.toml`. Tapes: `~/.eli/tapes/`.
