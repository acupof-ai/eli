//! LLM creation and request building for the agent.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use nexil::llm::{ChatRequest, LLM};
use nexil::{ConduitError, TapeContext, Tool, ToolAutoResult, ToolContext, ToolSet};
use serde_json::Value;

use crate::builtin::settings::{AgentSettings, ApiBaseConfig, ApiKeyConfig};
use crate::builtin::store::ForkTapeStore;
use crate::prompt_builder::{PromptBuilder, PromptMode};

use crate::tools::{REGISTRY, model_tools, model_tools_cached};
use crate::types::{PromptValue, RUNTIME_SYSTEM_PROMPT_KEY, RUNTIME_TAPES_DIR_KEY};

pub(super) fn build_tool_state(
    state: &HashMap<String, Value>,
    settings: &AgentSettings,
    allowed_skills: Option<&HashSet<String>>,
    allowed_tools: Option<&HashSet<String>>,
) -> HashMap<String, Value> {
    let mut tool_state = state.clone();
    tool_state.insert(
        RUNTIME_TAPES_DIR_KEY.to_owned(),
        Value::String(settings.home.join("tapes").display().to_string()),
    );

    if let Some(allowed) = allowed_skills {
        tool_state.insert("allowed_skills".to_owned(), sorted_string_array(allowed));
    }
    if let Some(allowed) = allowed_tools {
        tool_state.insert("allowed_tools".to_owned(), sorted_string_array(allowed));
    }

    tool_state
}

fn sorted_string_array(set: &HashSet<String>) -> Value {
    let mut items: Vec<&str> = set.iter().map(String::as_str).collect();
    items.sort_unstable();
    Value::Array(
        items
            .into_iter()
            .map(|s| Value::String(s.to_owned()))
            .collect(),
    )
}

pub(super) fn build_tool_context(
    run_id: &str,
    tape_name: &str,
    tool_state: &HashMap<String, Value>,
) -> ToolContext {
    let mut ctx = ToolContext::new(run_id).with_tape(tape_name.to_owned());
    for (key, value) in tool_state {
        ctx = ctx.with_state(key.clone(), value.clone());
    }
    ctx
}

pub(super) fn lookup_registered_tool(name: &str) -> Option<Tool> {
    let reg = REGISTRY.lock();
    reg.get(name)
        .cloned()
        .or_else(|| {
            if name.contains('_') {
                reg.get(&name.replace('_', ".")).cloned()
            } else {
                None
            }
        })
        .or_else(|| {
            if name.contains('.') {
                reg.get(&name.replace('.', "_")).cloned()
            } else {
                None
            }
        })
}

#[allow(clippy::type_complexity)]
fn resolve_stored_api_key(
    model_str: &str,
) -> Option<(
    Option<String>,
    Option<std::collections::HashMap<String, String>>,
)> {
    let provider = nexil::core::provider_policies::normalized_provider_name(
        model_str.split(':').next().unwrap_or(""),
    );
    let key_map: HashMap<String, String> = provider_resolvers()
        .into_iter()
        .filter(|(name, _)| provider.is_empty() || provider == *name)
        .filter_map(|(name, resolve)| resolve().map(|key| (name.to_owned(), key)))
        .collect();

    match key_map.len() {
        0 => None,
        1 => {
            let (_, v) = key_map
                .into_iter()
                .next()
                .expect("SAFETY: len == 1 verified");
            Some((Some(v), None))
        }
        _ => Some((None, Some(key_map))),
    }
}

type ProviderResolver = (&'static str, Box<dyn FnOnce() -> Option<String>>);

fn provider_resolvers() -> Vec<ProviderResolver> {
    vec![
        (
            "openai",
            Box::new(|| {
                let resolver = nexil::auth::openai_codex::codex_cli_api_key_resolver(None);
                resolver("openai")
            }),
        ),
        (
            "anthropic",
            Box::new(crate::builtin::config::load_anthropic_api_key),
        ),
        (
            "github-copilot",
            Box::new(|| {
                let resolver =
                    nexil::auth::github_copilot::github_copilot_oauth_resolver(None, None, None);
                resolver("github-copilot")
            }),
        ),
        (
            "volcano",
            Box::new(|| crate::builtin::config::load_api_key_entry("volcano")),
        ),
        (
            "deepseek",
            Box::new(|| crate::builtin::config::load_api_key_entry("deepseek")),
        ),
    ]
}

pub(super) fn create_llm(
    settings: &AgentSettings,
    model_override: Option<&str>,
    tape_store: ForkTapeStore,
) -> Result<LLM, ConduitError> {
    let model_str = resolve_model_string(model_override.unwrap_or(&settings.model));

    // Warn once at startup when no fallback chain is configured anywhere —
    // not in ELI_FALLBACK_MODELS and not derivable from the profiles list —
    // so the operator knows rate-limit / overflow errors won't roll over.
    if settings.fallback_models.is_none() {
        static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        WARNED.get_or_init(|| {
            tracing::warn!(
                "no fallback models configured (set ELI_FALLBACK_MODELS or add more \
                 profiles to ~/.eli/config.toml); rate-limit and context-overflow \
                 errors will not automatically retry on a different model"
            );
        });
    }

    let mut builder = LLM::builder()
        .model(&model_str)
        .api_format(settings.api_format)
        .verbose(settings.verbose as u32)
        .tape_store(tape_store)
        .spill_dir(settings.home.join("tapes"))
        .context_window(settings.context_window);

    if let Some(fallback_models) = settings.fallback_models.clone() {
        builder = builder.fallback_models(fallback_models);
    }

    builder = apply_api_key(builder, &settings.api_key, &model_str);
    builder = apply_api_base(builder, &settings.api_base);

    builder.build()
}

fn resolve_model_string(model_str: &str) -> String {
    if model_str.contains(':') {
        model_str.to_owned()
    } else {
        let config = crate::builtin::config::EliConfig::load();
        let provider = config
            .resolve_provider()
            .unwrap_or_else(|| "openai".to_string());
        format!("{provider}:{model_str}")
    }
}

fn apply_api_key(
    builder: nexil::llm::LLMBuilder,
    config: &ApiKeyConfig,
    model_str: &str,
) -> nexil::llm::LLMBuilder {
    let (api_key, api_key_map) = match config.clone() {
        ApiKeyConfig::Single(k) => (Some(k), None),
        ApiKeyConfig::PerProvider(m) => (None, Some(m)),
        ApiKeyConfig::None => resolve_stored_api_key(model_str).unwrap_or((None, None)),
    };
    match (api_key, api_key_map) {
        (Some(key), _) => builder.api_key(&key),
        (_, Some(map)) => builder.api_key_map(map),
        _ => builder,
    }
}

fn apply_api_base(
    builder: nexil::llm::LLMBuilder,
    config: &ApiBaseConfig,
) -> nexil::llm::LLMBuilder {
    match config.clone() {
        ApiBaseConfig::Single(b) => builder.api_base(&b),
        ApiBaseConfig::PerProvider(m) => builder.api_base_map(m),
        ApiBaseConfig::None => builder,
    }
}

pub(super) fn build_system_prompt(
    settings: &AgentSettings,
    prompt_text: &str,
    state: &HashMap<String, Value>,
    allowed_skills: Option<&HashSet<String>>,
    workspace: &Path,
) -> String {
    PromptBuilder::new(PromptMode::Full).build(
        settings,
        prompt_text,
        state,
        allowed_skills,
        &HashSet::new(),
        workspace,
    )
}

fn precomputed_system_prompt(state: &HashMap<String, Value>) -> Option<String> {
    state
        .get(RUNTIME_SYSTEM_PROMPT_KEY)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub(super) fn system_prompt_for_turn(
    settings: &AgentSettings,
    prompt_text: &str,
    state: &HashMap<String, Value>,
    allowed_skills: Option<&HashSet<String>>,
    workspace: &Path,
) -> String {
    precomputed_system_prompt(state).unwrap_or_else(|| {
        build_system_prompt(settings, prompt_text, state, allowed_skills, workspace)
    })
}

/// Max active tasks surfaced in the tail recitation.
const MAX_RECITED_TASKS: usize = 12;

/// Build an ephemeral active-task recitation for this session, surfaced at the
/// context tail (see `ChatRequest.tail_reminder`) to counter lost-in-the-middle
/// drift. Returns `None` when the session has no active tasks, so it's a no-op
/// for sessions that don't use the taskboard.
async fn build_task_recitation(session_id: &str) -> Option<String> {
    let store = crate::taskboard::task_store()?;
    let filter = crate::taskboard::TaskFilter {
        status: None,
        kind: None,
        parent: None,
        session_origin: Some(session_id.to_owned()),
        limit: Some(50),
    };
    let active: Vec<_> = store
        .list(filter)
        .await
        .into_iter()
        .filter(|t| !t.status.is_terminal())
        .collect();
    if active.is_empty() {
        return None;
    }
    let mut out = String::from(
        "[Active tasks — keep these in focus and update their status as you progress:]",
    );
    for t in active.iter().take(MAX_RECITED_TASKS) {
        let brief: String = t
            .context
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        out.push_str(&format!(
            "\n- {} [{}] {}: {brief}",
            &t.id.to_string()[..8],
            t.status.label(),
            t.kind,
        ));
    }
    Some(out)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_tools_once(
    llm: &mut LLM,
    system_prompt: &str,
    tape_name: &str,
    prompt: &PromptValue,
    tool_state: &HashMap<String, Value>,
    settings: &AgentSettings,
    allowed_tools: Option<&HashSet<String>>,
    tape_context: Option<&TapeContext>,
    session_id: &str,
) -> Result<ToolAutoResult, ConduitError> {
    let has_filter = allowed_tools.is_some();
    let mut tools: Vec<Tool> = {
        let reg = REGISTRY.lock();
        if let Some(allowed) = allowed_tools {
            reg.values()
                .filter(|t| allowed.contains(&t.name.to_lowercase()))
                .cloned()
                .collect()
        } else {
            reg.values().cloned().collect()
        }
    };

    // Apply tool wrapping from the turn context (set by framework).
    let wrap_fn = crate::control_plane::turn_wrap_tools();
    if let Some(ref wf) = wrap_fn {
        tools = wf(tools);
    }

    // Use cached model tools when no filtering or wrapping was applied.
    let model_tool_list = if !has_filter && wrap_fn.is_none() {
        model_tools_cached()
    } else {
        model_tools(&tools)
    };
    let schemas: Vec<Value> = model_tool_list.iter().map(|t| t.schema()).collect();
    let tool_set = ToolSet {
        schemas,
        runnable: tools,
    };

    let (prompt_str, user_content) = match prompt {
        PromptValue::Parts(parts) => (None, Some(parts.clone())),
        _ => (Some(prompt.strict_text()), None),
    };
    let prompt_ref = prompt_str.as_deref();

    let tool_ctx = build_tool_context("agent_loop", tape_name, tool_state);

    let cancellation = crate::control_plane::turn_cancellation();
    let tail_reminder = build_task_recitation(session_id).await;

    let result = llm
        .run_tools(ChatRequest {
            prompt: prompt_ref,
            user_content,
            system_prompt: Some(system_prompt),
            max_tokens: Some(settings.max_tokens as u32),
            tools: Some(&tool_set),
            tool_context: Some(&tool_ctx),
            tape: Some(tape_name),
            tape_context,
            cancellation,
            context_window: Some(settings.context_window),
            session_id: Some(session_id),
            token_budget: settings.max_turn_tokens,
            tail_reminder,
            ..Default::default()
        })
        .await?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::builtin::settings::{ApiBaseConfig, ApiKeyConfig};
    use nexil::llm::ApiFormat;
    use serde_json::json;

    fn test_settings(home: &Path) -> AgentSettings {
        AgentSettings {
            home: home.to_path_buf(),
            model: "test-model".into(),
            fallback_models: None,
            api_key: ApiKeyConfig::None,
            api_base: ApiBaseConfig::None,
            api_format: ApiFormat::Auto,
            max_steps: 5,
            max_tokens: 256,
            model_timeout_seconds: None,
            verbose: 0,
            context_window: 128_000,
            max_turn_tokens: None,
        }
    }

    #[test]
    fn test_system_prompt_for_turn_prefers_precomputed_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(workspace.join(".agents")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(workspace.join(".agents").join("SOUL.md"), "from-builder").unwrap();

        let mut state = HashMap::new();
        state.insert(RUNTIME_SYSTEM_PROMPT_KEY.to_owned(), json!("from-state"));

        let result =
            system_prompt_for_turn(&test_settings(&home), "hello", &state, None, &workspace);

        assert_eq!(result, "from-state");
    }
}
