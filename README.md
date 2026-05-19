<p align="center">
  <img src="assets/banner.png" alt="Eli Banner" width="100%">
</p>

<h1 align="center">Eli</h1>

<p align="center">
  Hook-first AI agent framework in Rust.<br>
  Single binary. CLI + gateway runtime. Governed self-evolution built in.
</p>

<p align="center">
  <a href="https://github.com/cklxx/eli/actions/workflows/main.yml"><img src="https://img.shields.io/github/actions/workflow/status/cklxx/eli/main.yml?branch=main&label=CI" alt="CI"></a>
  <a href="https://github.com/cklxx/eli/actions/workflows/pages.yml"><img src="https://img.shields.io/github/actions/workflow/status/cklxx/eli/pages.yml?branch=main&label=Pages" alt="Pages"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="Apache-2.0"></a>
</p>

<p align="center">
  <a href="https://cklxx.github.io/eli">Website</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#self-evolution">Self-Evolution</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="https://github.com/cklxx/eli/issues">Issues</a>
</p>

---

Eli is for people who want an agent runtime, not a notebook demo.

It is built around a fixed turn pipeline, explicit hooks, append-only tape history, and a provider-agnostic LLM layer. You can run it in a terminal, wire it into Telegram, or expose it through a gateway and sidecar-backed channels such as Feishu, WeChat, Slack, Discord, and DingTalk.

```bash
eli chat
eli run "summarize this repo"
eli gateway
```

## Why Eli

LangChain, AutoGen, and crewAI are good at getting an agent demo running. Eli is optimized for people who want a cleaner runtime shape they can keep in production:

- Rust workspace, not Python orchestration glue.
- Single binary deployment for the core runtime.
- Hook-first architecture instead of subclass piles and hidden control flow.
- Tape-backed history that is inspectable, forkable, and replayable.
- Governed self-evolution that does not let the model silently rewrite its own core prompt.

## What You Get

| Capability | What it means |
|---|---|
| **Hook-first runtime** | Override session resolution, prompt building, model execution, state persistence, or outbound rendering independently |
| **Stable turn pipeline** | `resolve_session → load_state → build_prompt → run_model → save_state → render_outbound → dispatch_outbound` |
| **Single binary core** | Core runtime ships as one Rust binary; no Python runtime required for the main agent |
| **Provider-agnostic LLM layer** | `nexil` handles streaming, tool schema, tape storage, OAuth, and provider routing |
| **Tape history** | Append-only session history with anchoring, forking, search, and viewer support |
| **Skills** | `SKILL.md` discovery with project/global precedence and markdown-native authoring |
| **Gateway mode** | Run as a listener for Telegram or via sidecar-backed channels |
| **Governed self-evolution** | Candidate capture, deterministic evaluation, canary promotion, rollback, and automation journal |

## Quick Start

```bash
git clone https://github.com/cklxx/eli.git
cd eli
cargo build --release
cp env.example .env
```

Then add your provider credentials and run:

```bash
eli chat
eli run "summarize this repo"
eli gateway
```

Useful project tasks:

```bash
just doctor
just check
just test-rust
just test-all
```

## Self-Evolution

Eli now includes a governed self-evolution loop.

**The model cannot silently rewrite its own core prompt.** Experience is distilled from tape evidence into candidates, then pushed through a controlled lifecycle:

```text
distill -> evaluate -> canary -> observe -> promote / rollback
```

Candidates share one lifecycle across four unified artifact kinds:

| Kind | Materialized to |
|---|---|
| `prompt_rule` | `.agents/evolution/rules/` (bundle: `rules.bundle.md`) |
| `compiled_knowledge` | `.agents/evolution/knowledge/` (bundle: `knowledge.bundle.md`) |
| `runtime_policy` | `.agents/evolution/runtime-policies/` (bundle: `runtime_policy.bundle.json`) |
| `skill` | `.agents/skills/<name>/SKILL.md` |

What is automated:

- Tape evidence can be distilled into candidates of any artifact kind.
- Candidates are deterministically evaluated before promotion.
- Low-risk candidates can be auto-staged as canaries.
- Repeated observations can auto-promote a canary.
- Every action is written to an automation journal.

What stays governed:

- Core persona prompt is not live-edited.
- Every artifact is materialized as a managed fragment, not an opaque prompt mutation.
- Rollback stays local to the promoted fragment.

Example commands:

```bash
eli evolution distill <tape> --persist
eli evolution evaluate <candidate_id>
eli evolution auto-run <tape>
eli evolution capture-knowledge <artifact> --summary "…" --content "…"
eli evolution capture-runtime-policy <artifact> --summary "…" --content '{…}'
eli evolution history --limit 20
eli evolution list
```

## CLI Surface

| Command | What it does |
|---|---|
| `eli chat` | Interactive REPL |
| `eli run "prompt"` | One-shot turn |
| `eli gateway` | Start channel listeners |
| `eli login` | Authenticate a provider |
| `eli use` | Switch profile |
| `eli model` | Show or switch model |
| `eli status` | Config and auth overview |
| `eli tape` | Tape viewer web UI |
| `eli decisions` | Persistent decision management |
| `eli evolution` | Distill, evaluate, auto-run, inspect history, promote, rollback |

## Gateway & Channels

Native:

```bash
ELI_TELEGRAM_TOKEN=xxx eli gateway
```

Sidecar-backed via OpenClaw:

- Feishu
- WeChat
- Slack
- Discord
- DingTalk

The sidecar is used for channels and plugin-backed integrations. The core Eli runtime still stays a single Rust binary.

## Architecture

![Architecture](site/assets/architecture.png)

> [Interactive diagram](https://cklxx.github.io/eli/#architecture) · [Video walkthrough](https://github.com/cklxx/eli/blob/main/site/assets/architecture.mp4)

Workspace layout:

| Component | Role |
|---|---|
| `crates/nexil` | Provider-agnostic LLM toolkit: transport, streaming, tools, tape, OAuth |
| `crates/eli` | Agent runtime: hooks, channels, tools, skills, prompt builder, evolution |
| `sidecar/` | OpenClaw bridge for plugin-backed channels and MCP-style integrations |

Turn pipeline:

```text
resolve_session
-> load_state
-> build_prompt
-> run_model
-> save_state
-> render_outbound
-> dispatch_outbound
```

The design goal is explicit interception points with minimal hidden state.

## Skills

Skills are markdown files with YAML frontmatter.

Discovery order:

1. `.agents/skills/<name>/SKILL.md`
2. `~/.eli/skills/<name>/SKILL.md`
3. Builtin or synthesized skill sources

That gives you local project override without inventing a new packaging format.

## Configuration

| Variable | Default | Description |
|---|---|---|
| `ELI_MODEL` | `openai:gpt-4o-mini` | `provider:model` identifier |
| `ELI_API_KEY` | — | Provider API key |
| `ELI_API_BASE` | — | Custom endpoint |
| `ELI_MAX_STEPS` | `50` | Max tool iterations per turn |
| `ELI_TELEGRAM_TOKEN` | — | Telegram bot token |
| `ELI_WEBHOOK_PORT` | `3100` | Webhook port |
| `ELI_HOME` | `~/.eli` | Config, tape, and runtime data directory |
| `ELI_TRACE` | — | Trace logging |
| `ELI_EVOLUTION_DISABLED` | — | Disable background auto-evolution loop when set to `1`/`true` |

Profiles live in `~/.eli/config.toml`.

Example profiles:

```toml
active_profile = "deepseek"

[profiles.deepseek]
provider = "deepseek"
model = "deepseek:deepseek-v4-pro"
api_base = "https://api.deepseek.com/beta"

[profiles.openai]
provider = "openai"
model = "openai:gpt-5.5"
```

OpenAI-family profiles, including `openai:gpt-5.5`, keep Eli's tape
`session_id` local. The public OpenAI API rejects a top-level `session_id`,
while local Eli sessions and provider-specific adapters still preserve the
runtime session context.

## Model A/B Benchmark

Run the live hard-tail comparison suite through Eli:

```bash
scripts/run_model_comparison_suite.py --keep-eli-home
```

The default pair is DSv4 (`deepseek:deepseek-v4-pro` at
`https://api.deepseek.com/beta`) versus GPT-5.5 (`openai:gpt-5.5`). The suite
uses the same Eli CLI path for both models and keeps only hard-tail cases that
stress planning, programming, writing, analysis, exploration, frontier-science
reasoning, Mac-only Metal-vs-MLX performance work, multi-turn memory, tool
evidence replay, tape handoff, and subagent orchestration. Results are written
to `tests/snapshots/model_comparison_latest.json`.

Metal performance cases are macOS/Darwin-only. They require a hand-written
Metal path to beat the MLX baseline before merge; Linux or CUDA validation is
not part of this benchmark.

## Development

Repository layout:

- Rust workspace in `crates/`
- Python integration tests in `tests/`
- TypeScript sidecar in `sidecar/`
- Project docs in [`docs/`](docs/)

Common workflows:

```bash
just doctor
just check
just test-rust
just test-py
just test-sidecar
just test-all
```

Docs entrypoint: [`docs/index.md`](docs/index.md)

## License

[Apache-2.0](./LICENSE)
