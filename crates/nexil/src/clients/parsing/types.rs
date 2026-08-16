//! Shared parser typing and primitives for transport response parsing.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The transport format used by an LLM provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Completion,
    Responses,
    Messages,
}

/// Tool call delta extracted from a streaming chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDelta {
    /// A provider-assigned identifier for the tool call, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// An index (or item_id) used to correlate deltas across chunks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<Value>,
    /// The type of the tool call (usually `"function"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_type: Option<String>,
    /// The function name, if present in this delta.
    pub name: String,
    /// The argument fragment (or full arguments when `arguments_complete` is true).
    pub arguments: String,
    /// Whether `arguments` contains the complete arguments string.
    pub arguments_complete: bool,
}

/// Abstract interface that transport-specific parsers implement.
pub trait BaseTransportParser: Send + Sync {
    /// Return `true` if `response` looks like a complete (non-streaming) response.
    fn is_non_stream_response(&self, response: &Value) -> bool;

    /// Extract tool-call deltas from a single streaming chunk.
    fn extract_chunk_tool_call_deltas(&self, chunk: &Value) -> Vec<ToolCallDelta>;

    /// Extract the text content from a single streaming chunk.
    fn extract_chunk_text(&self, chunk: &Value) -> String;

    /// Reasoning delta (`choices[0].delta.reasoning_content` on OpenAI-compatible
    /// transports); empty when the transport has none.
    fn extract_chunk_reasoning(&self, _chunk: &Value) -> String {
        String::new()
    }

    /// Extract the full text from a completed (non-streaming) response.
    fn extract_text(&self, response: &Value) -> String;

    /// Extract structured tool calls from a completed response.
    fn extract_tool_calls(&self, response: &Value) -> Vec<Value>;

    /// Extract usage information from a response or chunk.
    fn extract_usage(&self, response: &Value) -> Option<Value>;
}
