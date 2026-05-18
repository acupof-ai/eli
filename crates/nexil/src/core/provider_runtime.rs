use serde_json::Value;

use super::api_format::ApiFormat;
use super::errors::{ConduitError, ErrorKind};
use super::provider_policies;
use super::provider_registry::ProviderRegistry;
use crate::clients::parsing::TransportKind;

pub struct ProviderRuntime<'a> {
    provider_name: &'a str,
    model_id: &'a str,
    api_key: Option<&'a str>,
    explicit_api_base: Option<&'a str>,
    api_format: ApiFormat,
    registry: Option<&'a ProviderRegistry>,
}

impl<'a> ProviderRuntime<'a> {
    pub fn new(
        provider_name: &'a str,
        model_id: &'a str,
        api_key: Option<&'a str>,
        explicit_api_base: Option<&'a str>,
        api_format: ApiFormat,
    ) -> Self {
        Self {
            provider_name,
            model_id,
            api_key,
            explicit_api_base,
            api_format,
            registry: None,
        }
    }

    /// Attach a provider registry for transport and API base lookups.
    pub fn with_registry(mut self, registry: &'a ProviderRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Resolve the effective `ApiFormat` by checking the registry first,
    /// then falling back to the explicitly configured format.
    fn effective_api_format(&self) -> ApiFormat {
        // Explicit (non-Auto) format always wins — the caller specifically asked for it.
        if self.api_format != ApiFormat::Auto {
            return self.api_format;
        }
        // Check the registry for a provider-level preference.
        if let Some(reg) = self.registry
            && let Some(cfg) = reg.get(self.provider_name)
            && cfg.api_format != ApiFormat::Auto
        {
            return cfg.api_format;
        }
        ApiFormat::Auto
    }

    pub fn selected_transport(
        &self,
        tools_payload: Option<&[Value]>,
        supports_responses: bool,
        preferred_transport: Option<TransportKind>,
    ) -> Result<TransportKind, ConduitError> {
        if let Some(forced) = preferred_transport {
            return Ok(forced);
        }

        let format = self.effective_api_format();
        match format {
            ApiFormat::Completion => Ok(TransportKind::Completion),
            ApiFormat::Messages => self.require_messages(),
            ApiFormat::Responses => self.require_responses(tools_payload, supports_responses),
            ApiFormat::Auto => {
                if provider_policies::supports_messages_format(self.provider_name, self.model_id) {
                    return Ok(TransportKind::Messages);
                }
                self.require_responses(tools_payload, supports_responses)
                    .or(Ok(TransportKind::Completion))
            }
        }
    }

    pub fn resolved_api_base(&self) -> String {
        if let Some(explicit) = self.explicit_api_base {
            return explicit.to_owned();
        }

        if self.uses_openai_codex_backend() {
            return "https://chatgpt.com/backend-api/codex".to_owned();
        }

        // Check the registry for a provider-level default.
        if let Some(reg) = self.registry
            && let Some(cfg) = reg.get(self.provider_name)
        {
            return cfg.api_base.clone();
        }

        Self::default_api_base(self.provider_name).to_owned()
    }

    pub fn is_anthropic_oauth(&self) -> bool {
        self.provider_name.eq_ignore_ascii_case("anthropic")
            && self
                .api_key
                .is_some_and(|key| key.starts_with("sk-ant-oat"))
    }

    pub fn should_include_completion_stream_usage(provider_name: &str) -> bool {
        provider_policies::should_include_completion_stream_usage(provider_name)
    }

    pub fn completion_max_tokens_arg(provider_name: &str) -> String {
        provider_policies::completion_max_tokens_arg(provider_name)
    }

    pub fn default_api_base(provider_name: &str) -> &'static str {
        match provider_name.trim().to_lowercase().as_str() {
            "anthropic" => "https://api.anthropic.com/v1",
            "openai" => "https://api.openai.com/v1",
            "openrouter" => "https://openrouter.ai/api/v1",
            "github-copilot" => "https://api.githubcopilot.com",
            "deepseek" | "deepseek-v4" | "deepseek_v4" | "ds-v4" | "dsv4" => {
                super::provider_policies::DEEPSEEK_OPENAI_BASE
            }
            "volcano" => super::provider_policies::VOLCANO_CODING_OPENAI_BASE,
            _ => "https://api.openai.com/v1",
        }
    }

    fn require_messages(&self) -> Result<TransportKind, ConduitError> {
        if !provider_policies::supports_messages_format(self.provider_name, self.model_id) {
            return Err(ConduitError::new(
                ErrorKind::InvalidInput,
                format!(
                    "{}:{}: messages format is only valid for Anthropic models",
                    self.provider_name, self.model_id
                ),
            ));
        }
        Ok(TransportKind::Messages)
    }

    fn require_responses(
        &self,
        tools_payload: Option<&[Value]>,
        supports_responses: bool,
    ) -> Result<TransportKind, ConduitError> {
        let has_tools = tools_payload.is_some_and(|tools| !tools.is_empty());
        if let Some(reason) = provider_policies::responses_rejection_reason(
            self.provider_name,
            self.model_id,
            has_tools,
            supports_responses,
        ) {
            return Err(ConduitError::new(
                ErrorKind::InvalidInput,
                format!("{}:{}: {}", self.provider_name, self.model_id, reason),
            ));
        }
        Ok(TransportKind::Responses)
    }

    fn uses_openai_codex_backend(&self) -> bool {
        self.provider_name.eq_ignore_ascii_case("openai")
            && self.api_key.is_some_and(|key| key.starts_with("eyJ"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::provider_registry::ProviderConfig;
    use super::*;

    #[test]
    fn test_auto_uses_messages_for_anthropic() {
        let runtime = ProviderRuntime::new(
            "anthropic",
            "claude-sonnet-4-6",
            None,
            None,
            ApiFormat::Auto,
        );
        let transport = runtime.selected_transport(None, false, None).unwrap();
        assert_eq!(transport, TransportKind::Messages);
    }

    #[test]
    fn test_auto_falls_back_to_completion_when_responses_are_rejected() {
        let runtime = ProviderRuntime::new("unknown", "custom-model", None, None, ApiFormat::Auto);
        let transport = runtime.selected_transport(None, false, None).unwrap();
        assert_eq!(transport, TransportKind::Completion);
    }

    #[test]
    fn test_codex_oauth_uses_chatgpt_backend_when_base_is_not_explicit() {
        let runtime = ProviderRuntime::new(
            "openai",
            "gpt-5.4",
            Some("eyJ.mock.jwt"),
            None,
            ApiFormat::Auto,
        );
        assert_eq!(
            runtime.resolved_api_base(),
            "https://chatgpt.com/backend-api/codex"
        );
    }

    #[test]
    fn test_default_api_base_deepseek_alias() {
        assert_eq!(
            ProviderRuntime::default_api_base("dsv4"),
            super::super::provider_policies::DEEPSEEK_OPENAI_BASE
        );
    }

    #[test]
    fn registry_api_format_overrides_auto() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "my-llm",
            ProviderConfig::new("https://api.my-llm.example.com/v1", ApiFormat::Completion),
        );

        let runtime = ProviderRuntime::new("my-llm", "some-model", None, None, ApiFormat::Auto)
            .with_registry(&reg);

        let transport = runtime.selected_transport(None, false, None).unwrap();
        assert_eq!(transport, TransportKind::Completion);
    }

    #[test]
    fn explicit_api_format_wins_over_registry() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "my-llm",
            ProviderConfig::new("https://api.my-llm.example.com/v1", ApiFormat::Responses),
        );

        // The caller explicitly requested Completion; the registry says Responses.
        // The explicit format must win.
        let runtime =
            ProviderRuntime::new("my-llm", "some-model", None, None, ApiFormat::Completion)
                .with_registry(&reg);

        let transport = runtime.selected_transport(None, false, None).unwrap();
        assert_eq!(transport, TransportKind::Completion);
    }

    #[test]
    fn registry_api_base_used_when_no_explicit_base() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "my-llm",
            ProviderConfig::new("https://api.my-llm.example.com/v1", ApiFormat::Auto),
        );

        let runtime = ProviderRuntime::new("my-llm", "some-model", None, None, ApiFormat::Auto)
            .with_registry(&reg);

        assert_eq!(
            runtime.resolved_api_base(),
            "https://api.my-llm.example.com/v1"
        );
    }

    #[test]
    fn explicit_api_base_wins_over_registry() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "my-llm",
            ProviderConfig::new("https://api.my-llm.example.com/v1", ApiFormat::Auto),
        );

        let runtime = ProviderRuntime::new(
            "my-llm",
            "some-model",
            None,
            Some("https://override.example.com/v1"),
            ApiFormat::Auto,
        )
        .with_registry(&reg);

        assert_eq!(
            runtime.resolved_api_base(),
            "https://override.example.com/v1"
        );
    }
}
