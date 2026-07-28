# Eli — Agent Contract

Minimize mandatory reading. Expand context only when triggered.

## 0. Read Policy

1. Read **§1** on every task.
2. Read **§2** only when a trigger matches.
3. No trigger? Follow **§3** and stop expanding context.

---

## 1. Mandatory Core

### 1.1 Identity and priority
- Greet **ckl** at conversation start.
- Priority: **safety > correctness > maintainability > speed**.
- User: senior Rust/backend engineer; values deep reasoning, clean architecture.

### 1.2 Code style (non-negotiable)
- Max function body: **15 lines**. Extract or redesign if exceeded.
- No comments that restate code. Only "why" comments for non-obvious decisions.
- Prefer composition over inheritance. Prefer data transforms over mutation.
- Every abstraction must justify itself: if used <2 places, inline it.
- Delete dead code immediately. No TODOs in committed code.
- Type signatures are documentation. Verbose names > comments.
- Between two correct approaches, pick the one with fewer moving parts.
- Trust type/caller invariants; no unnecessary defensive code.
- No compatibility shims when requirements change; redesign cleanly.
- Modify only relevant files.
- Rust edition `2024`, 4-space indentation. `snake_case` for modules/functions/variables, `PascalCase` for types, `UPPER_SNAKE_CASE` for constants.

Reference density:
```rust
fn authenticate(token: &str, secret: &str) -> Result<Claims, AuthError> {
    decode(token)
        .and_then(|t| verify(t, secret))
        .map_err(AuthError::from)
}
```
No wrapper structs for one field. No builders unless >4 params. Transform pipeline.

### 1.3 Approach confirmation
- Changes touching >3 files or involving architectural decisions → outline approach in 3–5 bullets before writing code. Wait for ckl's approval.
- When proposing structural changes (tool grouping, caching, prompt restructuring), consider downstream impact on LLM KV cache and call patterns. Flag anything that would break cache efficiency.

### 1.4 Prior art
- Before implementing non-trivial logic, WebSearch for how the world solves it. Prior art > invention. Skip for trivial or project-specific changes.

### 1.5 Editing safety
- When modifying existing content (prompts, system messages, config), NEVER delete content not explicitly targeted. Only add/modify what was requested.
- Show a diff summary before committing any deletions of existing content.

### 1.6 Delivery
- Prefer TDD for logic changes; cover edge cases.
- Run lint + tests before delivery: `cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`.
- Fix P0/P1 before commit; follow-up for P2.
- Small, scoped commits. Warn before destructive ops.
- Follow Conventional Commits: `feat:`, `fix:`, `docs:`, `chore:`.
- **Release safety**: Check latest published version before bumping. If publish fails on version conflict, bump patch and retry (max 3).

---

## 2. Progressive Disclosure (trigger-gated)

| Trigger | Load |
|---|---|
| Non-trivial staged execution | CLAUDE.md Execution Workflow; create `docs/plans/*` |
| Architecture boundaries (`crates/**`) | Architecture section in CLAUDE.md + key references + `docs/rust-conventions.md` |
| Hook/trait changes | List all implementors, mark blast radius before editing |
| Memory/history retrieval | Auto memory + `docs/experience/` summaries first |
| Large mechanical edits | Agent tool: explore → plan → execute → review, max 2 retries |
| User correction | Codify preventive feedback memory before resuming |

No trigger → do not load.

---

## 3. Default Route

1. Read §1 only.
2. Inspect target files and neighboring patterns.
3. Implement minimal correct change.
4. Proportionate verification (scoped tests/lint).
5. Commit and report.

---

## 4. Project Snapshot

- Product: hook-first AI agent framework (CLI, Telegram, Feishu).
- Two-crate workspace: `nexil` (LLM toolkit) → `eli` (agent framework).
- Turn pipeline: `resolve_session → load_state → build_prompt → run_model → save_state → render_outbound → dispatch_outbound`.
- Key dirs: `crates/eli/src/` (framework, hooks, builtins, channels, skills, tools), `crates/nexil/src/` (LLM, auth, tape).
- Config: `ELI_*` env vars, `.env` for secrets, `~/.eli/config.toml` for profiles.

---

## 5. Testing Guidelines

- Add or update Rust unit tests close to the changed code with `#[cfg(test)]`.
- Prefer behavior-oriented test names: `test_build_system_prompt_appends_workspace_agents_guidance`.
- Use `tempfile` workspaces for tests that depend on filesystem state.
- Cover prompt composition, hook precedence, channel routing, tape persistence, and tool wiring.

---

## 6. Detail Sources (trigger-gated only)

`CLAUDE.md` · `docs/rust-conventions.md` · `docs/plans/` · `docs/experience/errors/` · `docs/experience/wins/`