//! Core execution: retry/orchestration loop and LLMCore definition.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;
use tracing::{info, warn};

use super::api_format::ApiFormat;
use super::client_registry::ClientRegistry;
use super::error_classify::AttemptDecision;
use super::errors::{ConduitError, ErrorKind};
use super::message_norm::normalize_messages_for_api;
use super::provider_registry::ProviderRegistry;
use super::provider_runtime::ProviderRuntime;
use super::response_parser::{SSE_STREAM_ERROR_PREFIX, TransportResponse};
use crate::clients::parsing::TransportKind;

/// Async callback that attempts to refresh an OAuth token for the given provider.
///
/// Returns `Some(new_api_key)` on success, `None` if refresh is not applicable or fails.
pub type OAuthTokenRefresher = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// How API keys are configured.
#[derive(Debug, Clone)]
pub enum ApiKeyConfig {
    None,
    Single(String),
    PerProvider(HashMap<String, String>),
}

/// How API bases are configured.
#[derive(Debug, Clone)]
pub enum ApiBaseConfig {
    None,
    Single(String),
    PerProvider(HashMap<String, String>),
}

/// Shared LLM execution utilities (provider resolution, retries, client cache).
pub struct LLMCore {
    provider: String,
    model: String,
    fallback_models: Vec<String>,
    max_retries: u32,
    api_key: ApiKeyConfig,
    api_base: ApiBaseConfig,
    client_registry: ClientRegistry,
    api_format: ApiFormat,
    verbose: u32,
    provider_registry: ProviderRegistry,
    #[allow(clippy::type_complexity)]
    error_classifier: Option<Box<dyn Fn(&ConduitError) -> Option<ErrorKind> + Send + Sync>>,
    /// Optional callback to refresh an OAuth token when a 401 is received.
    oauth_refresher: Option<OAuthTokenRefresher>,
    /// Single-flight guard: prevents concurrent 401s from triggering multiple refreshes.
    oauth_refresh_guard: Arc<TokioMutex<()>>,
}

fn split_model_id<'a>(
    model: &'a str,
    message: &'static str,
) -> Result<(&'a str, &'a str), ConduitError> {
    let (provider, model_id) = model
        .split_once(':')
        .ok_or_else(|| ConduitError::new(ErrorKind::InvalidInput, message))?;
    if provider.is_empty() || model_id.is_empty() {
        return Err(ConduitError::new(ErrorKind::InvalidInput, message));
    }
    Ok((provider, model_id))
}

struct ModelCandidate {
    provider_name: String,
    model_id: String,
    client: Arc<Client>,
    api_key: Option<String>,
    api_base: Option<String>,
}

impl ModelCandidate {
    fn runtime<'a>(
        &'a self,
        api_format: ApiFormat,
        registry: &'a ProviderRegistry,
    ) -> ProviderRuntime<'a> {
        ProviderRuntime::new(
            &self.provider_name,
            &self.model_id,
            self.api_key.as_deref(),
            self.api_base.as_deref(),
            api_format,
        )
        .with_registry(registry)
    }
}

struct PreparedAttempt {
    transport: TransportKind,
    url: String,
    body: Value,
    body_forced_stream: bool,
}

impl LLMCore {
    /// Create a new `LLMCore` instance.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: String,
        model: String,
        fallback_models: Vec<String>,
        max_retries: u32,
        api_key: ApiKeyConfig,
        api_base: ApiBaseConfig,
        api_format: impl Into<ApiFormat>,
        verbose: u32,
    ) -> Self {
        Self {
            provider,
            model,
            fallback_models,
            max_retries,
            api_key,
            api_base,
            client_registry: ClientRegistry::new(),
            api_format: api_format.into(),
            verbose,
            provider_registry: ProviderRegistry::new(),
            error_classifier: None,
            oauth_refresher: None,
            oauth_refresh_guard: Arc::new(TokioMutex::new(())),
        }
    }

    /// Replace the provider registry.
    pub fn set_provider_registry(&mut self, registry: ProviderRegistry) {
        self.provider_registry = registry;
    }

    /// Access the provider registry.
    pub fn provider_registry(&self) -> &ProviderRegistry {
        &self.provider_registry
    }

    pub fn with_error_classifier(
        mut self,
        classifier: impl Fn(&ConduitError) -> Option<ErrorKind> + Send + Sync + 'static,
    ) -> Self {
        self.error_classifier = Some(Box::new(classifier));
        self
    }

    /// Set an OAuth token refresher callback.
    ///
    /// When a 401 Unauthorized is received and this refresher is set, it will be
    /// called to attempt a token refresh. A single-flight guard prevents concurrent
    /// 401s from triggering multiple refresh attempts.
    pub fn with_oauth_refresher(mut self, refresher: OAuthTokenRefresher) -> Self {
        self.oauth_refresher = Some(refresher);
        self
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn fallback_models(&self) -> &[String] {
        &self.fallback_models
    }

    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    pub fn api_key_config(&self) -> &ApiKeyConfig {
        &self.api_key
    }

    pub fn api_base_config(&self) -> &ApiBaseConfig {
        &self.api_base
    }

    pub fn api_format(&self) -> ApiFormat {
        self.api_format
    }

    pub fn verbose(&self) -> u32 {
        self.verbose
    }

    /// `1 + max_retries`, minimum 1.
    pub fn max_attempts(&self) -> u32 {
        1u32.max(1 + self.max_retries)
    }

    fn retry_attempts(&self) -> std::ops::Range<u32> {
        0..self.max_attempts()
    }

    /// Hard cap on the exponential backoff ceiling, in seconds.
    const BACKOFF_CAP_SECS: u64 = 8;

    /// Exponential backoff ceiling for retry `attempt` (0-indexed): 0s, 1s, 2s,
    /// 4s, then capped at [`BACKOFF_CAP_SECS`]. Overflow-safe (no large shifts).
    fn backoff_ceiling_secs(attempt: u32) -> u64 {
        match attempt {
            0 => 0,
            // 1 << 3 == 8 already reaches the cap, so anything ≥4 saturates.
            n if n >= 4 => Self::BACKOFF_CAP_SECS,
            n => 1u64 << (n - 1),
        }
    }

    /// Full-jitter backoff (AWS "Exponential Backoff And Jitter"): a `jitter`
    /// fraction in `[0, 1]` of the exponential ceiling. Pure and deterministic
    /// for testing. Concurrent retriers (e.g. many sessions hitting one 429)
    /// sample independent fractions and so spread out instead of retrying in
    /// lockstep, which avoids self-inflicted thundering-herd retry storms.
    fn backoff_duration(attempt: u32, jitter: f64) -> Duration {
        let ceiling = Self::backoff_ceiling_secs(attempt);
        if ceiling == 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(ceiling as f64 * jitter.clamp(0.0, 1.0))
    }

    /// Sample a full-jitter backoff duration using the thread RNG (live path).
    fn sample_backoff(attempt: u32) -> Duration {
        use rand::Rng;
        Self::backoff_duration(attempt, rand::thread_rng().gen_range(0.0..1.0))
    }

    /// Sleep for a full-jitter exponential backoff before retry `attempt`.
    async fn backoff_delay(attempt: u32) {
        if attempt == 0 {
            return;
        }
        let delay = Self::sample_backoff(attempt);
        warn!(
            attempt,
            delay_ms = delay.as_millis() as u64,
            "retrying after backoff (full jitter)"
        );
        tokio::time::sleep(delay).await;
    }

    /// Delegate to the custom error classifier, if set.
    pub(crate) fn custom_classify(&self, error: &ConduitError) -> Option<ErrorKind> {
        self.error_classifier.as_ref().and_then(|clf| clf(error))
    }

    pub fn resolve_model_provider(
        model: &str,
        provider: Option<&str>,
    ) -> Result<(String, String), ConduitError> {
        if let Some(p) = provider {
            if model.contains(':') {
                return Err(ConduitError::new(
                    ErrorKind::InvalidInput,
                    "When provider is specified, model must not include a provider prefix.",
                ));
            }
            return Ok((p.to_owned(), model.to_owned()));
        }
        let (prov, mdl) = split_model_id(model, "Model must be in 'provider:model' format.")?;
        Ok((prov.to_owned(), mdl.to_owned()))
    }

    /// Resolve a fallback model string into `(provider, model)`.
    pub fn resolve_fallback(&self, model: &str) -> Result<(String, String), ConduitError> {
        if model.contains(':') {
            let (prov, mdl) =
                split_model_id(model, "Fallback models must be in 'provider:model' format.")?;
            return Ok((prov.to_owned(), mdl.to_owned()));
        }
        if !self.provider.is_empty() {
            return Ok((self.provider.clone(), model.to_owned()));
        }
        Err(ConduitError::new(
            ErrorKind::InvalidInput,
            "Fallback models must include provider or LLM must be initialized with a provider.",
        ))
    }

    /// Build the ordered list of (provider, model) candidates to try.
    pub fn model_candidates(
        &self,
        override_model: Option<&str>,
        override_provider: Option<&str>,
    ) -> Result<Vec<(String, String)>, ConduitError> {
        if let Some(om) = override_model {
            let (p, m) = Self::resolve_model_provider(om, override_provider)?;
            return Ok(vec![(p, m)]);
        }
        let mut candidates = vec![(self.provider.clone(), self.model.clone())];
        for fallback in &self.fallback_models {
            candidates.push(self.resolve_fallback(fallback)?);
        }
        Ok(candidates)
    }

    /// Resolve the API key for a given provider.
    pub fn resolve_api_key(&self, provider: &str) -> Option<String> {
        match &self.api_key {
            ApiKeyConfig::None => None,
            ApiKeyConfig::Single(key) => Some(key.clone()),
            ApiKeyConfig::PerProvider(map) => map.get(provider).cloned(),
        }
    }

    /// Resolve the API base URL for a given provider.
    pub fn resolve_api_base(&self, provider: &str) -> Option<String> {
        match &self.api_base {
            ApiBaseConfig::None => None,
            ApiBaseConfig::Single(base) => Some(base.clone()),
            ApiBaseConfig::PerProvider(map) => map.get(provider).cloned(),
        }
    }

    /// Read the response payload, handling forced-stream collection.
    async fn read_response_payload(
        resp: reqwest::Response,
        body_forced_stream: bool,
        transport: TransportKind,
        provider_name: &str,
        model_id: &str,
    ) -> Result<Value, ConduitError> {
        if body_forced_stream {
            Self::collect_sse_response(resp, transport).await
        } else {
            resp.json().await.map_err(|e| {
                ConduitError::new(
                    ErrorKind::Provider,
                    format!("{provider_name}:{model_id}: failed to parse response: {e}"),
                )
            })
        }
    }

    /// Classify a reqwest transport error into an `ErrorKind`.
    fn classify_reqwest_error(e: &reqwest::Error) -> ErrorKind {
        if e.is_timeout() {
            return ErrorKind::Temporary;
        }
        if e.is_connect() {
            return ErrorKind::Provider;
        }
        ErrorKind::Unknown
    }

    /// Build the request body for a given transport kind.
    fn build_body_for_transport(
        transport: TransportKind,
        request: &super::request_builder::TransportCallRequest,
        provider_name: &str,
    ) -> Result<serde_json::Value, ConduitError> {
        match transport {
            TransportKind::Responses => Self::build_responses_body(request),
            TransportKind::Messages => Self::build_messages_body(request),
            TransportKind::Completion => Self::build_completion_body(request, provider_name),
        }
    }

    /// Send an HTTP request, returning the response or classifying the error.
    async fn send_http_request(
        client: &Client,
        url: &str,
        body: &Value,
        provider_name: &str,
        model_id: &str,
    ) -> Result<reqwest::Response, ConduitError> {
        let resp = client.post(url).json(body).send().await.map_err(|e| {
            ConduitError::new(
                Self::classify_reqwest_error(&e),
                format!("{provider_name}:{model_id}: {e}"),
            )
        })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            let kind = Self::classify_http_status(status.as_u16()).unwrap_or(ErrorKind::Provider);
            return Err(ConduitError::new(
                kind,
                format!("{provider_name}:{model_id}: HTTP {status} - {error_body}"),
            ));
        }
        Ok(resp)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_transport_request(
        client: &Arc<Client>,
        provider_name: &str,
        model_id: &str,
        resolved_api_base: &str,
        normalized_messages: Vec<Value>,
        tools_payload: &Option<Vec<Value>>,
        max_tokens: Option<u32>,
        stream: bool,
        reasoning_effort: &Option<Value>,
        kwargs: &serde_json::Map<String, Value>,
        session_id: Option<&str>,
        runtime: &ProviderRuntime<'_>,
    ) -> super::request_builder::TransportCallRequest {
        super::request_builder::TransportCallRequest {
            client: Arc::clone(client),
            provider_name: provider_name.to_owned(),
            model_id: model_id.to_owned(),
            api_base: Some(resolved_api_base.to_owned()),
            messages_payload: normalized_messages,
            tools_payload: tools_payload.clone(),
            max_tokens,
            stream,
            reasoning_effort: reasoning_effort.clone(),
            kwargs: kwargs.clone(),
            is_anthropic_oauth: runtime.is_anthropic_oauth(),
            session_id: session_id.map(|s| s.to_owned()),
        }
    }

    /// Handle a failed attempt and decide whether to retry or move on.
    /// Returns `Ok(true)` for retry, `Ok(false)` for break.
    fn handle_send_error(
        &self,
        error: ConduitError,
        provider_name: &str,
        model_id: &str,
        attempt: u32,
        last_error: &mut Option<ConduitError>,
    ) -> bool {
        let outcome = self.handle_attempt_error(error, provider_name, model_id, attempt);
        *last_error = Some(outcome.error);
        outcome.decision == AttemptDecision::RetrySameModel
    }

    /// Check whether an error is a 401 Unauthorized that could be resolved by
    /// refreshing an OAuth token. Returns `true` if the error message contains
    /// "HTTP 401".
    fn is_http_401(error: &ConduitError) -> bool {
        error.kind == ErrorKind::Config && error.message.contains("HTTP 401")
    }

    /// Attempt to refresh an OAuth token for the given provider.
    ///
    /// Uses a single-flight guard so concurrent 401s don't trigger multiple
    /// refreshes. On success, updates the API key config and evicts the cached
    /// client so the next attempt uses the fresh token.
    ///
    /// Returns `true` if the token was refreshed and the caller should retry.
    async fn try_oauth_refresh(&mut self, provider_name: &str) -> bool {
        let Some(refresher) = &self.oauth_refresher else {
            return false;
        };

        // Single-flight: acquire the guard. If another task is already refreshing,
        // we wait for it, then check if the key changed (it did if the other task
        // succeeded).
        let _guard = self.oauth_refresh_guard.lock().await;

        let refresher = Arc::clone(refresher);
        match refresher(provider_name) {
            Some(new_key) => {
                info!(
                    provider = %provider_name,
                    "OAuth token refreshed successfully, evicting cached client"
                );

                // Evict old client so the next build_candidate picks up the new key.
                let old_api_key = self.resolve_api_key(provider_name);
                let api_base = self.resolve_api_base(provider_name);
                self.client_registry.remove(
                    provider_name,
                    old_api_key.as_deref(),
                    api_base.as_deref(),
                );

                // Update the API key config.
                match &mut self.api_key {
                    ApiKeyConfig::Single(key) => {
                        *key = new_key;
                    }
                    ApiKeyConfig::PerProvider(map) => {
                        map.insert(provider_name.to_owned(), new_key);
                    }
                    ApiKeyConfig::None => {
                        self.api_key = ApiKeyConfig::Single(new_key);
                    }
                }

                true
            }
            None => {
                warn!(
                    provider = %provider_name,
                    "OAuth token refresh returned None, not retrying"
                );
                false
            }
        }
    }

    /// Get or create an HTTP client for the given provider.
    pub fn get_client(&mut self, provider: &str) -> Arc<Client> {
        let api_key = self.resolve_api_key(provider);
        let api_base = self.resolve_api_base(provider);
        self.client_registry
            .get_or_create(provider, api_key.as_deref(), api_base.as_deref())
    }

    fn build_candidate(&mut self, provider_name: &str, model_id: &str) -> ModelCandidate {
        let api_key = self.resolve_api_key(provider_name);
        let api_base = self.resolve_api_base(provider_name);
        let client = self.client_registry.get_or_create(
            provider_name,
            api_key.as_deref(),
            api_base.as_deref(),
        );
        ModelCandidate {
            provider_name: provider_name.to_owned(),
            model_id: model_id.to_owned(),
            client,
            api_key,
            api_base,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_attempt(
        &self,
        candidate: &ModelCandidate,
        messages_payload: &[Value],
        tools_payload: &Option<Vec<Value>>,
        max_tokens: Option<u32>,
        stream: bool,
        reasoning_effort: &Option<Value>,
        kwargs: &serde_json::Map<String, Value>,
        session_id: Option<&str>,
    ) -> Result<PreparedAttempt, ConduitError> {
        let runtime = candidate.runtime(self.api_format, &self.provider_registry);
        let transport = runtime.selected_transport(tools_payload.as_deref(), false, None)?;
        let resolved_api_base = runtime.resolved_api_base();
        let request = Self::build_transport_request(
            &candidate.client,
            &candidate.provider_name,
            &candidate.model_id,
            &resolved_api_base,
            normalize_messages_for_api(messages_payload.to_vec(), transport),
            tools_payload,
            max_tokens,
            stream,
            reasoning_effort,
            kwargs,
            session_id,
            &runtime,
        );
        let url = Self::build_request_url(&resolved_api_base, transport);
        let body = Self::build_body_for_transport(transport, &request, &candidate.provider_name)?;
        let body_forced_stream = !stream
            && body
                .get("stream")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        Ok(PreparedAttempt {
            transport,
            url,
            body,
            body_forced_stream,
        })
    }

    fn log_request(candidate: &ModelCandidate, prepared: &PreparedAttempt) {
        info!(
            target: "eli_trace",
            provider = %candidate.provider_name,
            model = %candidate.model_id,
            transport = ?prepared.transport,
            stream = prepared.body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
            request_body = %prepared.body,
            "llm.request"
        );
    }

    fn log_response(candidate: &ModelCandidate, transport: TransportKind, payload: &Value) {
        info!(
            target: "eli_trace",
            provider = %candidate.provider_name,
            model = %candidate.model_id,
            transport = ?transport,
            response_payload = %payload,
            "llm.response"
        );
    }

    /// Execute a synchronous (non-streaming) chat call with retry logic.
    ///
    /// Iterates over model candidates, retrying on transient errors.
    /// The `on_response` callback receives `(TransportResponse, provider, model, attempt)`
    /// and should return `Ok(T)` on success or `Err(None)` to signal a retry.
    ///
    /// When the primary model overflows, the fallback receives the **full**
    /// unmodified context.  If it also overflows, the error propagates and the
    /// higher-level handoff mechanism (via `maybe_auto_handoff_on_error`) places
    /// a tape anchor so the next turn starts fresh.  We never truncate here
    /// because the tape is append-only and information must never be silently
    /// dropped from the payload.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_chat<T, F>(
        &mut self,
        messages_payload: Vec<Value>,
        tools_payload: Option<Vec<Value>>,
        model: Option<&str>,
        provider: Option<&str>,
        max_tokens: Option<u32>,
        stream: bool,
        reasoning_effort: Option<Value>,
        kwargs: serde_json::Map<String, Value>,
        session_id: Option<&str>,
        on_response: F,
    ) -> Result<T, ConduitError>
    where
        F: Fn(TransportResponse, &str, &str, u32) -> Result<T, Option<ConduitError>>,
    {
        let candidates = self.model_candidates(model, provider)?;
        let mut last_error: Option<ConduitError> = None;
        let mut oauth_refreshed = false;

        for (provider_name, model_id) in &candidates {
            for attempt in self.retry_attempts() {
                Self::backoff_delay(attempt).await;

                // Build the candidate inside the retry loop so that after a client eviction
                // (e.g. stale SSE connection pool) the next attempt gets a fresh Arc<Client>
                // from the registry rather than re-using the old one still held by a prior
                // ModelCandidate.
                let candidate = self.build_candidate(provider_name, model_id);

                let prepared = match self.prepare_attempt(
                    &candidate,
                    &messages_payload,
                    &tools_payload,
                    max_tokens,
                    stream,
                    &reasoning_effort,
                    &kwargs,
                    session_id,
                ) {
                    Ok(prepared) => prepared,
                    Err(e) => {
                        last_error = Some(e);
                        break;
                    }
                };
                Self::log_request(&candidate, &prepared);

                let resp = match Self::send_http_request(
                    &candidate.client,
                    &prepared.url,
                    &prepared.body,
                    &candidate.provider_name,
                    &candidate.model_id,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        // On 401 with an OAuth refresher, try to refresh once.
                        if !oauth_refreshed
                            && Self::is_http_401(&e)
                            && self.try_oauth_refresh(&candidate.provider_name).await
                        {
                            oauth_refreshed = true;
                            warn!(
                                provider = %candidate.provider_name,
                                model = %candidate.model_id,
                                "retrying after OAuth token refresh"
                            );
                            continue;
                        }
                        if self.handle_send_error(
                            e,
                            &candidate.provider_name,
                            &candidate.model_id,
                            attempt,
                            &mut last_error,
                        ) {
                            continue;
                        }
                        break;
                    }
                };

                let payload: Value = match Self::read_response_payload(
                    resp,
                    prepared.body_forced_stream,
                    prepared.transport,
                    &candidate.provider_name,
                    &candidate.model_id,
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        // SSE stream body errors are caused by stale pooled connections or
                        // server-side stream termination (e.g. backend time limit).
                        // Evict the cached client so the next retry gets a fresh connection pool.
                        if e.message.contains(SSE_STREAM_ERROR_PREFIX) {
                            let next = attempt + 1;
                            let max = self.max_attempts();
                            if next < max {
                                warn!(
                                    provider = %candidate.provider_name,
                                    model = %candidate.model_id,
                                    attempt = next,
                                    max_attempts = max,
                                    error = %e.message,
                                    "SSE stream error — evicting client and retrying"
                                );
                            }
                            self.client_registry.remove(
                                &candidate.provider_name,
                                candidate.api_key.as_deref(),
                                candidate.api_base.as_deref(),
                            );
                        }
                        if self.handle_send_error(
                            e,
                            &candidate.provider_name,
                            &candidate.model_id,
                            attempt,
                            &mut last_error,
                        ) {
                            continue;
                        }
                        break;
                    }
                };

                Self::log_response(&candidate, prepared.transport, &payload);

                match on_response(
                    TransportResponse {
                        transport: prepared.transport,
                        payload,
                    },
                    &candidate.provider_name,
                    &candidate.model_id,
                    attempt,
                ) {
                    Ok(result) => return Ok(result),
                    Err(Some(e)) => {
                        last_error = Some(e);
                        break;
                    }
                    Err(None) => continue,
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            ConduitError::new(ErrorKind::Temporary, "LLM call failed after retries")
        }))
    }

    /// Execute a streaming chat call.
    ///
    /// Returns the raw `reqwest::Response` for the caller to consume as an SSE stream.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_chat_stream(
        &mut self,
        messages_payload: Vec<Value>,
        tools_payload: Option<Vec<Value>>,
        model: Option<&str>,
        provider: Option<&str>,
        max_tokens: Option<u32>,
        reasoning_effort: Option<Value>,
        kwargs: serde_json::Map<String, Value>,
        session_id: Option<&str>,
    ) -> Result<(reqwest::Response, TransportKind, String, String), ConduitError> {
        let candidates = self.model_candidates(model, provider)?;
        let mut last_error: Option<ConduitError> = None;
        let mut oauth_refreshed = false;

        for (provider_name, model_id) in &candidates {
            for attempt in self.retry_attempts() {
                Self::backoff_delay(attempt).await;

                // Build candidate inside the retry loop (same as run_chat) so that
                // after an OAuth refresh + client eviction the next attempt uses
                // the fresh token.
                let candidate = self.build_candidate(provider_name, model_id);

                let prepared = match self.prepare_attempt(
                    &candidate,
                    &messages_payload,
                    &tools_payload,
                    max_tokens,
                    true,
                    &reasoning_effort,
                    &kwargs,
                    session_id,
                ) {
                    Ok(prepared) => prepared,
                    Err(e) => {
                        last_error = Some(e);
                        break;
                    }
                };
                Self::log_request(&candidate, &prepared);

                let resp = match Self::send_http_request(
                    &candidate.client,
                    &prepared.url,
                    &prepared.body,
                    &candidate.provider_name,
                    &candidate.model_id,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        // On 401 with an OAuth refresher, try to refresh once.
                        if !oauth_refreshed
                            && Self::is_http_401(&e)
                            && self.try_oauth_refresh(&candidate.provider_name).await
                        {
                            oauth_refreshed = true;
                            warn!(
                                provider = %candidate.provider_name,
                                model = %candidate.model_id,
                                "retrying stream after OAuth token refresh"
                            );
                            continue;
                        }
                        if self.handle_send_error(
                            e,
                            &candidate.provider_name,
                            &candidate.model_id,
                            attempt,
                            &mut last_error,
                        ) {
                            continue;
                        }
                        break;
                    }
                };

                return Ok((
                    resp,
                    prepared.transport,
                    candidate.provider_name.clone(),
                    candidate.model_id.clone(),
                ));
            }
        }

        Err(last_error.unwrap_or_else(|| {
            ConduitError::new(
                ErrorKind::Temporary,
                "LLM streaming call failed after retries",
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::anthropic_messages;
    use crate::core::error_classify::classify_by_text_signature;
    use crate::core::message_norm::{enforce_anthropic_message_rules, prune_orphan_tool_messages};
    use serde_json::json;

    // Re-import TransportCallRequest for test use
    use super::super::request_builder::TransportCallRequest;

    #[test]
    fn test_resolve_model_provider() {
        let (p, m) = LLMCore::resolve_model_provider("openai:gpt-4", None).unwrap();
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4");
    }

    #[test]
    fn test_resolve_model_provider_with_override() {
        let (p, m) = LLMCore::resolve_model_provider("gpt-4", Some("openai")).unwrap();
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4");
    }

    #[test]
    fn test_resolve_model_provider_error() {
        let result = LLMCore::resolve_model_provider("gpt-4", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_classify_http_status() {
        assert_eq!(LLMCore::classify_http_status(401), Some(ErrorKind::Config));
        assert_eq!(
            LLMCore::classify_http_status(429),
            Some(ErrorKind::Temporary)
        );
        assert_eq!(
            LLMCore::classify_http_status(500),
            Some(ErrorKind::Provider)
        );
        assert_eq!(LLMCore::classify_http_status(200), None);
    }

    // -- backoff (full jitter) ------------------------------------------------

    #[test]
    fn backoff_ceiling_is_capped_exponential() {
        assert_eq!(LLMCore::backoff_ceiling_secs(0), 0);
        assert_eq!(LLMCore::backoff_ceiling_secs(1), 1);
        assert_eq!(LLMCore::backoff_ceiling_secs(2), 2);
        assert_eq!(LLMCore::backoff_ceiling_secs(3), 4);
        assert_eq!(LLMCore::backoff_ceiling_secs(4), 8);
        // Large attempts saturate at the cap without shift overflow.
        assert_eq!(LLMCore::backoff_ceiling_secs(40), 8);
    }

    #[test]
    fn backoff_duration_scales_with_jitter_fraction() {
        // attempt 3 → 4s ceiling. Full jitter scales linearly in [0, ceiling],
        // replacing the old fixed lockstep 4s that made concurrent retriers
        // collide.
        let ceiling = Duration::from_secs(4);
        assert_eq!(LLMCore::backoff_duration(3, 0.0), Duration::ZERO);
        assert_eq!(LLMCore::backoff_duration(3, 0.5), Duration::from_secs(2));
        assert_eq!(LLMCore::backoff_duration(3, 1.0), ceiling);
        // Out-of-range fractions are clamped; never exceeds the ceiling.
        assert_eq!(LLMCore::backoff_duration(3, 2.0), ceiling);
        assert_eq!(LLMCore::backoff_duration(3, -1.0), Duration::ZERO);
        assert!(LLMCore::backoff_duration(3, 0.99) < ceiling);
    }

    #[test]
    fn backoff_attempt_zero_has_no_delay() {
        assert_eq!(LLMCore::backoff_duration(0, 1.0), Duration::ZERO);
    }

    #[test]
    fn sampled_backoff_decorrelates_concurrent_retriers() {
        // The point of jitter: the live RNG path yields a spread within the
        // ceiling, not a single lockstep value (the pre-jitter behavior).
        let ceiling = Duration::from_secs(4);
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..256 {
            let d = LLMCore::sample_backoff(3);
            assert!(d <= ceiling, "sampled delay {d:?} exceeded ceiling");
            distinct.insert(d.as_millis());
        }
        assert!(
            distinct.len() > 16,
            "full jitter should spread delays; got {} distinct values",
            distinct.len()
        );
    }

    #[test]
    fn test_split_messages_for_responses() {
        let messages = vec![
            json!({"role": "system", "content": "You are helpful."}),
            json!({"role": "user", "content": "Hello"}),
        ];
        let (instructions, items) = LLMCore::split_messages_for_responses(&messages);
        assert_eq!(instructions.unwrap(), "You are helpful.");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"], "user");
    }

    #[test]
    fn test_split_messages_for_responses_keeps_multimodal_parts() {
        let messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "compare"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,A"}},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,B"}}
            ]
        })];
        let (_, items) = LLMCore::split_messages_for_responses(&messages);
        assert_eq!(items.len(), 1);
        let content = items[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[2]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,A");
    }

    #[test]
    fn test_split_messages_for_responses_keeps_image_only_message() {
        let messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,ONLY"}}
            ]
        })];
        let (_, items) = LLMCore::split_messages_for_responses(&messages);
        assert_eq!(items.len(), 1);
        let content = items[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "input_image");
        assert_eq!(content[0]["image_url"], "data:image/png;base64,ONLY");
    }

    #[test]
    fn test_convert_tools_for_responses() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "greet",
                "description": "Say hello",
                "parameters": {"type": "object"}
            }
        })];
        let converted = LLMCore::convert_tools_for_responses(Some(&tools)).unwrap();
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["name"], "greet");
        assert!(converted[0].get("function").is_none());
    }

    #[test]
    fn test_normalize_anthropic_messages_keeps_multiple_tool_results() {
        let messages = vec![
            json!({
                "role": "user",
                "content": "find latest papers"
            }),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"cmd": "pwd"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "skill", "input": {"name": "active-research"}}
                ]
            }),
            json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok-1"}
                ]
            }),
            json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": "ok-2"}
                ]
            }),
        ];

        let normalized = anthropic_messages::normalize_messages(messages);
        assert_eq!(normalized.len(), 3);
        assert_eq!(normalized[0]["role"], "user");
        assert_eq!(normalized[1]["role"], "assistant");
        assert_eq!(normalized[2]["role"], "user");
        let content = normalized[2]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["tool_use_id"], "toolu_1");
        assert_eq!(content[1]["tool_use_id"], "toolu_2");
    }

    #[test]
    fn test_normalize_anthropic_messages_merges_text_with_tool_results() {
        let messages = vec![
            json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
                ]
            }),
            json!({
                "role": "user",
                "content": "continue with a summary"
            }),
        ];

        let normalized = anthropic_messages::normalize_messages(messages);
        assert_eq!(normalized.len(), 1);
        let content = normalized[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "continue with a summary");
    }

    #[test]
    fn test_build_messages_body_keeps_tool_results_immediately_after_tool_use() {
        let request = TransportCallRequest {
            client: Arc::new(reqwest::Client::new()),
            provider_name: "anthropic".to_owned(),
            model_id: "claude-test".to_owned(),
            api_base: None,
            messages_payload: vec![
                json!({"role": "system", "content": "system rules"}),
                json!({"role": "user", "content": "find latest papers"}),
                json!({
                    "role": "assistant",
                    "tool_calls": [
                        {"id": "toolu_1", "type": "function", "function": {"name": "bash", "arguments": "{\"cmd\":\"pwd\"}"}},
                        {"id": "toolu_2", "type": "function", "function": {"name": "skill", "arguments": "{\"name\":\"active-research\"}"}}
                    ]
                }),
                json!({"role": "tool", "tool_call_id": "toolu_1", "content": "ok-1"}),
                json!({"role": "tool", "tool_call_id": "toolu_2", "content": "ok-2"}),
                json!({"role": "user", "content": "\u{7ee7}\u{7eed}"}),
            ],
            tools_payload: None,
            max_tokens: Some(512),
            stream: false,
            reasoning_effort: None,
            kwargs: serde_json::Map::new(),
            is_anthropic_oauth: false,
            session_id: None,
        };

        let body = LLMCore::build_messages_body(&request).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");

        let user_blocks = messages[2]["content"].as_array().unwrap();
        assert_eq!(user_blocks[0]["type"], "tool_result");
        assert_eq!(user_blocks[0]["tool_use_id"], "toolu_1");
        assert_eq!(user_blocks[1]["type"], "tool_result");
        assert_eq!(user_blocks[1]["tool_use_id"], "toolu_2");
        assert_eq!(user_blocks[2]["type"], "text");
        assert_eq!(user_blocks[2]["text"], "\u{7ee7}\u{7eed}");
    }

    #[test]
    fn test_max_attempts() {
        let core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            2,
            ApiKeyConfig::None,
            ApiBaseConfig::None,
            TransportKind::Completion,
            0,
        );
        assert_eq!(core.max_attempts(), 3);
    }

    #[test]
    fn test_build_request_url() {
        assert_eq!(
            LLMCore::build_request_url("https://api.openai.com/v1", TransportKind::Completion),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            LLMCore::build_request_url("https://api.openai.com/v1", TransportKind::Responses),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            LLMCore::build_request_url("https://api.anthropic.com/v1", TransportKind::Messages),
            "https://api.anthropic.com/v1/messages"
        );
    }

    // ----- build_error_payload -----

    #[test]
    fn test_build_error_payload_basic() {
        let core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            2,
            ApiKeyConfig::None,
            ApiBaseConfig::None,
            TransportKind::Completion,
            0,
        );
        let error = ConduitError::new(ErrorKind::Provider, "server error");
        let payload = core.build_error_payload(&error, "openai", "gpt-4", 1, None);

        assert_eq!(payload.kind, ErrorKind::Provider);
        assert_eq!(payload.message, "server error");
        let details = payload.details.unwrap();
        assert_eq!(details["provider"], "openai");
        assert_eq!(details["model"], "gpt-4");
        assert_eq!(details["attempt"], 2); // attempt + 1
        assert_eq!(details["max_attempts"], 3); // 1 + max_retries(2)
        assert!(details.get("http_status").is_none());
    }

    #[test]
    fn test_build_error_payload_with_http_status() {
        let core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            3,
            ApiKeyConfig::None,
            ApiBaseConfig::None,
            TransportKind::Completion,
            0,
        );
        let error = ConduitError::new(ErrorKind::Temporary, "rate limited");
        let payload = core.build_error_payload(&error, "openai", "gpt-4", 0, Some(429));

        assert_eq!(payload.kind, ErrorKind::Temporary);
        let details = payload.details.unwrap();
        assert_eq!(details["http_status"], 429);
        assert_eq!(details["attempt"], 1);
        assert_eq!(details["max_attempts"], 4);
    }

    #[test]
    fn test_build_error_payload_different_provider() {
        let core = LLMCore::new(
            "anthropic".into(),
            "claude-3".into(),
            vec![],
            1,
            ApiKeyConfig::None,
            ApiBaseConfig::None,
            TransportKind::Completion,
            0,
        );
        let error = ConduitError::new(ErrorKind::Config, "auth failed");
        let payload = core.build_error_payload(&error, "anthropic", "claude-3", 0, Some(401));

        assert_eq!(payload.kind, ErrorKind::Config);
        let details = payload.details.unwrap();
        assert_eq!(details["provider"], "anthropic");
        assert_eq!(details["model"], "claude-3");
        assert_eq!(details["http_status"], 401);
    }

    // ----- accessor tests -----

    #[test]
    fn test_api_key_config_accessor() {
        let core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            3,
            ApiKeyConfig::Single("my-key".into()),
            ApiBaseConfig::None,
            TransportKind::Completion,
            0,
        );
        match core.api_key_config() {
            ApiKeyConfig::Single(key) => assert_eq!(key, "my-key"),
            _ => panic!("Expected Single key config"),
        }
    }

    #[test]
    fn test_api_base_config_accessor() {
        let core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            3,
            ApiKeyConfig::None,
            ApiBaseConfig::Single("https://custom.api.com".into()),
            TransportKind::Completion,
            0,
        );
        match core.api_base_config() {
            ApiBaseConfig::Single(base) => assert_eq!(base, "https://custom.api.com"),
            _ => panic!("Expected Single base config"),
        }
    }

    #[test]
    fn test_api_format_accessor() {
        let core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            3,
            ApiKeyConfig::None,
            ApiBaseConfig::None,
            TransportKind::Responses,
            0,
        );
        assert_eq!(core.api_format(), ApiFormat::Responses);
    }

    #[test]
    fn test_verbose_accessor() {
        let core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            3,
            ApiKeyConfig::None,
            ApiBaseConfig::None,
            TransportKind::Completion,
            2,
        );
        assert_eq!(core.verbose(), 2);
    }

    // ----- classify_by_text_signature -----

    #[test]
    fn test_classify_auth_errors() {
        assert_eq!(
            classify_by_text_signature("Unauthorized request"),
            Some(ErrorKind::Config)
        );
        assert_eq!(
            classify_by_text_signature("invalid api key provided"),
            Some(ErrorKind::Config)
        );
        assert_eq!(
            classify_by_text_signature("Invalid key format"),
            Some(ErrorKind::Config)
        );
    }

    #[test]
    fn test_classify_rate_limit_errors() {
        assert_eq!(
            classify_by_text_signature("Rate limit exceeded"),
            Some(ErrorKind::Temporary)
        );
        assert_eq!(
            classify_by_text_signature("HTTP 429 Too Many Requests"),
            Some(ErrorKind::Temporary)
        );
        assert_eq!(
            classify_by_text_signature("Quota exceeded for this month"),
            Some(ErrorKind::Temporary)
        );
    }

    #[test]
    fn test_classify_not_found_errors() {
        assert_eq!(
            classify_by_text_signature("Model not found"),
            Some(ErrorKind::NotFound)
        );
        assert_eq!(
            classify_by_text_signature("HTTP 404"),
            Some(ErrorKind::NotFound)
        );
    }

    #[test]
    fn test_classify_timeout_errors() {
        assert_eq!(
            classify_by_text_signature("Request timeout"),
            Some(ErrorKind::Temporary)
        );
        assert_eq!(
            classify_by_text_signature("Connection timed out"),
            Some(ErrorKind::Temporary)
        );
    }

    #[test]
    fn test_classify_server_errors() {
        assert_eq!(
            classify_by_text_signature("Internal server error"),
            Some(ErrorKind::Temporary)
        );
        assert_eq!(
            classify_by_text_signature("HTTP 502 Bad Gateway"),
            Some(ErrorKind::Temporary)
        );
        assert_eq!(
            classify_by_text_signature("HTTP 503 Service Unavailable"),
            Some(ErrorKind::Temporary)
        );
    }

    #[test]
    fn test_classify_unknown_message() {
        assert_eq!(classify_by_text_signature("Something went wrong"), None);
        assert_eq!(classify_by_text_signature(""), None);
    }

    // ----- normalize_messages_for_api -----

    #[test]
    fn test_prune_orphan_tool_result() {
        // tool_result without matching tool_call should be removed
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "tool", "tool_call_id": "call_orphan", "content": "result"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        let result = prune_orphan_tool_messages(messages);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[1]["role"], "assistant");
    }

    #[test]
    fn test_prune_orphan_tool_call() {
        // assistant with tool_calls but no matching tool_result should be removed
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "foo", "arguments": "{}"}}]}),
        ];
        let result = prune_orphan_tool_messages(messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");
    }

    #[test]
    fn test_keep_matched_tool_pair() {
        // matched tool_call + tool_result should both be kept
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "foo", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "result"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let result = prune_orphan_tool_messages(messages);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_anthropic_merge_consecutive_user() {
        let messages = vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "user", "content": "second"}),
            json!({"role": "assistant", "content": "reply"}),
        ];
        let result = enforce_anthropic_message_rules(messages);
        // merged_user + assistant + trailing "Continue." user
        assert_eq!(result.len(), 3);
        assert_eq!(result[0]["content"], "first\n\nsecond");
        assert_eq!(result[1]["role"], "assistant");
        assert_eq!(result[2]["role"], "user");
        assert_eq!(result[2]["content"], "Continue.");
    }

    #[test]
    fn test_anthropic_insert_user_at_start() {
        let messages = vec![json!({"role": "assistant", "content": "hi"})];
        let result = enforce_anthropic_message_rules(messages);
        assert_eq!(result.len(), 3); // synthetic user + assistant + trailing user
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["content"], "Continue.");
        assert_eq!(result[1]["role"], "assistant");
        assert_eq!(result[2]["role"], "user");
    }

    #[test]
    fn test_anthropic_append_user_at_end() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "reply"}),
        ];
        let result = enforce_anthropic_message_rules(messages);
        assert_eq!(result.len(), 3);
        assert_eq!(result[2]["role"], "user");
        assert_eq!(result[2]["content"], "Continue.");
    }

    #[test]
    fn test_anthropic_no_append_when_user_last() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "reply"}),
            json!({"role": "user", "content": "more"}),
        ];
        let result = enforce_anthropic_message_rules(messages);
        assert_eq!(result.len(), 3);
        assert_eq!(result[2]["content"], "more");
    }

    #[test]
    fn test_anthropic_system_preserved() {
        let messages = vec![
            json!({"role": "system", "content": "system prompt"}),
            json!({"role": "user", "content": "hello"}),
        ];
        let result = enforce_anthropic_message_rules(messages);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "system");
        assert_eq!(result[1]["role"], "user");
    }

    #[test]
    fn test_normalize_empty() {
        let result = normalize_messages_for_api(vec![], TransportKind::Messages);
        assert!(result.is_empty());
    }

    #[test]
    fn test_normalize_completion_skips_anthropic_rules() {
        // For completion transport, consecutive same-role should NOT be merged
        let messages = vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "user", "content": "second"}),
        ];
        let result = normalize_messages_for_api(messages, TransportKind::Completion);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_normalize_messages_keeps_multiple_tool_results_for_messages_transport() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                    {"id": "call_2", "type": "function", "function": {"name": "skill", "arguments": "{}"}}
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "result-1"}),
            json!({"role": "tool", "tool_call_id": "call_2", "content": "result-2"}),
        ];

        let result = normalize_messages_for_api(messages, TransportKind::Messages);
        assert_eq!(result.len(), 4);
        assert_eq!(result[2]["tool_call_id"], "call_1");
        assert_eq!(result[3]["tool_call_id"], "call_2");
    }

    #[test]
    fn test_normalize_messages_canonicalizes_responses_style_tool_calls() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "type": "function_call",
                    "call_id": "call_123",
                    "name": "tape_info",
                    "arguments": "{}"
                }]
            }),
            json!({"role": "tool", "tool_call_id": "call_123", "content": "ok"}),
        ];

        let result = normalize_messages_for_api(messages, TransportKind::Messages);

        assert_eq!(result.len(), 3);
        assert_eq!(result[1]["tool_calls"][0]["id"], "call_123");
        assert_eq!(result[1]["tool_calls"][0]["function"]["name"], "tape_info");
    }

    // -- OAuth token refresh tests -------------------------------------------

    #[test]
    fn test_is_http_401_detects_401() {
        let error = ConduitError::new(ErrorKind::Config, "openai:gpt-4: HTTP 401 Unauthorized - ");
        assert!(LLMCore::is_http_401(&error));
    }

    #[test]
    fn test_is_http_401_rejects_other_errors() {
        let error = ConduitError::new(ErrorKind::Config, "invalid api key");
        assert!(!LLMCore::is_http_401(&error));

        let error = ConduitError::new(ErrorKind::Temporary, "rate limit exceeded");
        assert!(!LLMCore::is_http_401(&error));

        let error = ConduitError::new(ErrorKind::Config, "HTTP 403 Forbidden");
        assert!(!LLMCore::is_http_401(&error));
    }

    #[tokio::test]
    async fn test_try_oauth_refresh_without_refresher_returns_false() {
        let mut core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            1,
            ApiKeyConfig::Single("old-key".into()),
            ApiBaseConfig::None,
            ApiFormat::default(),
            0,
        );
        assert!(!core.try_oauth_refresh("openai").await);
    }

    #[tokio::test]
    async fn test_try_oauth_refresh_with_successful_refresh() {
        let refresher: OAuthTokenRefresher =
            Arc::new(|_provider: &str| Some("new-oauth-token".to_string()));

        let mut core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            1,
            ApiKeyConfig::Single("expired-token".into()),
            ApiBaseConfig::None,
            ApiFormat::default(),
            0,
        )
        .with_oauth_refresher(refresher);

        // Pre-populate the client cache
        core.get_client("openai");
        assert!(!core.client_registry.is_empty());

        assert!(core.try_oauth_refresh("openai").await);

        // API key should be updated
        assert_eq!(
            core.resolve_api_key("openai"),
            Some("new-oauth-token".to_string())
        );

        // Cached client should be evicted (old key no longer matches)
        assert!(
            !core
                .client_registry
                .contains("openai", Some("expired-token"), None),
            "old client should be evicted after refresh"
        );
    }

    #[tokio::test]
    async fn test_try_oauth_refresh_with_failed_refresh() {
        let refresher: OAuthTokenRefresher = Arc::new(|_provider: &str| None);

        let mut core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            1,
            ApiKeyConfig::Single("old-key".into()),
            ApiBaseConfig::None,
            ApiFormat::default(),
            0,
        )
        .with_oauth_refresher(refresher);

        assert!(!core.try_oauth_refresh("openai").await);

        // API key should be unchanged
        assert_eq!(core.resolve_api_key("openai"), Some("old-key".to_string()));
    }

    #[tokio::test]
    async fn test_try_oauth_refresh_updates_per_provider_map() {
        let refresher: OAuthTokenRefresher =
            Arc::new(|_provider: &str| Some("refreshed-key".to_string()));

        let mut map = HashMap::new();
        map.insert("openai".to_string(), "stale-key".to_string());

        let mut core = LLMCore::new(
            "openai".into(),
            "gpt-4".into(),
            vec![],
            1,
            ApiKeyConfig::PerProvider(map),
            ApiBaseConfig::None,
            ApiFormat::default(),
            0,
        )
        .with_oauth_refresher(refresher);

        assert!(core.try_oauth_refresh("openai").await);

        assert_eq!(
            core.resolve_api_key("openai"),
            Some("refreshed-key".to_string())
        );
    }
}
