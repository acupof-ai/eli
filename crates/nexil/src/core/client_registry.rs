//! Provider-specific client creation and caching helpers.

use std::collections::HashMap;
use std::sync::Arc;

use reqwest::Client;

/// A key that uniquely identifies a cached HTTP client instance.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ClientCacheKey {
    pub provider: String,
    pub api_key: Option<String>,
    pub api_base: Option<String>,
}

impl std::fmt::Debug for ClientCacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCacheKey")
            .field("provider", &self.provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl ClientCacheKey {
    pub fn new(provider: &str, api_key: Option<&str>, api_base: Option<&str>) -> Self {
        Self {
            provider: provider.to_owned(),
            api_key: api_key.map(|s| s.to_owned()),
            api_base: api_base.map(|s| s.to_owned()),
        }
    }
}

/// Configuration for creating provider HTTP clients.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub default_headers: HashMap<String, String>,
    pub timeout_secs: u64,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            default_headers: HashMap::new(),
            // Total-request timeout, streaming included. 600s: reasoning
            // models (deepseek-v4-flash etc.) can stream thinking for several
            // minutes; the old 120s default cut them mid-stream and surfaced
            // as "(model returned empty response)". Overridable via
            // ELI_MODEL_TIMEOUT_SECONDS.
            timeout_secs: std::env::var("ELI_MODEL_TIMEOUT_SECONDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(600),
        }
    }
}

/// Registry that caches `reqwest::Client` instances by (provider, key, base) tuple.
#[derive(Debug, Clone)]
pub struct ClientRegistry {
    cache: HashMap<ClientCacheKey, Arc<Client>>,
    config: ClientConfig,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            config: ClientConfig::default(),
        }
    }

    pub fn with_config(config: ClientConfig) -> Self {
        Self {
            cache: HashMap::new(),
            config,
        }
    }

    /// Get or create a reqwest `Client` for the given provider/key/base combination.
    pub fn get_or_create(
        &mut self,
        provider: &str,
        api_key: Option<&str>,
        api_base: Option<&str>,
    ) -> Arc<Client> {
        let cache_key = ClientCacheKey::new(provider, api_key, api_base);
        if let Some(client) = self.cache.get(&cache_key) {
            return Arc::clone(client);
        }

        let client = Arc::new(self.build_client(provider, api_key));
        self.cache.insert(cache_key, Arc::clone(&client));
        client
    }

    pub fn contains(&self, provider: &str, api_key: Option<&str>, api_base: Option<&str>) -> bool {
        let cache_key = ClientCacheKey::new(provider, api_key, api_base);
        self.cache.contains_key(&cache_key)
    }

    pub fn remove(&mut self, provider: &str, api_key: Option<&str>, api_base: Option<&str>) {
        let cache_key = ClientCacheKey::new(provider, api_key, api_base);
        self.cache.remove(&cache_key);
    }

    pub fn clear(&mut self) {
        self.cache.clear();
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    fn build_client(&self, provider: &str, api_key: Option<&str>) -> Client {
        let mut headers = self.default_headers();
        let is_anthropic = provider.eq_ignore_ascii_case("anthropic");
        let is_oauth_token =
            api_key.is_some_and(|k| k.to_ascii_lowercase().starts_with("sk-ant-oat"));
        // OpenAI Codex CLI uses a JWT (eyJ…) routed to chatgpt.com/backend-api/codex.
        // That backend forces streaming on every request and can run for many minutes on
        // complex tasks — it needs the same extended timeout as Anthropic OAuth tokens.
        let is_codex_jwt = api_key.is_some_and(|k| k.starts_with("eyJ"));

        Self::insert_auth_header(&mut headers, api_key, is_anthropic, is_oauth_token);

        if is_anthropic {
            Self::insert_anthropic_headers(&mut headers, is_oauth_token);
        }

        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        // Long-running backends (Anthropic OAuth extended thinking, OpenAI Codex JWT) force
        // streaming and can take several minutes per request. Use a much longer timeout so the
        // 120 s default does not cut off mid-stream after exactly 4 × 120 s = 480 s.
        let timeout_secs = if is_oauth_token || is_codex_jwt {
            self.config.timeout_secs.max(600)
        } else {
            self.config.timeout_secs
        };

        Client::builder()
            .default_headers(headers)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .unwrap_or_else(|err| {
                tracing::warn!(%err, provider, "failed to build HTTP client, falling back to default");
                Client::new()
            })
    }

    fn default_headers(&self) -> reqwest::header::HeaderMap {
        self.config
            .default_headers
            .iter()
            .filter_map(|(key, value)| {
                let name = reqwest::header::HeaderName::from_bytes(key.as_bytes()).ok()?;
                let val = reqwest::header::HeaderValue::from_str(value).ok()?;
                Some((name, val))
            })
            .collect()
    }

    fn insert_auth_header(
        headers: &mut reqwest::header::HeaderMap,
        api_key: Option<&str>,
        is_anthropic: bool,
        is_oauth_token: bool,
    ) {
        // Skip auth entirely when no key is supplied OR the key is empty/whitespace.
        // Local inference servers (agent-infer, Ollama, vLLM, LM Studio) expose
        // no-auth endpoints; sending a malformed `Bearer ` header causes them
        // to reject the request.
        let Some(key) = api_key.map(str::trim).filter(|k| !k.is_empty()) else {
            return;
        };
        if is_anthropic && !is_oauth_token {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(key)
                && let Ok(name) = reqwest::header::HeaderName::from_bytes(b"x-api-key")
            {
                headers.insert(name, val);
            }
        } else if let Ok(val) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
            headers.insert(reqwest::header::AUTHORIZATION, val);
        }
    }

    fn insert_anthropic_headers(headers: &mut reqwest::header::HeaderMap, is_oauth_token: bool) {
        if let Ok(val) = reqwest::header::HeaderValue::from_str("2023-06-01")
            && let Ok(name) = reqwest::header::HeaderName::from_bytes(b"anthropic-version")
        {
            headers.insert(name, val);
        }

        if is_oauth_token {
            Self::insert_oauth_impersonation_headers(headers);
        }
    }

    fn insert_oauth_impersonation_headers(headers: &mut reqwest::header::HeaderMap) {
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("claude-cli/2.1.75"),
        );
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(b"x-app") {
            headers.insert(name, reqwest::header::HeaderValue::from_static("cli"));
        }
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        if let Ok(name) =
            reqwest::header::HeaderName::from_bytes(b"anthropic-dangerous-direct-browser-access")
        {
            headers.insert(name, reqwest::header::HeaderValue::from_static("true"));
        }
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(b"anthropic-beta") {
            headers.insert(
                name,
                reqwest::header::HeaderValue::from_static(
                    "claude-code-20250219,oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14,interleaved-thinking-2025-05-14"
                ),
            );
        }
    }
}

impl Default for ClientRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_equality() {
        let k1 = ClientCacheKey::new("openai", Some("sk-123"), None);
        let k2 = ClientCacheKey::new("openai", Some("sk-123"), None);
        assert_eq!(k1, k2);
    }

    #[test]
    fn test_get_or_create_caches() {
        let mut registry = ClientRegistry::new();
        let c1 = registry.get_or_create("openai", Some("sk-123"), None);
        let c2 = registry.get_or_create("openai", Some("sk-123"), None);
        assert!(Arc::ptr_eq(&c1, &c2));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_different_keys_separate_clients() {
        let mut registry = ClientRegistry::new();
        let c1 = registry.get_or_create("openai", Some("sk-111"), None);
        let c2 = registry.get_or_create("openai", Some("sk-222"), None);
        assert!(!Arc::ptr_eq(&c1, &c2));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn test_clear() {
        let mut registry = ClientRegistry::new();
        registry.get_or_create("openai", Some("sk-123"), None);
        assert!(!registry.is_empty());
        registry.clear();
        assert!(registry.is_empty());
    }

    fn extract_auth_header(api_key: Option<&str>, provider: &str) -> Option<String> {
        let mut headers = reqwest::header::HeaderMap::new();
        let is_anthropic = provider.eq_ignore_ascii_case("anthropic");
        let is_oauth_token =
            api_key.is_some_and(|k| k.to_ascii_lowercase().starts_with("sk-ant-oat"));
        ClientRegistry::insert_auth_header(&mut headers, api_key, is_anthropic, is_oauth_token);
        headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned())
            .or_else(|| {
                headers
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_owned())
            })
    }

    #[test]
    fn test_no_auth_header_when_key_is_none() {
        assert!(extract_auth_header(None, "agent-infer").is_none());
    }

    #[test]
    fn test_no_auth_header_when_key_is_empty() {
        assert!(extract_auth_header(Some(""), "agent-infer").is_none());
        assert!(extract_auth_header(Some("   "), "agent-infer").is_none());
    }

    #[test]
    fn test_bearer_auth_for_non_empty_key() {
        assert_eq!(
            extract_auth_header(Some("sk-123"), "openai").as_deref(),
            Some("Bearer sk-123")
        );
    }
}
