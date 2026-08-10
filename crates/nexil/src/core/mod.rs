//! Core utilities for Conduit.

pub(crate) mod anthropic_messages;
pub mod api_format;
pub mod client_registry;
pub mod error_classify;
pub mod errors;
pub mod execution;
pub mod message_norm;
pub mod provider_policies;
pub mod provider_registry;
pub mod provider_runtime;
pub mod request_adapters;
pub mod request_builder;
pub mod response_parser;
pub mod results;
pub(crate) mod tool_calls;

pub use api_format::ApiFormat;
pub use error_classify::{AttemptDecision, AttemptOutcome, classify_by_text_signature};
pub use errors::{ConduitError, ErrorKind};
pub use execution::{ProviderValue, LLMCore, OAuthTokenRefresher};
pub use request_builder::TransportCallRequest;
pub use response_parser::TransportResponse;
pub use results::{
    AsyncStreamEvents, AsyncTextStream, ErrorPayload, StreamEvent, StreamEventKind, StreamEvents,
    StreamState, TextStream, ToolAutoResult, ToolAutoResultKind, ToolExecution,
};
