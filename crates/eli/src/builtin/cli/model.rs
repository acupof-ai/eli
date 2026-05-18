//! Model management: show, list, switch.
//!
//! All provider-touching operations route through [`RequestContext`], a
//! profile-derived bundle that mirrors the runtime resolution path
//! (profile → env override → provider default). New backends only need
//! to register a default `api_base` in `provider_policies`; auth is
//! optional so local servers (agent-infer, ollama, vllm, lmstudio) work
//! without provider-specific surgery here.

use crate::builtin::coding_plan;
use crate::builtin::config::EliConfig;
use crate::builtin::settings::EnvConfig;
use nexil::core::provider_policies::{
    default_api_base, is_known_provider, normalized_provider_name,
};

/// Resolved request context for an active profile.
///
/// Mirrors the runtime path's view of "where do I call this provider":
/// `api_base` is the profile override or the registry default; `api_key`
/// is whatever any auth source produced, or `None` for keyless backends.
struct RequestContext {
    provider: String,
    api_base: String,
    api_key: Option<String>,
}

impl RequestContext {
    /// Build a request context from the active profile, applying the same
    /// precedence used by the runtime (`profile.api_base` overrides the
    /// provider registry default; missing API key is allowed).
    fn from_active(config: &EliConfig) -> anyhow::Result<Self> {
        let profile = config.active_profile().ok_or_else(|| {
            anyhow::anyhow!(
                "No active profile configured.\nRun `eli login <provider>` to get started."
            )
        })?;
        let provider = normalized_provider_name(&profile.provider);
        let api_base = profile
            .api_base
            .clone()
            .unwrap_or_else(|| default_api_base(&provider));
        let api_key = resolve_api_key_optional(&provider);
        Ok(Self {
            provider,
            api_base,
            api_key,
        })
    }
}

/// Manage model selection: show, list, or switch.
pub(crate) async fn model_command(name: Option<String>) -> anyhow::Result<()> {
    match name.as_deref() {
        None => model_show(),
        Some("list") => model_list().await,
        Some(model_name) => model_switch(model_name),
    }
}

/// Show the current model.
fn model_show() -> anyhow::Result<()> {
    let config = EliConfig::load();

    match config.active_profile() {
        Some(profile) => {
            let profile_name = config.active_profile.as_deref().unwrap_or("(none)");
            println!("Current model: {}", profile.model);
            println!("  Profile:  {profile_name}");
            println!("  Provider: {}", profile.provider);
        }
        None => {
            println!("No active profile configured.");
            println!("Run `eli login <provider>` to get started.");
        }
    }

    if let Some(env_model) = EnvConfig::model_override() {
        println!();
        println!("Note: ELI_MODEL environment variable is set to: {env_model}");
        println!("  This overrides the configured model at runtime.");
    }

    Ok(())
}

/// Best-effort API key resolution. Returns `None` for keyless / unknown
/// backends; callers are responsible for surfacing a useful error if the
/// downstream HTTP call fails with 401.
fn resolve_api_key_optional(provider: &str) -> Option<String> {
    if let Some(key) = resolve_from_env(provider) {
        return Some(key);
    }
    if let Some(key) = resolve_from_config(provider) {
        return Some(key);
    }
    resolve_via_oauth(provider).ok()
}

fn resolve_from_env(provider: &str) -> Option<String> {
    let provider = normalized_provider_name(provider);
    EnvConfig::api_key(None)
        .filter(|key| !key.is_empty())
        .or_else(|| resolve_provider_env_key(&provider))
        .filter(|key| !key.is_empty())
}

fn resolve_provider_env_key(provider: &str) -> Option<String> {
    let eli_key = format!("ELI_{}_API_KEY", provider.to_uppercase().replace('-', "_"));
    std::env::var(eli_key)
        .ok()
        .filter(|key| !key.is_empty())
        .or_else(|| resolve_standard_env_key(provider))
}

fn resolve_standard_env_key(provider: &str) -> Option<String> {
    match provider {
        "anthropic" => first_env(&["ANTHROPIC_API_KEY"]),
        "openai" => first_env(&["OPENAI_API_KEY"]),
        "openrouter" => first_env(&["OPENROUTER_API_KEY"]),
        "github-copilot" => first_env(&["GITHUB_TOKEN"]),
        "deepseek" => first_env(&["DEEPSEEK_API_KEY"]),
        "volcano" => first_env(&["VOLCANO_API_KEY", "ARK_API_KEY"]),
        _ => None,
    }
}

fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

fn resolve_from_config(provider: &str) -> Option<String> {
    match normalized_provider_name(provider).as_str() {
        "anthropic" => crate::builtin::config::load_anthropic_api_key(),
        "deepseek" => crate::builtin::config::load_api_key_entry("deepseek"),
        "volcano" => crate::builtin::config::load_api_key_entry("volcano"),
        _ => None,
    }
}

fn resolve_via_oauth(provider: &str) -> anyhow::Result<String> {
    match normalized_provider_name(provider).as_str() {
        "openai" => resolve_openai_oauth().ok_or_else(|| {
            anyhow::anyhow!("No API key found for OpenAI.\nRun `eli login openai` or set OPENAI_API_KEY.")
        }),
        "github-copilot" => resolve_github_copilot_oauth().ok_or_else(|| {
            anyhow::anyhow!("No GitHub token found for Copilot.\nRun `eli login github-copilot` or set GITHUB_TOKEN.")
        }),
        "anthropic" => anyhow::bail!(
            "No API key found for Anthropic.\nRun `eli login claude` or set ANTHROPIC_API_KEY."
        ),
        "openrouter" => anyhow::bail!(
            "No API key found for OpenRouter.\nSet OPENROUTER_API_KEY environment variable."
        ),
        "deepseek" => anyhow::bail!(
            "No API key found for DeepSeek.\nRun `eli login deepseek` or set DEEPSEEK_API_KEY."
        ),
        "volcano" => anyhow::bail!(
            "No API key found for Volcano.\nRun `eli login volcano` or set ARK_API_KEY."
        ),
        _ => anyhow::bail!(
            "Cannot resolve API key for unknown provider: {provider}\nSet ELI_API_KEY environment variable."
        ),
    }
}

fn resolve_openai_oauth() -> Option<String> {
    let codex_home = std::env::var("CODEX_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".codex")
        });
    let resolver = nexil::auth::openai_codex::codex_cli_api_key_resolver(Some(codex_home));
    resolver("openai")
}

fn resolve_github_copilot_oauth() -> Option<String> {
    let resolver = nexil::auth::github_copilot::github_copilot_oauth_resolver(None, None, None);
    resolver("github-copilot")
}

/// Fetch available models for the given request context.
///
/// Provider-specific overrides (Codex backend, Copilot session token) are
/// retained because they hit non-standard endpoints. Everything else
/// (openrouter, agent-infer, ollama, vllm, lmstudio, custom) flows through
/// the generic OpenAI-compatible `/v1/models` path keyed on `ctx.api_base`.
async fn fetch_models(ctx: &RequestContext) -> anyhow::Result<Vec<String>> {
    let client = reqwest::Client::new();
    match ctx.provider.as_str() {
        "openai" => {
            let key = ctx.api_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "No API key found for OpenAI.\nRun `eli login openai` or set OPENAI_API_KEY."
                )
            })?;
            fetch_models_openai_codex(&client, key).await
        }
        // Anthropic's model list is curated — no dynamic API needed.
        "anthropic" => Ok(known_models("anthropic")),
        "volcano" => Ok(known_models("volcano")),
        "github-copilot" => {
            let key = ctx.api_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "No GitHub token found for Copilot.\nRun `eli login github-copilot` or set GITHUB_TOKEN."
                )
            })?;
            fetch_models_github_copilot(&client, key).await
        }
        _ => fetch_models_openai_compatible(&client, ctx.api_key.as_deref(), &ctx.api_base).await,
    }
}

async fn fetch_models_openai_codex(
    client: &reqwest::Client,
    api_key: &str,
) -> anyhow::Result<Vec<String>> {
    let resp = client
        .get("https://chatgpt.com/backend-api/codex/models?client_version=0.114.0")
        .bearer_auth(api_key)
        .send()
        .await?;

    if !resp.status().is_success() {
        return fetch_models_openai_compatible(client, Some(api_key), "https://api.openai.com/v1")
            .await;
    }

    let body: serde_json::Value = resp.json().await?;

    let models = body["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|m| m["visibility"].as_str() != Some("hidden"))
                .filter_map(|m| m["slug"].as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(models)
}

/// Fetch models from an OpenAI-compatible API (OpenAI, OpenRouter, agent-infer,
/// ollama, vllm, lmstudio, …). `api_key` is optional: keyless local backends
/// skip the `Authorization` header entirely rather than sending `Bearer `.
async fn fetch_models_openai_compatible(
    client: &reqwest::Client,
    api_key: Option<&str>,
    api_base: &str,
) -> anyhow::Result<Vec<String>> {
    let url = format!("{}/models", api_base.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(key) = api_key.map(str::trim).filter(|k| !k.is_empty()) {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("API request failed (HTTP {status}): {text}");
    }

    let body: serde_json::Value = resp.json().await?;
    let models = body["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(models)
}

async fn fetch_models_github_copilot(
    client: &reqwest::Client,
    github_token: &str,
) -> anyhow::Result<Vec<String>> {
    let token_resp = client
        .get("https://api.github.com/copilot_internal/v2/token")
        .header("Authorization", format!("Bearer {github_token}"))
        .header("User-Agent", "conduit-eli/0")
        .header("Accept", "application/json")
        .send()
        .await?;

    if !token_resp.status().is_success() {
        let status = token_resp.status();
        let text = token_resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "Failed to get Copilot session token (HTTP {status}): {text}\n\
             Make sure you have an active GitHub Copilot subscription."
        );
    }

    let token_body: serde_json::Value = token_resp.json().await?;
    let session_token = token_body["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Copilot token response missing 'token' field"))?;

    let resp = client
        .get("https://api.githubcopilot.com/models")
        .header("Authorization", format!("Bearer {session_token}"))
        .header("Copilot-Integration-Id", "vscode-chat")
        .header("User-Agent", "conduit-eli/0")
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Copilot models API request failed (HTTP {status}): {text}");
    }

    let body: serde_json::Value = resp.json().await?;

    let models = if let Some(arr) = body["data"].as_array() {
        arr.iter()
            .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
            .collect()
    } else if let Some(arr) = body.as_array() {
        arr.iter()
            .filter_map(|m| {
                m["id"]
                    .as_str()
                    .or_else(|| m["model"].as_str())
                    .map(|s| s.to_string())
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok(models)
}

fn filter_chat_models(provider: &str, models: &[String]) -> Vec<String> {
    if provider != "openai" {
        return models.to_vec();
    }

    let skip_prefixes = [
        "text-embedding",
        "tts-",
        "whisper-",
        "dall-e",
        "davinci",
        "babbage",
        "curie",
        "ada",
    ];
    let skip_contains = [
        "embedding",
        "moderation",
        "realtime",
        "transcribe",
        "search",
    ];

    models
        .iter()
        .filter(|m| {
            let lower = m.to_lowercase();
            !skip_prefixes.iter().any(|p| lower.starts_with(p))
                && !skip_contains.iter().any(|c| lower.contains(c))
        })
        .cloned()
        .collect()
}

/// Known models per provider (fallback when API is unavailable).
fn known_models(provider: &str) -> Vec<String> {
    let list: &[&str] = match provider {
        "openai" => &[
            // GPT-5.4 (latest frontier, March 2026)
            "gpt-5.4",
            "gpt-5.4-pro",
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            // GPT-5.x
            "gpt-5.3-codex",
            "gpt-5.3-codex-spark",
            "gpt-5.2",
            "gpt-5.2-pro",
            "gpt-5.2-codex",
            "gpt-5.1",
            "gpt-5.1-codex-max",
            "gpt-5.1-codex",
            "gpt-5.1-codex-mini",
            "gpt-5",
            "gpt-5-pro",
            "gpt-5-mini",
            "gpt-5-nano",
            "gpt-5-codex",
            "gpt-5-codex-mini",
            // O-series reasoning
            "o3",
            "o3-pro",
            "o3-mini",
            "o4-mini",
            // GPT-4.x
            "gpt-4.1",
            "gpt-4.1-mini",
            "gpt-4.1-nano",
            "gpt-4o",
            "gpt-4o-mini",
            // Open-weight
            "gpt-oss-120b",
            "gpt-oss-20b",
        ],
        "anthropic" => &[
            // Claude 4.6 (latest, March 2026)
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            // Claude 4.5
            "claude-opus-4-5-20251101",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
            // Claude 4.0
            "claude-opus-4-20250514",
            "claude-sonnet-4-20250514",
            // Claude 4.1
            "claude-opus-4-1-20250805",
        ],
        "github-copilot" => &[
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.3-codex",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4.1",
            "claude-sonnet-4",
            "o3-mini",
            "o4-mini",
        ],
        "openrouter" => &[
            "openai/gpt-5.4",
            "openai/gpt-5.4-mini",
            "openai/gpt-4o",
            "openai/gpt-4o-mini",
            "anthropic/claude-opus-4-6",
            "anthropic/claude-sonnet-4-6",
            "anthropic/claude-sonnet-4-20250514",
            "google/gemini-2.5-pro",
        ],
        "deepseek" => &["deepseek-v4-pro", "deepseek-v4-flash"],
        "volcano" => return coding_plan::volcano_models(),
        _ => &[],
    };
    list.iter().map(|s| s.to_string()).collect()
}

/// List available models from the active provider.
async fn model_list() -> anyhow::Result<()> {
    let config = EliConfig::load();
    let ctx = RequestContext::from_active(&config)?;

    println!(
        "Fetching models from {} ({})...",
        ctx.provider, ctx.api_base
    );
    println!();

    let mut models = fetch_or_fallback_models(&ctx).await;
    models.sort();
    models.dedup();

    print_model_list(&ctx.provider, &models, &config);
    Ok(())
}

async fn fetch_or_fallback_models(ctx: &RequestContext) -> Vec<String> {
    match fetch_models(ctx).await {
        Ok(m) => {
            let filtered = filter_chat_models(&ctx.provider, &m);
            if filtered.is_empty() { m } else { filtered }
        }
        Err(e) => {
            eprintln!("  (API fetch failed: {e})");
            let fallback = known_models(&ctx.provider);
            if fallback.is_empty() {
                eprintln!(
                    "  No fallback model list registered for '{}'.",
                    ctx.provider
                );
            } else {
                eprintln!("  Showing known models instead.");
            }
            eprintln!();
            fallback
        }
    }
}

fn print_model_list(provider: &str, models: &[String], config: &EliConfig) {
    if models.is_empty() {
        println!("No models found.");
        return;
    }
    println!("Available models ({provider}):");
    for model in models {
        let is_active = config
            .active_profile()
            .is_some_and(|p| p.model == *model || p.model == format!("{provider}:{model}"));
        let marker = if is_active { " *" } else { "" };
        println!("  {model}{marker}");
    }
}

/// Switch the active profile's model.
fn model_switch(model_name: &str) -> anyhow::Result<()> {
    let mut config = EliConfig::load();

    let profile_name = config.active_profile.clone().ok_or_else(|| {
        anyhow::anyhow!("No active profile configured.\nRun `eli login <provider>` to get started.")
    })?;

    let profile = config
        .profiles
        .get_mut(&profile_name)
        .ok_or_else(|| anyhow::anyhow!("Active profile '{profile_name}' not found in config"))?;

    let old_model = profile.model.clone();
    let provider = normalized_provider_name(&profile.provider);
    // Treat the part before `:` as a provider prefix only if it normalizes to
    // a real built-in provider. Ollama-style model tags (`llama3.2:3b`) carry
    // colons in the *model name* — without this guard we'd store the tag as
    // a phantom provider and the runtime would fail to route the request.
    let new_model = match model_name.split_once(':') {
        Some((prefix, _)) if is_known_provider(prefix) => model_name.to_string(),
        _ => format!("{provider}:{model_name}"),
    };
    profile.model = new_model.clone();
    config.save()?;

    println!("Model updated for profile '{profile_name}':");
    println!("  {old_model} -> {new_model}");

    Ok(())
}
