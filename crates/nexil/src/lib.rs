#![cfg_attr(docsrs, feature(doc_auto_cfg))]
//! Provider-agnostic LLM toolkit — streaming, tool calls, tape storage, OAuth.
//!
//! # Key types
//!
//! | Type | Purpose |
//! |------|---------|
//! | [`LLM`] | High-level facade for chat completions and tool-calling loops. |
//! | [`LLMBuilder`] | Fluent builder for configuring an [`LLM`] instance. |
//! | [`ChatRequest`] | Parameters for a single chat completion or tool-calling request. |
//! | [`StreamEvent`] | A single event emitted by a streaming LLM response. |
//! | [`Tool`] | A callable unit the model can invoke, with optional handler. |
//! | [`ToolSet`] | A validated collection of tools ready to send as the `tools` parameter. |
//! | [`TapeContext`] | Configuration for building a prompt context window from tape entries. |
//! | [`TapeManager`] | Persistent tape storage and retrieval. |
//! | [`ConduitError`] | Unified error type across all nexil operations. |

pub(crate) mod adapter;
pub mod auth;
pub mod clients;
pub mod core;
pub mod llm;
pub(crate) mod providers;
pub mod tape;
pub mod tools;

// Re-export top-level public types for convenience.
pub use crate::auth::{
    APIKeyResolver, CodexOAuthLoginError, GitHubCopilotOAuthLoginError, GitHubCopilotOAuthTokens,
    OpenAICodexOAuthTokens, codex_cli_api_key_resolver, github_copilot_oauth_resolver,
    load_openai_codex_oauth_tokens, login_github_copilot_oauth, login_openai_codex_oauth,
    multi_api_key_resolver,
};
pub use crate::core::errors::{ConduitError, ErrorKind};
pub use crate::core::execution::OAuthTokenRefresher;
pub use crate::core::provider_registry::{ProviderConfig, ProviderRegistry};
pub use crate::core::results::{
    AsyncStreamEvents, AsyncTextStream, ErrorPayload, StreamEvent, StreamEvents, StreamState,
    TextStream, ToolAutoResult, ToolExecution, UsageEvent,
};
pub use crate::llm::{
    ApiFormat, ChatRequest, LLM, LLMBuilder,
    collect_active_decisions, inject_decisions_into_system_prompt,
};
pub use crate::tape::{
    AnchorSelector, TapeContext, TapeEntry, TapeEntryKind, AsyncTapeManager, TapeQuery, TapeSession,
};
pub use crate::tools::{
    Tool, ToolAction, ToolCallResponse, ToolContext, ToolExecutor, ToolSet,
};
pub use tokio_util::sync::CancellationToken;
