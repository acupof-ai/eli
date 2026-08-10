//! Provider policy decisions shared across request paths.

use serde::{Deserialize, Serialize};

pub const DEEPSEEK_OPENAI_BASE: &str = "https://api.deepseek.com/beta";
pub const VOLCANO_CODING_OPENAI_BASE: &str = "https://ark.cn-beijing.volces.com/api/coding/v3";

/// Provider-specific behavioural toggles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderPolicy {
    pub enable_responses_without_capability: bool,
    pub include_usage_in_completion_stream: bool,
    pub completion_max_tokens_arg: String,
    pub responses_tools_blocked_model_prefixes: Vec<String>,
}

impl Default for ProviderPolicy {
    fn default() -> Self {
        Self {
            enable_responses_without_capability: false,
            include_usage_in_completion_stream: false,
            completion_max_tokens_arg: "max_tokens".to_owned(),
            responses_tools_blocked_model_prefixes: Vec::new(),
        }
    }
}

fn provider_name_key(provider_name: &str) -> String {
    provider_name.trim().to_lowercase()
}

fn provider_alias(provider_name: &str) -> String {
    match provider_name_key(provider_name).as_str() {
        "claude" | "anthropic" => "anthropic".to_owned(),
        "copilot" | "github-copilot" => "github-copilot".to_owned(),
        // All keyless OpenAI-Chat-Completions-compatible local servers
        // (agent-infer, ollama, vllm, lmstudio, llama.cpp, …) collapse onto
        // a single `local` provider. Brand names survive only as the
        // user-facing profile label.
        "local" | "agent-infer" | "agent_infer" | "agentinfer" | "ollama" | "vllm" | "lmstudio"
        | "llama-cpp" | "llamacpp" | "llama.cpp" => "local".to_owned(),
        "deepseek" | "deepseek-v4" | "deepseek_v4" | "ds-v4" | "dsv4" => "deepseek".to_owned(),
        "volcano" | "volcengine" | "ark" => "volcano".to_owned(),
        other => other.to_owned(),
    }
}

/// Normalize provider aliases while preserving unknown provider names.
pub fn normalized_provider_name(provider_name: &str) -> String {
    provider_alias(provider_name)
}

/// Whether `name` (or one of its aliases) names a built-in provider.
///
/// Used by callers that need to decide whether a colon-bearing model id
/// (e.g. `"openai:gpt-5"` vs ollama's `"llama3.2:3b"` tag syntax) already
/// carries a provider prefix. The first segment is a real prefix only if
/// it normalizes to one of the canonical built-in providers.
pub fn is_known_provider(name: &str) -> bool {
    matches!(
        provider_alias(name).as_str(),
        "anthropic" | "openai" | "openrouter" | "github-copilot" | "local" | "deepseek" | "volcano"
    )
}

/// Default API base URL for a provider.
pub fn default_api_base(provider_name: &str) -> String {
    match provider_alias(provider_name).as_str() {
        "anthropic" => "https://api.anthropic.com/v1".to_owned(),
        "openai" => "https://api.openai.com/v1".to_owned(),
        "openrouter" => "https://openrouter.ai/api/v1".to_owned(),
        "github-copilot" => "https://api.githubcopilot.com".to_owned(),
        "deepseek" => DEEPSEEK_OPENAI_BASE.to_owned(),
        "volcano" => VOLCANO_CODING_OPENAI_BASE.to_owned(),
        // The first port across the canonical local-LLM defaults
        // (agent-infer/vllm:8000). Every saved profile is expected to carry
        // its own `api_base` from autodetection, so this is a fallback only.
        "local" => "http://127.0.0.1:8000/v1".to_owned(),
        other => format!("https://api.{other}.com/v1"),
    }
}

/// Default model for a given provider.
pub fn default_model_for_provider(provider_name: &str) -> &'static str {
    match provider_alias(provider_name).as_str() {
        "openai" => "openai:gpt-5.4-mini",
        "anthropic" => "anthropic:claude-sonnet-4-6",
        "github-copilot" => "github-copilot:gpt-5.4-mini",
        "deepseek" => "deepseek:deepseek-v4-pro",
        "volcano" => "volcano:ark-code-latest",
        // Local backends have no canonical model; the login flow queries
        // /v1/models and writes the real served model into the profile.
        // This placeholder is only used when no profile exists and the user
        // has not set ELI_MODEL — it matches the README's canonical example.
        "local" => "local:Qwen/Qwen3-4B",
        _ => "openrouter:openai/gpt-5.4-mini",
    }
}

/// Look up the policy for a given provider name.
///
/// Unknown providers fall back to the default policy.
pub fn provider_policy(provider_name: &str) -> ProviderPolicy {
    match provider_alias(provider_name).as_str() {
        "github-copilot" => ProviderPolicy {
            include_usage_in_completion_stream: true,
            completion_max_tokens_arg: "max_tokens".to_owned(),
            ..Default::default()
        },
        "openai" => ProviderPolicy {
            enable_responses_without_capability: true,
            include_usage_in_completion_stream: true,
            completion_max_tokens_arg: "max_completion_tokens".to_owned(),
            ..Default::default()
        },
        "openrouter" => ProviderPolicy {
            enable_responses_without_capability: true,
            include_usage_in_completion_stream: true,
            responses_tools_blocked_model_prefixes: vec!["anthropic/".to_owned()],
            ..Default::default()
        },
        "deepseek" => ProviderPolicy {
            include_usage_in_completion_stream: true,
            completion_max_tokens_arg: "max_tokens".to_owned(),
            ..Default::default()
        },
        "volcano" => ProviderPolicy {
            include_usage_in_completion_stream: true,
            completion_max_tokens_arg: "max_tokens".to_owned(),
            ..Default::default()
        },
        _ => ProviderPolicy::default(),
    }
}

fn responses_tools_blocked_for_model(provider_name: &str, model_id: &str) -> bool {
    let policy = provider_policy(provider_name);
    let lowered_model = model_id.trim().to_lowercase();
    policy
        .responses_tools_blocked_model_prefixes
        .iter()
        .any(|prefix| lowered_model.starts_with(prefix))
}

/// Return the reason why the responses format should be rejected, or `None`
/// if it is acceptable.
pub fn responses_rejection_reason(
    provider_name: &str,
    model_id: &str,
    has_tools: bool,
    supports_responses: bool,
) -> Option<String> {
    if has_tools && responses_tools_blocked_for_model(provider_name, model_id) {
        return Some(
            "responses format is not supported for this model when tools are enabled".to_owned(),
        );
    }
    if supports_responses {
        return None;
    }
    if provider_policy(provider_name).enable_responses_without_capability {
        return None;
    }
    Some("responses format is not supported by this provider".to_owned())
}

/// Whether the provider+model combination supports the messages format.
pub fn supports_messages_format(provider_name: &str, model_id: &str) -> bool {
    let normalized_provider = normalized_provider_name(provider_name);
    let normalized_model = model_id.trim().to_lowercase();
    normalized_provider == "anthropic" || normalized_model.starts_with("anthropic/")
}

/// Whether usage data should be included in the completion stream.
pub fn should_include_completion_stream_usage(provider_name: &str) -> bool {
    provider_policy(provider_name).include_usage_in_completion_stream
}

/// The argument name used for max tokens in completion requests.
pub fn completion_max_tokens_arg(provider_name: &str) -> String {
    provider_policy(provider_name).completion_max_tokens_arg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalized_provider_name() {
        assert_eq!(normalized_provider_name("claude"), "anthropic");
        assert_eq!(normalized_provider_name("copilot"), "github-copilot");
        assert_eq!(normalized_provider_name("openai"), "openai");
        assert_eq!(normalized_provider_name(" Claude "), "anthropic");
        assert_eq!(normalized_provider_name("agent-infer"), "local");
        assert_eq!(normalized_provider_name("agent_infer"), "local");
        assert_eq!(normalized_provider_name("AgentInfer"), "local");
        assert_eq!(normalized_provider_name("ollama"), "local");
        assert_eq!(normalized_provider_name("vllm"), "local");
        assert_eq!(normalized_provider_name("lmstudio"), "local");
        assert_eq!(normalized_provider_name("llama-cpp"), "local");
        assert_eq!(normalized_provider_name("llamacpp"), "local");
        assert_eq!(normalized_provider_name("local"), "local");
        assert_eq!(normalized_provider_name("dsv4"), "deepseek");
        assert_eq!(normalized_provider_name("deepseek-v4"), "deepseek");
        assert_eq!(normalized_provider_name("volcengine"), "volcano");
        assert_eq!(normalized_provider_name("ark"), "volcano");
    }

    #[test]
    fn test_is_known_provider() {
        // Built-in canonicals + their aliases all return true.
        assert!(is_known_provider("openai"));
        assert!(is_known_provider("anthropic"));
        assert!(is_known_provider("claude"));
        assert!(is_known_provider("github-copilot"));
        assert!(is_known_provider("copilot"));
        assert!(is_known_provider("openrouter"));
        assert!(is_known_provider("local"));
        assert!(is_known_provider("deepseek"));
        assert!(is_known_provider("dsv4"));
        assert!(is_known_provider("volcano"));
        assert!(is_known_provider("ark"));
        assert!(is_known_provider("ollama"));
        assert!(is_known_provider("llama-cpp"));
        // The "prefix" part of an ollama tag like "llama3.2:3b" is NOT a provider.
        assert!(!is_known_provider("llama3.2"));
        assert!(!is_known_provider("qwen2.5"));
        assert!(!is_known_provider("custom"));
    }

    #[test]
    fn test_default_api_base() {
        assert_eq!(default_api_base("claude"), "https://api.anthropic.com/v1");
        assert_eq!(default_api_base("copilot"), "https://api.githubcopilot.com");
        assert_eq!(default_api_base("cohere"), "https://api.cohere.com/v1");
        assert_eq!(default_api_base("local"), "http://127.0.0.1:8000/v1");
        assert_eq!(default_api_base("agent-infer"), "http://127.0.0.1:8000/v1");
        assert_eq!(default_api_base("ollama"), "http://127.0.0.1:8000/v1");
        assert_eq!(default_api_base("vllm"), "http://127.0.0.1:8000/v1");
        assert_eq!(default_api_base("deepseek"), DEEPSEEK_OPENAI_BASE);
        assert_eq!(default_api_base("dsv4"), DEEPSEEK_OPENAI_BASE);
        assert_eq!(default_api_base("volcano"), VOLCANO_CODING_OPENAI_BASE);
    }

    #[test]
    fn test_default_model_for_provider() {
        assert_eq!(
            default_model_for_provider("claude"),
            "anthropic:claude-sonnet-4-6"
        );
        assert_eq!(
            default_model_for_provider("copilot"),
            "github-copilot:gpt-5.4-mini"
        );
        assert_eq!(
            default_model_for_provider("cohere"),
            "openrouter:openai/gpt-5.4-mini"
        );
        assert_eq!(
            default_model_for_provider("volcengine"),
            "volcano:ark-code-latest"
        );
        assert_eq!(
            default_model_for_provider("dsv4"),
            "deepseek:deepseek-v4-pro"
        );
    }
}
