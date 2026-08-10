//! [`LLMBuilder`] — fluent builder for constructing an [`LLM`](super::LLM).

use std::collections::HashMap;

use crate::auth::APIKeyResolver;
use crate::core::api_format::ApiFormat;
use crate::core::errors::{ConduitError, ErrorKind};
use crate::core::execution::{LLMCore, OAuthTokenRefresher, ProviderValue};
use crate::core::provider_registry::{ProviderConfig, ProviderRegistry};
use crate::tape::{
    AsyncTapeManager, AsyncTapeStore, AsyncTapeStoreAdapter, InMemoryTapeStore, TapeContext,
};
use crate::tools::executor::ToolExecutor;

use super::{DEFAULT_MODEL, LLM};

// ---------------------------------------------------------------------------
// LLMBuilder
// ---------------------------------------------------------------------------

/// Builder for constructing an [`LLM`] instance.
pub struct LLMBuilder {
    model: Option<String>,
    provider: Option<String>,
    fallback_models: Option<Vec<String>>,
    max_retries: Option<u32>,
    api_key: Option<String>,
    api_key_map: Option<HashMap<String, String>>,
    api_key_resolver: Option<APIKeyResolver>,
    api_base: Option<String>,
    api_base_map: Option<HashMap<String, String>>,
    api_format: Option<ApiFormat>,
    verbose: Option<u32>,
    context: Option<TapeContext>,
    tape_store: Option<Box<dyn AsyncTapeStore + Send + Sync>>,
    spill_dir: Option<std::path::PathBuf>,
    context_window: Option<usize>,
    provider_registry: Option<ProviderRegistry>,
    oauth_refresher: Option<OAuthTokenRefresher>,
}

impl LLMBuilder {
    /// Create a new builder with all fields unset.
    pub fn new() -> Self {
        Self {
            model: None,
            provider: None,
            fallback_models: None,
            max_retries: None,
            api_key: None,
            api_key_map: None,
            api_key_resolver: None,
            api_base: None,
            api_base_map: None,
            api_format: None,
            verbose: None,
            context: None,
            tape_store: None,
            spill_dir: None,
            context_window: None,
            provider_registry: None,
            oauth_refresher: None,
        }
    }

    /// Set the model (e.g. `"openai:gpt-4o"`).
    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_owned());
        self
    }

    /// Set the provider explicitly (e.g. `"openai"`).
    pub fn provider(mut self, provider: &str) -> Self {
        self.provider = Some(provider.to_owned());
        self
    }

    /// Set fallback models to try when the primary model fails.
    pub fn fallback_models(mut self, models: Vec<String>) -> Self {
        self.fallback_models = Some(models);
        self
    }

    /// Set the maximum number of retries per model.
    pub fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = Some(retries);
        self
    }

    /// Set a single API key used for all providers.
    pub fn api_key(mut self, key: &str) -> Self {
        self.api_key = Some(key.to_owned());
        self
    }

    /// Set per-provider API keys.
    pub fn api_key_map(mut self, map: HashMap<String, String>) -> Self {
        self.api_key_map = Some(map);
        self
    }

    /// Set a dynamic API key resolver.
    pub fn api_key_resolver(mut self, resolver: APIKeyResolver) -> Self {
        self.api_key_resolver = Some(resolver);
        self
    }

    /// Set the API base URL.
    pub fn api_base(mut self, base: &str) -> Self {
        self.api_base = Some(base.to_owned());
        self
    }

    /// Set per-provider API base URLs.
    pub fn api_base_map(mut self, map: HashMap<String, String>) -> Self {
        self.api_base_map = Some(map);
        self
    }

    /// Set the API format explicitly.
    pub fn api_format(mut self, format: ApiFormat) -> Self {
        self.api_format = Some(format);
        self
    }

    /// Set the verbosity level (0, 1, or 2).
    pub fn verbose(mut self, level: u32) -> Self {
        self.verbose = Some(level);
        self
    }

    /// Set the tape context for conversation history selection.
    pub fn context(mut self, context: TapeContext) -> Self {
        self.context = Some(context);
        self
    }

    /// Set a custom tape store.
    pub fn tape_store(mut self, store: impl AsyncTapeStore + 'static) -> Self {
        self.tape_store = Some(Box::new(store));
        self
    }

    /// Set the directory for spilling large tool results to disk.
    pub fn spill_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.spill_dir = Some(dir.into());
        self
    }

    /// Set the model context window in tokens.
    pub fn context_window(mut self, tokens: usize) -> Self {
        self.context_window = Some(tokens);
        self
    }

    /// Replace the entire provider registry.
    pub fn provider_registry(mut self, registry: ProviderRegistry) -> Self {
        self.provider_registry = Some(registry);
        self
    }

    /// Register a single custom provider. If no registry has been set yet, one
    /// is created with the built-in defaults first.
    pub fn register_provider(mut self, name: impl Into<String>, config: ProviderConfig) -> Self {
        self.provider_registry
            .get_or_insert_with(ProviderRegistry::new)
            .register(name, config);
        self
    }

    /// Set an OAuth token refresher for automatic 401 retry.
    ///
    /// When a 401 Unauthorized is received, the refresher is called to obtain a
    /// new API key. The request is retried at most once with the refreshed token.
    pub fn oauth_refresher(mut self, refresher: OAuthTokenRefresher) -> Self {
        self.oauth_refresher = Some(refresher);
        self
    }

    /// Build the [`LLM`] instance.
    pub fn build(self) -> Result<LLM, ConduitError> {
        let verbose = self.verbose.unwrap_or(0);
        if verbose > 2 {
            return Err(ConduitError::new(
                ErrorKind::InvalidInput,
                "verbose must be 0, 1, or 2",
            ));
        }

        let max_retries = self.max_retries.unwrap_or(3);
        let model_str = self.model.as_deref().unwrap_or(DEFAULT_MODEL);

        let provider_str = self.provider.as_deref();

        let (resolved_provider, resolved_model) =
            LLMCore::resolve_model_provider(model_str, provider_str)?;

        let api_key_config = if let Some(key) = self.api_key {
            ProviderValue::Single(key)
        } else if let Some(resolver) = &self.api_key_resolver {
            // Call the resolver for the default provider at build time.
            if let Some(resolved_key) = resolver(&resolved_provider) {
                ProviderValue::Single(resolved_key)
            } else if let Some(map) = self.api_key_map {
                ProviderValue::PerProvider(map)
            } else {
                ProviderValue::None
            }
        } else if let Some(map) = self.api_key_map {
            ProviderValue::PerProvider(map)
        } else {
            ProviderValue::None
        };

        let api_base_config = match (self.api_base, self.api_base_map) {
            (Some(base), _) => ProviderValue::Single(base),
            (None, Some(map)) => ProviderValue::PerProvider(map),
            (None, None) => ProviderValue::None,
        };

        let api_format = self.api_format.unwrap_or_default();

        let mut core = LLMCore::new(
            resolved_provider,
            resolved_model,
            self.fallback_models.unwrap_or_default(),
            max_retries,
            api_key_config,
            api_base_config,
            api_format,
            verbose,
        );
        if let Some(registry) = self.provider_registry {
            core.set_provider_registry(registry);
        }
        if let Some(refresher) = self.oauth_refresher {
            core = core.with_oauth_refresher(refresher);
        }

        let context = self.context;

        let async_tape = if let Some(custom_store) = self.tape_store {
            AsyncTapeManager::new(Some(custom_store), context)
        } else {
            let shared_tape_store = InMemoryTapeStore::new();
            let async_store = AsyncTapeStoreAdapter::new(shared_tape_store);
            AsyncTapeManager::new(Some(Box::new(async_store)), context)
        };

        Ok(LLM {
            core,
            tool_executor: ToolExecutor::new(),
            async_tape,
            spill_dir: self.spill_dir,
            context_window: self.context_window,
        })
    }
}

impl Default for LLMBuilder {
    fn default() -> Self {
        Self::new()
    }
}
