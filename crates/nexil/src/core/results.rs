//! Structured results and errors for Conduit.

use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::errors::ErrorKind;

/// Serializable error payload for streams and results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl std::fmt::Display for ErrorPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.kind, self.message)
    }
}

impl std::error::Error for ErrorPayload {}

impl ErrorPayload {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Serialize to a JSON map.
    pub fn as_map(&self) -> serde_json::Map<String, Value> {
        let required = [
            ("kind", Value::String(self.kind.as_str().to_owned())),
            ("message", Value::String(self.message.clone())),
        ];
        let optional = self.details.as_ref().map(|d| ("details", d.clone()));

        required
            .into_iter()
            .chain(optional)
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
    }
}

/// Post-stream state: error and usage populated after iteration ends.
#[derive(Debug, Clone, Default)]
pub struct StreamState {
    pub error: Option<ErrorPayload>,
    pub usage: Option<Value>,
}

impl StreamState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Synchronous text chunk stream.
pub struct TextStream {
    iterator: Box<dyn Iterator<Item = String> + Send>,
    state: StreamState,
}

impl TextStream {
    pub fn new(
        iterator: impl Iterator<Item = String> + Send + 'static,
        state: Option<StreamState>,
    ) -> Self {
        Self {
            iterator: Box::new(iterator),
            state: state.unwrap_or_default(),
        }
    }

    pub fn error(&self) -> Option<&ErrorPayload> {
        self.state.error.as_ref()
    }

    pub fn usage(&self) -> Option<&Value> {
        self.state.usage.as_ref()
    }

    pub fn state_mut(&mut self) -> &mut StreamState {
        &mut self.state
    }
}

impl Iterator for TextStream {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        self.iterator.next()
    }
}

/// Asynchronous text chunk stream.
pub struct AsyncTextStream {
    stream: Pin<Box<dyn Stream<Item = String> + Send>>,
    state: StreamState,
}

impl AsyncTextStream {
    pub fn new(
        stream: impl Stream<Item = String> + Send + 'static,
        state: Option<StreamState>,
    ) -> Self {
        Self {
            stream: Box::pin(stream),
            state: state.unwrap_or_default(),
        }
    }

    pub fn error(&self) -> Option<&ErrorPayload> {
        self.state.error.as_ref()
    }

    pub fn usage(&self) -> Option<&Value> {
        self.state.usage.as_ref()
    }

    pub fn state_mut(&mut self) -> &mut StreamState {
        &mut self.state
    }

    pub fn into_stream(self) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        self.stream
    }
}

/// The kind tag for a `StreamEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamEventKind {
    Text,
    ToolCall,
    ToolResult,
    Usage,
    Error,
    Final,
}

/// Single event from a structured stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEvent {
    pub kind: StreamEventKind,
    pub data: Value,
}

impl StreamEvent {
    pub fn new(kind: StreamEventKind, data: Value) -> Self {
        Self { kind, data }
    }
}

/// Synchronous `StreamEvent` iterator.
pub struct StreamEvents {
    iterator: Box<dyn Iterator<Item = StreamEvent> + Send>,
    state: StreamState,
}

impl StreamEvents {
    pub fn new(
        iterator: impl Iterator<Item = StreamEvent> + Send + 'static,
        state: Option<StreamState>,
    ) -> Self {
        Self {
            iterator: Box::new(iterator),
            state: state.unwrap_or_default(),
        }
    }

    pub fn error(&self) -> Option<&ErrorPayload> {
        self.state.error.as_ref()
    }

    pub fn usage(&self) -> Option<&Value> {
        self.state.usage.as_ref()
    }

    pub fn state_mut(&mut self) -> &mut StreamState {
        &mut self.state
    }
}

impl Iterator for StreamEvents {
    type Item = StreamEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.iterator.next()
    }
}

/// Asynchronous `StreamEvent` stream.
pub struct AsyncStreamEvents {
    stream: Pin<Box<dyn Stream<Item = StreamEvent> + Send>>,
    state: StreamState,
}

impl AsyncStreamEvents {
    pub fn new(
        stream: impl Stream<Item = StreamEvent> + Send + 'static,
        state: Option<StreamState>,
    ) -> Self {
        Self {
            stream: Box::pin(stream),
            state: state.unwrap_or_default(),
        }
    }

    pub fn error(&self) -> Option<&ErrorPayload> {
        self.state.error.as_ref()
    }

    pub fn usage(&self) -> Option<&Value> {
        self.state.usage.as_ref()
    }

    pub fn state_mut(&mut self) -> &mut StreamState {
        &mut self.state
    }

    pub fn into_stream(self) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        self.stream
    }
}

/// The result of executing tool calls in a single round.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolExecution {
    #[serde(default)]
    pub tool_calls: Vec<Value>,
    #[serde(default)]
    pub tool_results: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

/// Token usage from a single API call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt-cache *write* tokens (Anthropic `cache_creation_input_tokens`):
    /// fresh tokens billed at ~1.25x while the cache entry is created. 0 when
    /// caching is off or unsupported.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Prompt-cache *read* tokens (Anthropic `cache_read_input_tokens`): tokens
    /// served from cache at ~0.1x cost. This is the prompt-caching win — high
    /// read counts on repeat turns mean the stable prefix is being reused.
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    pub timestamp: String,
}

impl UsageEvent {
    /// Extract a `UsageEvent` from a raw API response's `"usage"` field.
    pub fn from_raw(raw: &Value, model: &str) -> Option<Self> {
        let usage = raw.as_object()?;
        // Accept both Anthropic-style (input_tokens/output_tokens) and
        // OpenAI-style (prompt_tokens/completion_tokens) field names.
        let field = |primary: &str, fallback: &str| {
            usage
                .get(primary)
                .or_else(|| usage.get(fallback))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        Some(Self {
            model: model.to_owned(),
            input_tokens: field("input_tokens", "prompt_tokens"),
            output_tokens: field("output_tokens", "completion_tokens"),
            cache_creation_input_tokens: field("cache_creation_input_tokens", ""),
            cache_read_input_tokens: field("cache_read_input_tokens", ""),
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    /// Total non-cached tokens billed for this event (input + output). Cache
    /// reads/writes are tracked separately and excluded so budget accounting
    /// stays on the primary billed counters.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// The kind tag for a `ToolAutoResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAutoResultKind {
    Text,
    Tools,
    Error,
}

/// Final result of an automatic tool-execution loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAutoResult {
    pub kind: ToolAutoResultKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub tool_calls: Vec<Value>,
    pub tool_results: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
    /// Token usage events from all API calls in this tool-execution loop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<UsageEvent>,
}

impl ToolAutoResult {
    /// Construct a text-only result.
    pub fn text_result(text: impl Into<String>) -> Self {
        Self {
            kind: ToolAutoResultKind::Text,
            text: Some(text.into()),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            error: None,
            usage: Vec::new(),
        }
    }

    /// Construct a tools result (successful tool-call round).
    pub fn tools_result(tool_calls: Vec<Value>, tool_results: Vec<Value>) -> Self {
        Self {
            kind: ToolAutoResultKind::Tools,
            text: None,
            tool_calls,
            tool_results,
            error: None,
            usage: Vec::new(),
        }
    }

    /// Construct an error result, optionally carrying partial tool data.
    pub fn error_result(
        error: ErrorPayload,
        tool_calls: Option<Vec<Value>>,
        tool_results: Option<Vec<Value>>,
    ) -> Self {
        Self {
            kind: ToolAutoResultKind::Error,
            text: None,
            tool_calls: tool_calls.unwrap_or_default(),
            tool_results: tool_results.unwrap_or_default(),
            error: Some(error),
            usage: Vec::new(),
        }
    }

    /// Total input + output tokens across all usage events.
    pub fn total_tokens(&self) -> u64 {
        self.usage.iter().map(|u| u.total_tokens()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_event_from_raw_parses_cache_tokens() {
        let raw = json!({
            "input_tokens": 100,
            "output_tokens": 42,
            "cache_creation_input_tokens": 20,
            "cache_read_input_tokens": 80,
        });
        let ev = UsageEvent::from_raw(&raw, "claude").unwrap();
        assert_eq!(ev.input_tokens, 100);
        assert_eq!(ev.output_tokens, 42);
        assert_eq!(ev.cache_creation_input_tokens, 20);
        assert_eq!(ev.cache_read_input_tokens, 80);
        // Cache tokens are tracked but excluded from the billed input+output total.
        assert_eq!(ev.total_tokens(), 142);
    }

    #[test]
    fn usage_event_from_raw_defaults_cache_tokens_to_zero() {
        let raw = json!({"input_tokens": 10, "output_tokens": 5});
        let ev = UsageEvent::from_raw(&raw, "gpt").unwrap();
        assert_eq!(ev.cache_creation_input_tokens, 0);
        assert_eq!(ev.cache_read_input_tokens, 0);
    }
}
