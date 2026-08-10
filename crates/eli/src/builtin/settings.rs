//! Agent configuration loaded from environment variables.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

pub use nexil::core::execution::ProviderValue;
pub use nexil::llm::ApiFormat;

/// Default model identifier.
pub const DEFAULT_MODEL: &str = "openrouter:qwen/qwen3-coder-next";
/// Fallback maximum output tokens when we cannot infer from the model name.
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 65_536;
/// Fallback context window when we cannot infer from the model name.
pub const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

use super::model_specs::{infer_context_window, infer_max_output_tokens};

/// Return the default home directory (`~/.eli`).
fn default_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".eli")
}

fn api_format_from_str_lossy(s: &str) -> ApiFormat {
    match s.trim().to_lowercase().as_str() {
        "auto" => ApiFormat::Auto,
        "responses" => ApiFormat::Responses,
        "messages" => ApiFormat::Messages,
        "completion" => ApiFormat::Completion,
        _ => ApiFormat::Auto,
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    env::var(key).ok().and_then(|v| v.parse().ok())
}

fn parse_fallback_models() -> Option<Vec<String>> {
    env::var("ELI_FALLBACK_MODELS").ok().map(|v| {
        v.split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Centralized env-var resolution
// ---------------------------------------------------------------------------

/// Centralized environment variable resolution for eli configuration.
///
/// Provides a single source of truth for how each `ELI_*` env var is resolved
/// and what its precedence is.  Call sites should prefer these helpers over
/// raw `std::env::var` so that precedence rules are documented in one place.
///
/// # Recognized environment variables
///
/// | Variable                       | Purpose                                       |
/// |--------------------------------|-----------------------------------------------|
/// | `ELI_HOME`                     | Override the eli home directory (`~/.eli`)     |
/// | `ELI_MODEL`                    | Model identifier (`provider:model`)            |
/// | `ELI_FALLBACK_MODELS`          | Comma-separated fallback model list            |
/// | `ELI_API_KEY`                  | Single API key for all providers               |
/// | `ELI_API_BASE`                 | Single API base URL for all providers          |
/// | `ELI_<PROVIDER>_API_KEY`       | Per-provider API key                           |
/// | `ELI_<PROVIDER>_API_BASE`      | Per-provider API base URL                      |
/// | `ELI_API_FORMAT`               | API wire format (`auto`/`messages`/`responses`/`completion`) |
/// | `ELI_MAX_STEPS`                | Maximum agent steps per turn (default 50)      |
/// | `ELI_MAX_TOKENS`               | Max output tokens (auto-detected from model)   |
/// | `ELI_MODEL_TIMEOUT_SECONDS`    | HTTP timeout for model calls                   |
/// | `ELI_CONTEXT_WINDOW`           | Context window override (auto-detected)        |
/// | `ELI_MAX_TURN_BUDGET`          | Per-turn token budget; loop stops when reached |
/// | `ELI_VERBOSE`                  | Verbosity level (0–2)                          |
/// | `ELI_TELEGRAM_TOKEN`           | Telegram bot token (gateway mode; alias `SIDECAR_TELEGRAM_TOKEN`) |
/// | `ELI_TELEGRAM_ALLOW_USERS`     | Comma-separated Telegram user-id allowlist     |
/// | `ELI_TELEGRAM_ALLOW_CHATS`     | Comma-separated Telegram chat-id allowlist     |
/// | `ELI_FEISHU_ACCOUNT`           | Feishu account id for lark-cli (default `default`) |
pub struct EnvConfig;

impl EnvConfig {
    /// Resolve the model identifier.
    ///
    /// Precedence: `ELI_MODEL` env > config file active profile > [`DEFAULT_MODEL`].
    pub fn model(config: &crate::builtin::config::EliConfig) -> String {
        env::var("ELI_MODEL")
            .ok()
            .or_else(|| config.resolve_model())
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned())
    }

    /// Resolve the API key configuration (single or per-provider).
    ///
    /// Precedence: `ELI_API_KEY` (single) or `ELI_<PROVIDER>_API_KEY` (per-provider).
    /// See [`resolve_api_credentials`] for the full algorithm.
    pub fn api_credentials() -> (ProviderValue, ProviderValue) {
        resolve_api_credentials()
    }

    /// Resolve the API key as a single optional string.
    ///
    /// Precedence: explicit parameter > `ELI_API_KEY` env.
    pub fn api_key(explicit: Option<&str>) -> Option<String> {
        explicit
            .map(String::from)
            .or_else(|| env::var("ELI_API_KEY").ok())
    }

    /// Read `ELI_MODEL` from the environment, if set.
    ///
    /// Useful for CLI display (e.g. "ELI_MODEL overrides your profile").
    pub fn model_override() -> Option<String> {
        env::var("ELI_MODEL").ok()
    }
}

// ---------------------------------------------------------------------------
// API credential resolution
// ---------------------------------------------------------------------------

/// Resolve API key and base URL from environment variables.
///
/// Supports both single-value (`ELI_API_KEY` / `ELI_API_BASE`) and per-provider
/// (`ELI_<PROVIDER>_API_KEY` / `ELI_<PROVIDER>_API_BASE`) patterns.
fn resolve_api_credentials() -> (ProviderValue, ProviderValue) {
    let single_key = env::var("ELI_API_KEY").ok();
    let single_base = env::var("ELI_API_BASE").ok();

    if let (Some(key), Some(base)) = (single_key.clone(), single_base.clone()) {
        return (ProviderValue::Single(key), ProviderValue::Single(base));
    }

    let mut key_map: HashMap<String, String> = HashMap::new();
    let mut base_map: HashMap<String, String> = HashMap::new();

    if let Some(k) = single_key {
        key_map.insert("default".to_owned(), k);
    }
    if let Some(b) = single_base {
        base_map.insert("default".to_owned(), b);
    }

    for (key, value) in env::vars() {
        if let Some(provider) = key
            .strip_prefix("ELI_")
            .and_then(|rest| rest.strip_suffix("_API_KEY"))
            && provider != "API"
        {
            key_map.insert(provider.to_lowercase(), value.clone());
        }
        if let Some(provider) = key
            .strip_prefix("ELI_")
            .and_then(|rest| rest.strip_suffix("_API_BASE"))
            && provider != "API"
        {
            base_map.insert(provider.to_lowercase(), value);
        }
    }

    let api_key = collapse_config_map(
        key_map,
        ProviderValue::None,
        ProviderValue::Single,
        ProviderValue::PerProvider,
    );
    let api_base = collapse_config_map(
        base_map,
        ProviderValue::None,
        ProviderValue::Single,
        ProviderValue::PerProvider,
    );
    (api_key, api_base)
}

/// Merge the active profile's `api_base` override into the env-resolved
/// [`ProviderValue`].
///
/// Precedence: any env-supplied `api_base` wins. When env did not target
/// this profile's provider, the profile override is inserted so local
/// inference servers (e.g. agent-infer at `127.0.0.1:8000`) are reachable
/// without requiring users to duplicate the URL in `ELI_<PROVIDER>_API_BASE`.
fn merge_profile_api_base(
    current: ProviderValue,
    provider: Option<&str>,
    profile_api_base: Option<&str>,
) -> ProviderValue {
    let (Some(provider), Some(profile_api_base)) = (provider, profile_api_base) else {
        return current;
    };
    let provider_key =
        nexil::core::provider_policies::normalized_provider_name(provider).to_lowercase();

    match current {
        ProviderValue::Single(s) => ProviderValue::Single(s),
        ProviderValue::None => {
            let mut map = HashMap::new();
            map.insert(provider_key, profile_api_base.to_owned());
            ProviderValue::PerProvider(map)
        }
        ProviderValue::PerProvider(mut map) => {
            map.entry(provider_key)
                .or_insert_with(|| profile_api_base.to_owned());
            ProviderValue::PerProvider(map)
        }
    }
}

/// Collapse a `HashMap` into a config enum: empty → `none`, single "default"
/// entry → `single(value)`, otherwise → `per_provider(map)`.
fn collapse_config_map<T>(
    mut map: HashMap<String, String>,
    none: T,
    single: fn(String) -> T,
    per_provider: fn(HashMap<String, String>) -> T,
) -> T {
    if map.is_empty() {
        none
    } else if map.len() == 1 && map.contains_key("default") {
        // SAFETY: guarded by `map.contains_key("default")` check above
        single(
            map.remove("default")
                .expect("SAFETY: key presence verified"),
        )
    } else {
        per_provider(map)
    }
}

// ---------------------------------------------------------------------------
// AgentSettings
// ---------------------------------------------------------------------------

/// Agent configuration, loaded from environment variables with the `ELI_` prefix
/// and optionally from a `.env` file via `dotenvy`.
#[derive(Debug, Clone)]
pub struct AgentSettings {
    pub home: PathBuf,
    pub model: String,
    pub fallback_models: Option<Vec<String>>,
    pub api_key: ProviderValue,
    pub api_base: ProviderValue,
    pub api_format: ApiFormat,
    pub max_steps: usize,
    pub max_tokens: usize,
    pub model_timeout_seconds: Option<u64>,
    pub verbose: u8,
    /// Context window size in tokens. Auto-detected from model name if not set
    /// via `ELI_CONTEXT_WINDOW`.
    pub context_window: usize,
    /// Optional per-turn token budget (input + output across all tool-loop
    /// rounds). Set via `ELI_MAX_TURN_BUDGET`; `None` (default) means unlimited.
    /// Acts as a cost circuit breaker against runaway tool loops.
    pub max_turn_tokens: Option<u64>,
}

impl AgentSettings {
    /// Load settings from environment (and `.env` file).
    ///
    /// Per-provider API keys are detected via `ELI_<PROVIDER>_API_KEY` and
    /// `ELI_<PROVIDER>_API_BASE` patterns, matching the Python implementation.
    pub fn from_env() -> Self {
        let home = env::var("ELI_HOME")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(default_home);

        let config = crate::builtin::config::EliConfig::load();
        let model = EnvConfig::model(&config);
        let (api_key, api_base) = EnvConfig::api_credentials();
        // Profile-level api_base fills in for providers the env did not
        // explicitly target. Env-supplied values always win. We merge **every**
        // configured profile's api_base, not just the active one, so a
        // fallback model on a different provider still reaches the right
        // endpoint (e.g. anthropic primary → deepseek fallback uses the
        // deepseek profile's api_base without needing ELI_DEEPSEEK_API_BASE).
        let mut api_base = merge_profile_api_base(
            api_base,
            config.resolve_provider().as_deref(),
            config.resolve_api_base().as_deref(),
        );
        for (provider, base) in config.profile_api_bases() {
            api_base = merge_profile_api_base(api_base, Some(&provider), Some(&base));
        }

        // Bug G: validate context_window so a misconfigured ELI_CONTEXT_WINDOW
        // (e.g. zero, absurdly large) doesn't cause handoff math to go haywire.
        const MIN_CONTEXT_WINDOW: usize = 1_000;
        const MAX_CONTEXT_WINDOW: usize = 10_000_000;
        let raw_context_window =
            env_parse("ELI_CONTEXT_WINDOW").unwrap_or_else(|| infer_context_window(&model));
        let context_window = raw_context_window.clamp(MIN_CONTEXT_WINDOW, MAX_CONTEXT_WINDOW);
        if context_window != raw_context_window {
            tracing::warn!(
                value = raw_context_window,
                clamped = context_window,
                min = MIN_CONTEXT_WINDOW,
                max = MAX_CONTEXT_WINDOW,
                "context_window out of valid range [1000, 10_000_000], clamped"
            );
        }

        // ELI_FALLBACK_MODELS takes precedence; otherwise auto-derive the
        // fallback chain from the other configured profiles so a quota /
        // outage on one provider rolls over to the next without operator
        // intervention. None when neither env nor config has a candidate.
        let fallback_models = parse_fallback_models().or_else(|| {
            let derived = config.default_fallback_models();
            if derived.is_empty() {
                None
            } else {
                Some(derived)
            }
        });

        Self {
            home,
            fallback_models,
            api_format: api_format_from_str_lossy(&env::var("ELI_API_FORMAT").unwrap_or_default()),
            max_steps: env_parse("ELI_MAX_STEPS").unwrap_or(50),
            max_tokens: env_parse("ELI_MAX_TOKENS")
                .unwrap_or_else(|| infer_max_output_tokens(&model)),
            model_timeout_seconds: env_parse("ELI_MODEL_TIMEOUT_SECONDS"),
            verbose: env_parse::<u8>("ELI_VERBOSE").unwrap_or(0).min(2),
            context_window,
            max_turn_tokens: env_parse("ELI_MAX_TURN_BUDGET"),
            api_key,
            api_base,
            model,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- ApiFormat tests ------------------------------------------------------

    #[test]
    fn test_api_format_from_str_lossy() {
        assert_eq!(
            api_format_from_str_lossy("completion"),
            ApiFormat::Completion
        );
        assert_eq!(api_format_from_str_lossy("auto"), ApiFormat::Auto);
        assert_eq!(api_format_from_str_lossy("responses"), ApiFormat::Responses);
        assert_eq!(api_format_from_str_lossy("messages"), ApiFormat::Messages);
        assert_eq!(api_format_from_str_lossy("RESPONSES"), ApiFormat::Responses);
        assert_eq!(api_format_from_str_lossy("unknown"), ApiFormat::Auto);
        assert_eq!(api_format_from_str_lossy(""), ApiFormat::Auto);
    }

    #[test]
    fn test_api_format_as_str() {
        assert_eq!(ApiFormat::Auto.as_str(), "auto");
        assert_eq!(ApiFormat::Completion.as_str(), "completion");
        assert_eq!(ApiFormat::Responses.as_str(), "responses");
        assert_eq!(ApiFormat::Messages.as_str(), "messages");
    }

    // -- ProviderValue / ProviderValue -----------------------------------------

    #[test]
    fn test_api_key_config_single() {
        let config = ProviderValue::Single("sk-test".into());
        match config {
            ProviderValue::Single(k) => assert_eq!(k, "sk-test"),
            _ => panic!("expected Single"),
        }
    }

    #[test]
    fn test_api_key_config_per_provider() {
        let mut map = HashMap::new();
        map.insert("openai".into(), "sk-openai".into());
        let config = ProviderValue::PerProvider(map);
        match config {
            ProviderValue::PerProvider(m) => assert_eq!(m["openai"], "sk-openai"),
            _ => panic!("expected PerProvider"),
        }
    }

    // -- merge_profile_api_base -----------------------------------------------

    #[test]
    fn test_merge_profile_api_base_into_none() {
        let merged = merge_profile_api_base(
            ProviderValue::None,
            Some("local"),
            Some("http://127.0.0.1:8000/v1"),
        );
        match merged {
            ProviderValue::PerProvider(m) => {
                assert_eq!(m["local"], "http://127.0.0.1:8000/v1");
            }
            _ => panic!("expected PerProvider"),
        }
    }

    #[test]
    fn test_merge_profile_api_base_preserves_env_per_provider() {
        let mut map = HashMap::new();
        map.insert("local".into(), "http://192.168.1.10:9000/v1".into());
        let merged = merge_profile_api_base(
            ProviderValue::PerProvider(map),
            Some("local"),
            Some("http://127.0.0.1:8000/v1"),
        );
        match merged {
            ProviderValue::PerProvider(m) => {
                // Env entry must win.
                assert_eq!(m["local"], "http://192.168.1.10:9000/v1");
            }
            _ => panic!("expected PerProvider"),
        }
    }

    #[test]
    fn test_merge_profile_api_base_single_is_pass_through() {
        // Global ELI_API_BASE is a broad directive; don't fight it.
        let merged = merge_profile_api_base(
            ProviderValue::Single("http://proxy.example.com/v1".into()),
            Some("local"),
            Some("http://127.0.0.1:8000/v1"),
        );
        match merged {
            ProviderValue::Single(s) => assert_eq!(s, "http://proxy.example.com/v1"),
            _ => panic!("expected Single"),
        }
    }

    #[test]
    fn test_merge_profile_api_base_normalizes_alias() {
        // Brand aliases ("agent-infer", "ollama", …) must land in the "local"
        // slot so the downstream provider lookup finds it.
        let merged = merge_profile_api_base(
            ProviderValue::None,
            Some("ollama"),
            Some("http://127.0.0.1:11434/v1"),
        );
        match merged {
            ProviderValue::PerProvider(m) => {
                assert!(m.contains_key("local"));
                assert_eq!(m["local"], "http://127.0.0.1:11434/v1");
            }
            _ => panic!("expected PerProvider"),
        }
    }

    // -- AgentSettings defaults -----------------------------------------------

    #[test]
    fn test_default_model_constant() {
        assert!(!DEFAULT_MODEL.is_empty());
    }

    // Documentary guard that the default stays positive. The value is a
    // compile-time constant, so the assert is constant by nature — allow the
    // lint rather than drop the invariant check.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn test_default_max_output_tokens_constant() {
        assert!(DEFAULT_MAX_OUTPUT_TOKENS > 0);
    }

    #[test]
    fn test_default_home_returns_path() {
        let home = default_home();
        assert!(home.ends_with(".eli"));
    }

    #[test]
    fn test_agent_settings_clone() {
        let settings = AgentSettings {
            home: PathBuf::from("/tmp"),
            model: "test-model".into(),
            fallback_models: Some(vec!["fallback1".into()]),
            api_key: ProviderValue::Single("sk-test".into()),
            api_base: ProviderValue::None,
            api_format: ApiFormat::Completion,
            max_steps: 10,
            max_tokens: 512,
            model_timeout_seconds: Some(30),
            verbose: 1,
            context_window: 128_000,
            max_turn_tokens: None,
        };
        let cloned = settings.clone();
        assert_eq!(cloned.model, "test-model");
        assert_eq!(cloned.max_steps, 10);
        assert_eq!(cloned.max_tokens, 512);
        assert_eq!(cloned.verbose, 1);
    }
}
