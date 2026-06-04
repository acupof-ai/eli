//! Conduit LLM facade.

mod builder;
mod decisions;
mod embedding;
mod helpers;
mod stream;
mod tool_loop;

pub use builder::LLMBuilder;
pub use decisions::{collect_active_decisions, inject_decisions_into_system_prompt};
pub use embedding::EmbedInput;

// Re-export helpers used by submodules (pub(crate) so they stay internal).
use helpers::{
    build_assistant_tool_call_message, build_full_context_from_entries, build_messages,
    extract_content, extract_tool_calls, prepend_tape_history, restore_last_user_content,
    slice_entries_by_anchor, strip_image_blocks_for_persistence,
};

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

pub use crate::core::api_format::ApiFormat;
use crate::core::errors::{ConduitError, ErrorKind};
use crate::core::execution::{ApiBaseConfig, ApiKeyConfig, LLMCore};
use crate::core::provider_policies;
use crate::core::response_parser::TransportResponse;
use crate::tape::{AsyncTapeManager, AsyncTapeStoreAdapter, InMemoryTapeStore, TapeContext};
use crate::tools::context::ToolContext;
use crate::tools::executor::ToolExecutor;
use crate::tools::schema::ToolSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Public constants
// ---------------------------------------------------------------------------

/// Default model when none is specified.
pub const DEFAULT_MODEL: &str = "openai:gpt-4o-mini";

/// Hook to process stream events before they are emitted to the caller.
/// Return `Some(event)` to forward (possibly transformed), or `None` to drop.
pub type StreamEventFilter = Arc<
    dyn Fn(crate::core::results::StreamEvent) -> Option<crate::core::results::StreamEvent>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// ChatRequest
// ---------------------------------------------------------------------------

/// Bundles the parameters shared across chat and tool-calling methods.
///
/// All fields are optional so callers only fill in what they need.
#[derive(Default)]
pub struct ChatRequest<'a> {
    pub prompt: Option<&'a str>,
    /// Multimodal content blocks for the user message (text + image).
    /// When set, takes precedence over `prompt`.
    pub user_content: Option<Vec<Value>>,
    pub system_prompt: Option<&'a str>,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub messages: Option<Vec<Value>>,
    pub max_tokens: Option<u32>,
    pub tools: Option<&'a ToolSet>,
    pub tool_context: Option<&'a ToolContext>,
    pub tape: Option<&'a str>,
    pub tape_context: Option<&'a TapeContext>,
    /// Optional cancellation token. When cancelled, `run_tools` returns partial
    /// results at the next iteration boundary.
    pub cancellation: Option<CancellationToken>,
    /// Context window size in tokens. When set, `apply_context_budget` and the
    /// tool loop use this to compute char thresholds instead of hardcoded constants.
    pub context_window: Option<usize>,
    /// Maximum tool-calling iterations. Defaults to 250 when `None`.
    pub max_tool_iterations: Option<usize>,
    /// Opaque routing hint forwarded to provider-aware adapters. The Anthropic
    /// adapter maps it to `metadata.user_id`; OpenAI keeps it local because the
    /// public API rejects a top-level `session_id`.
    pub session_id: Option<&'a str>,
    /// Optional per-turn token budget (input + output, summed across every
    /// tool-loop round). When set, the loop stops cleanly once accumulated usage
    /// reaches the budget — a cost circuit breaker against runaway tool loops.
    /// `None` (default) means unlimited, preserving prior behavior.
    pub token_budget: Option<u64>,
}

// ---------------------------------------------------------------------------
// LLM (public facade)
// ---------------------------------------------------------------------------

/// Developer-first LLM client powered by any-llm.
pub struct LLM {
    core: LLMCore,
    tool_executor: ToolExecutor,
    async_tape: AsyncTapeManager,
    spill_dir: Option<std::path::PathBuf>,
    stream_filter: Option<StreamEventFilter>,
    /// Model context window in tokens, used for tape budget and tool loop limits.
    pub(crate) context_window: Option<usize>,
}

impl LLM {
    /// Create a new LLM client.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: Option<&str>,
        provider: Option<&str>,
        fallback_models: Option<Vec<String>>,
        max_retries: Option<u32>,
        api_key: Option<String>,
        api_key_map: Option<std::collections::HashMap<String, String>>,
        api_base: Option<String>,
        api_base_map: Option<std::collections::HashMap<String, String>>,
        api_format: Option<ApiFormat>,
        verbose: Option<u32>,
        context: Option<TapeContext>,
    ) -> Result<Self, ConduitError> {
        let verbose = verbose.unwrap_or(0);
        if verbose > 2 {
            return Err(ConduitError::new(
                ErrorKind::InvalidInput,
                "verbose must be 0, 1, or 2",
            ));
        }

        let max_retries = max_retries.unwrap_or(3);
        let model_str = model.unwrap_or(DEFAULT_MODEL);

        let (resolved_provider, resolved_model) =
            LLMCore::resolve_model_provider(model_str, provider)?;

        let api_key_config = match (api_key, api_key_map) {
            (Some(key), _) => ApiKeyConfig::Single(key),
            (None, Some(map)) => ApiKeyConfig::PerProvider(map),
            (None, None) => ApiKeyConfig::None,
        };

        let api_base_config = match (api_base, api_base_map) {
            (Some(base), _) => ApiBaseConfig::Single(base),
            (None, Some(map)) => ApiBaseConfig::PerProvider(map),
            (None, None) => ApiBaseConfig::None,
        };

        let api_format = api_format.unwrap_or_default();

        let core = LLMCore::new(
            resolved_provider,
            resolved_model,
            fallback_models.unwrap_or_default(),
            max_retries,
            api_key_config,
            api_base_config,
            api_format,
            verbose,
        );

        let shared_tape_store = InMemoryTapeStore::new();
        let async_store = AsyncTapeStoreAdapter::new(shared_tape_store);
        let async_tape = AsyncTapeManager::new(Some(Box::new(async_store)), context);

        Ok(Self {
            core,
            tool_executor: ToolExecutor::new(),
            async_tape,
            stream_filter: None,
            spill_dir: None,
            context_window: None,
        })
    }

    /// Return a new [`LLMBuilder`].
    pub fn builder() -> LLMBuilder {
        LLMBuilder::new()
    }

    // -- Accessors -----------------------------------------------------------

    /// The resolved model name.
    pub fn model(&self) -> &str {
        self.core.model()
    }

    /// The resolved provider name.
    pub fn provider(&self) -> &str {
        self.core.provider()
    }

    /// The fallback models.
    pub fn fallback_models(&self) -> &[String] {
        self.core.fallback_models()
    }

    /// Access the tool executor.
    pub fn tools(&self) -> &ToolExecutor {
        &self.tool_executor
    }

    /// The context window size in tokens, if set.
    pub fn context_window(&self) -> Option<usize> {
        self.context_window
    }

    /// Set the context window size in tokens.
    pub fn set_context_window(&mut self, tokens: usize) {
        self.context_window = Some(tokens);
    }

    /// Set a stream event filter. Events returning `None` are dropped.
    pub fn with_stream_filter(&mut self, filter: StreamEventFilter) {
        self.stream_filter = Some(filter);
    }

    /// Remove any previously set stream event filter.
    pub fn clear_stream_filter(&mut self) {
        self.stream_filter = None;
    }

    /// Set the tape context used for conversation history selection.
    pub fn set_context(&mut self, context: TapeContext) {
        self.async_tape.set_default_context(context);
    }

    /// Return a reference to the current tape context, if one is set.
    pub fn context(&self) -> Option<&TapeContext> {
        Some(self.async_tape.default_context())
    }

    /// Append a raw tape entry to the named tape (async).
    pub async fn append_tape_entry(
        &self,
        tape: &str,
        entry: &crate::tape::TapeEntry,
    ) -> Result<(), ConduitError> {
        self.async_tape.append_entry(tape, entry).await
    }

    /// Record a handoff (anchor + event) to the named tape (async).
    pub async fn handoff_tape(
        &self,
        tape: &str,
        name: &str,
        state: Option<Value>,
        meta: Value,
    ) -> Result<Vec<crate::tape::TapeEntry>, ConduitError> {
        self.async_tape.handoff(tape, name, state, meta).await
    }

    /// Create a [`TapeSession`](crate::tape::TapeSession) bound to a tape name.
    pub fn session(&mut self, tape: impl Into<String>) -> crate::tape::TapeSession<'_> {
        crate::tape::TapeSession::new(self, tape)
    }

    /// Access the current stream filter, if any.
    pub fn stream_filter(&self) -> Option<&StreamEventFilter> {
        self.stream_filter.as_ref()
    }

    // -- Sync wrappers -------------------------------------------------------

    /// Synchronous wrapper for [`chat_async`](Self::chat_async).
    ///
    /// Creates a single-threaded tokio runtime and blocks the current thread.
    ///
    /// # Panics
    /// Panics if called from within an async runtime context.
    pub fn chat_sync(&mut self, req: ChatRequest<'_>) -> Result<String, ConduitError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                ConduitError::new(ErrorKind::Unknown, format!("Failed to create runtime: {e}"))
            })?;
        rt.block_on(self.chat_async(req))
    }

    /// Synchronous wrapper for [`run_tools`](Self::run_tools).
    ///
    /// Creates a single-threaded tokio runtime and blocks the current thread.
    ///
    /// # Panics
    /// Panics if called from within an async runtime context.
    pub fn run_tools_sync(
        &mut self,
        req: ChatRequest<'_>,
    ) -> Result<crate::core::results::ToolAutoResult, ConduitError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                ConduitError::new(ErrorKind::Unknown, format!("Failed to create runtime: {e}"))
            })?;
        rt.block_on(self.run_tools(req))
    }

    /// Synchronous chat (blocks the current thread).
    ///
    /// # Panics
    /// Panics if called from within an async runtime context.
    pub fn chat(&mut self, req: ChatRequest<'_>) -> Result<String, ConduitError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                ConduitError::new(ErrorKind::Unknown, format!("Failed to create runtime: {e}"))
            })?;
        rt.block_on(self.chat_async(req))
    }

    // -- Chat ----------------------------------------------------------------

    /// Async chat completion returning the assistant text.
    pub async fn chat_async(&mut self, req: ChatRequest<'_>) -> Result<String, ConduitError> {
        let ChatRequest {
            prompt,
            user_content,
            system_prompt,
            model,
            provider,
            messages,
            max_tokens,
            tape,
            session_id,
            ..
        } = req;

        let tape_messages = match tape {
            Some(tape_name) => self.build_tape_messages(tape_name, None).await,
            None => Vec::new(),
        };

        let mut msgs = build_messages(
            prompt,
            user_content.as_deref(),
            system_prompt,
            messages.as_deref(),
        );
        prepend_tape_history(&mut msgs, tape_messages);

        let new_messages: Vec<Value> = msgs
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .cloned()
            .collect();

        let response =
            self.core
                .run_chat(
                    msgs,
                    None,
                    model,
                    provider,
                    max_tokens,
                    false,
                    None,
                    Default::default(),
                    session_id,
                    |resp: TransportResponse, _prov: &str, _model: &str, _attempt: u32| {
                        Ok(resp.payload)
                    },
                )
                .await?;

        let content = extract_content(&response)?;

        if let Some(tape_name) = tape {
            let run_id = Uuid::new_v4().to_string();
            if let Err(e) = self
                .async_tape
                .record_chat(
                    tape_name,
                    &run_id,
                    system_prompt,
                    None, // context_error
                    &new_messages,
                    Some(&content),
                    None, // tool_calls
                    None, // tool_results
                    None, // error
                    None, // usage
                    Some(self.core.provider()),
                    Some(self.core.model()),
                )
                .await
            {
                tracing::error!(error = %e, tape = %tape_name, "failed to record chat transcript");
            }
        }

        Ok(content)
    }

    // -- Responses -----------------------------------------------------------

    /// Send a raw responses-format request.
    pub async fn responses(
        &mut self,
        input: Value,
        model: Option<&str>,
        provider: Option<&str>,
    ) -> Result<Value, ConduitError> {
        let prov = provider
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.core.provider().to_string());
        let base = self
            .core
            .resolve_api_base(&prov)
            .unwrap_or_else(|| default_api_base(&prov));
        let api_key = self.core.resolve_api_key(&prov).ok_or_else(|| {
            ConduitError::new(
                ErrorKind::Config,
                format!("No API key found for provider '{prov}'"),
            )
        })?;

        let mdl = model
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.core.model().to_string());

        let url = format!("{}/responses", base.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": mdl,
            "input": input,
        });

        let client = self.core.get_client(&prov);
        let resp = client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ConduitError::new(ErrorKind::Provider, format!("HTTP request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ConduitError::new(
                ErrorKind::Provider,
                format!("HTTP {status}: {text}"),
            ));
        }

        resp.json::<Value>().await.map_err(|e| {
            ConduitError::new(
                ErrorKind::Provider,
                format!("Failed to parse responses response: {e}"),
            )
        })
    }

    // -- Embeddings ----------------------------------------------------------

    /// Embed one or more inputs.
    pub async fn embed(
        &mut self,
        inputs: EmbedInput<'_>,
        model: Option<&str>,
        provider: Option<&str>,
    ) -> Result<Vec<Vec<f64>>, ConduitError> {
        let prov = provider
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.core.provider().to_string());
        let mdl = model
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.core.model().to_string());
        let base = self
            .core
            .resolve_api_base(&prov)
            .unwrap_or_else(|| default_api_base(&prov));
        let url = format!("{base}/embeddings");

        let api_key = self.core.resolve_api_key(&prov).ok_or_else(|| {
            ConduitError::new(
                ErrorKind::Config,
                format!("No API key found for provider '{prov}'"),
            )
        })?;

        let input_val: Value = match inputs {
            EmbedInput::Single(s) => serde_json::json!(s),
            EmbedInput::Multiple(v) => serde_json::json!(v),
        };

        let body = serde_json::json!({
            "model": mdl,
            "input": input_val,
        });

        let client = self.core.get_client(&prov);
        let resp = client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ConduitError::new(ErrorKind::Provider, format!("HTTP request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ConduitError::new(
                ErrorKind::Provider,
                format!("HTTP {status}: {text}"),
            ));
        }

        let val: Value = resp.json().await.map_err(|e| {
            ConduitError::new(
                ErrorKind::Provider,
                format!("Failed to parse embedding response: {e}"),
            )
        })?;

        let data = val.get("data").and_then(|d| d.as_array()).ok_or_else(|| {
            ConduitError::new(
                ErrorKind::Provider,
                "Embedding response missing 'data' array",
            )
        })?;

        data.iter()
            .map(|item| {
                item.get("embedding")
                    .and_then(|e| e.as_array())
                    .ok_or_else(|| {
                        ConduitError::new(ErrorKind::Provider, "Embedding item missing 'embedding'")
                    })
                    .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect())
            })
            .collect()
    }

    // -- Text utilities ------------------------------------------------------

    /// Boolean question: does the input satisfy the question?
    pub async fn if_(&mut self, input_text: &str, question: &str) -> Result<bool, ConduitError> {
        let prompt = format!(
            "Answer ONLY 'yes' or 'no'. Question about the following text:\n\
             Text: {input_text}\n\
             Question: {question}"
        );
        let answer = self
            .chat_async(ChatRequest {
                prompt: Some(&prompt),
                max_tokens: Some(16),
                ..Default::default()
            })
            .await?;
        let normalized = answer.trim().to_lowercase();
        Ok(normalized.starts_with("yes"))
    }

    /// Classify input text into one of the provided choices.
    pub async fn classify(
        &mut self,
        input_text: &str,
        choices: &[String],
    ) -> Result<String, ConduitError> {
        if choices.is_empty() {
            return Err(ConduitError::new(
                ErrorKind::InvalidInput,
                "classify requires at least one choice",
            ));
        }

        let choices_str = choices
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}. {c}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "Classify the following text into exactly one of the categories below.\n\
             Reply with ONLY the category label, nothing else.\n\n\
             Categories:\n{choices_str}\n\n\
             Text: {input_text}"
        );

        let answer = self
            .chat_async(ChatRequest {
                prompt: Some(&prompt),
                max_tokens: Some(64),
                ..Default::default()
            })
            .await?;

        let trimmed = answer.trim().to_string();
        let lower = trimmed.to_lowercase();

        let matched = choices
            .iter()
            .find(|c| trimmed == **c)
            .or_else(|| {
                choices
                    .iter()
                    .find(|c| lower.starts_with(&c.to_lowercase()))
            })
            .cloned()
            .unwrap_or(trimmed);
        Ok(matched)
    }
}

impl fmt::Display for LLM {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LLM({}:{})", self.core.provider(), self.core.model())
    }
}

impl fmt::Debug for LLM {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LLM")
            .field("provider", &self.core.provider())
            .field("model", &self.core.model())
            .field("fallback_models", &self.core.fallback_models())
            .field("max_retries", &self.core.max_retries())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn default_api_base(provider: &str) -> String {
    provider_policies::default_api_base(provider)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
