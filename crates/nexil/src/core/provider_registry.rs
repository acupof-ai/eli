//! Runtime provider registry for custom LLM provider configuration.
//!
//! The registry holds [`ProviderConfig`] entries keyed by provider name and is
//! consulted at request time for transport selection, API base resolution, and
//! custom headers. Built-in providers (OpenAI, Anthropic, OpenRouter, etc.) are
//! pre-populated; callers can override or add new entries via
//! [`ProviderRegistry::register`].

use std::collections::HashMap;

use super::api_format::ApiFormat;

/// Configuration for a custom or overridden LLM provider.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Default API base URL (e.g. `"https://api.example.com/v1"`).
    pub api_base: String,
    /// Preferred API format. `Auto` defers to the standard selection logic.
    pub api_format: ApiFormat,
    /// Additional HTTP headers sent with every request to this provider.
    pub custom_headers: HashMap<String, String>,
}

impl ProviderConfig {
    /// Create a new config with the given API base and format, no custom headers.
    pub fn new(api_base: impl Into<String>, api_format: ApiFormat) -> Self {
        Self {
            api_base: api_base.into(),
            api_format,
            custom_headers: HashMap::new(),
        }
    }

    /// Builder-style: add a custom header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.custom_headers.insert(name.into(), value.into());
        self
    }
}

/// Registry mapping provider names to their [`ProviderConfig`].
///
/// The registry is pre-populated with built-in providers. Custom providers
/// can be added (or built-in ones overridden) via [`register`](Self::register).
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, ProviderConfig>,
}

impl ProviderRegistry {
    /// Create a new registry pre-populated with built-in providers.
    pub fn new() -> Self {
        let mut providers = HashMap::new();

        providers.insert(
            "anthropic".to_owned(),
            ProviderConfig::new("https://api.anthropic.com/v1", ApiFormat::Messages),
        );
        providers.insert(
            "openai".to_owned(),
            ProviderConfig::new("https://api.openai.com/v1", ApiFormat::Auto),
        );
        providers.insert(
            "openrouter".to_owned(),
            ProviderConfig::new("https://openrouter.ai/api/v1", ApiFormat::Auto),
        );
        providers.insert(
            "github-copilot".to_owned(),
            ProviderConfig::new("https://api.githubcopilot.com", ApiFormat::Auto),
        );
        providers.insert(
            "deepseek".to_owned(),
            ProviderConfig::new(
                super::provider_policies::DEEPSEEK_OPENAI_BASE,
                ApiFormat::Completion,
            ),
        );
        providers.insert(
            "volcano".to_owned(),
            ProviderConfig::new(
                super::provider_policies::VOLCANO_CODING_OPENAI_BASE,
                ApiFormat::Completion,
            ),
        );
        // Generic local OpenAI-Chat-Completions-compatible backend.
        // One slot covers agent-infer / ollama / vllm / lmstudio / llama.cpp /
        // any other keyless local server — the brand only differs in the
        // saved profile label and the default port we autodetect against.
        // Per-profile `api_base` is the source of truth at request time;
        // this default is just a fallback for profiles missing api_base.
        providers.insert(
            "local".to_owned(),
            ProviderConfig::new("http://127.0.0.1:8000/v1", ApiFormat::Completion),
        );

        Self { providers }
    }

    /// Register (or overwrite) a provider configuration.
    pub fn register(&mut self, name: impl Into<String>, config: ProviderConfig) {
        self.providers.insert(name.into(), config);
    }

    /// Look up a provider by name. Aliases (`agent-infer`, `ollama`,
    /// `claude`, …) resolve via [`normalized_provider_name`] so callers
    /// can use whatever string the user typed.
    pub fn get(&self, name: &str) -> Option<&ProviderConfig> {
        let key = super::provider_policies::normalized_provider_name(name);
        self.providers.get(&key)
    }

    /// Return `true` if the registry contains a config for `name`
    /// (alias-aware, mirroring [`Self::get`]).
    pub fn contains(&self, name: &str) -> bool {
        let key = super::provider_policies::normalized_provider_name(name);
        self.providers.contains_key(&key)
    }

    /// Iterate over all registered `(name, config)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ProviderConfig)> {
        self.providers.iter()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_providers_are_populated() {
        let reg = ProviderRegistry::new();
        assert!(reg.contains("anthropic"));
        assert!(reg.contains("openai"));
        assert!(reg.contains("openrouter"));
        assert!(reg.contains("github-copilot"));
        assert!(reg.contains("local"));
        assert!(reg.contains("deepseek"));
        assert!(reg.contains("dsv4"));
        assert!(reg.contains("volcano"));
        assert!(!reg.contains("custom-provider"));
    }

    #[test]
    fn local_provider_defaults() {
        let reg = ProviderRegistry::new();
        let cfg = reg.get("local").expect("local registered");
        assert_eq!(cfg.api_base, "http://127.0.0.1:8000/v1");
        assert_eq!(cfg.api_format, ApiFormat::Completion);
    }

    #[test]
    fn volcano_provider_defaults_to_coding_plan_chat_api() {
        let reg = ProviderRegistry::new();
        let cfg = reg.get("ark").expect("volcano alias registered");
        assert_eq!(
            cfg.api_base,
            crate::core::provider_policies::VOLCANO_CODING_OPENAI_BASE
        );
        assert_eq!(cfg.api_format, ApiFormat::Completion);
    }

    #[test]
    fn deepseek_provider_defaults_to_beta_chat_api() {
        let reg = ProviderRegistry::new();
        let cfg = reg.get("dsv4").expect("deepseek alias registered");
        assert_eq!(
            cfg.api_base,
            crate::core::provider_policies::DEEPSEEK_OPENAI_BASE
        );
        assert_eq!(cfg.api_format, ApiFormat::Completion);
    }

    #[test]
    fn register_custom_provider() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "my-llm",
            ProviderConfig::new("https://api.my-llm.example.com/v1", ApiFormat::Completion),
        );
        let cfg = reg.get("my-llm").expect("should find custom provider");
        assert_eq!(cfg.api_base, "https://api.my-llm.example.com/v1");
        assert_eq!(cfg.api_format, ApiFormat::Completion);
    }

    #[test]
    fn override_builtin_provider() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "openai",
            ProviderConfig::new(
                "https://my-openai-proxy.example.com/v1",
                ApiFormat::Responses,
            ),
        );
        let cfg = reg.get("openai").unwrap();
        assert_eq!(cfg.api_base, "https://my-openai-proxy.example.com/v1");
        assert_eq!(cfg.api_format, ApiFormat::Responses);
    }

    #[test]
    fn case_insensitive_lookup() {
        let reg = ProviderRegistry::new();
        assert!(reg.get("OpenAI").is_some());
        assert!(reg.get("ANTHROPIC").is_some());
    }

    #[test]
    fn custom_headers() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "custom",
            ProviderConfig::new("https://api.custom.example.com", ApiFormat::Completion)
                .with_header("X-Custom-Token", "secret"),
        );
        let cfg = reg.get("custom").unwrap();
        assert_eq!(cfg.custom_headers.get("X-Custom-Token").unwrap(), "secret");
    }

    #[test]
    fn iter_over_providers() {
        let reg = ProviderRegistry::new();
        let names: Vec<_> = reg.iter().map(|(name, _)| name.clone()).collect();
        assert!(names.contains(&"openai".to_owned()));
        assert!(names.contains(&"anthropic".to_owned()));
    }
}
