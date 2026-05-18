//! Tape entries for Conduit.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::results::ErrorPayload;
use crate::core::tool_calls::{normalize_message_tool_calls, normalize_tool_calls};

pub fn utc_now() -> String {
    Utc::now().to_rfc3339()
}

/// The kind of a tape entry, replacing stringly-typed comparisons with
/// compile-time checked variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapeEntryKind {
    Anchor,
    Message,
    System,
    Event,
    ToolCall,
    ToolResult,
    Error,
    Decision,
    DecisionRevoked,
}

pub fn latest_system_content(entries: &[TapeEntry]) -> Option<&str> {
    entries
        .iter()
        .rev()
        .find(|e| e.kind == TapeEntryKind::System)
        .and_then(|e| e.payload.get("content").and_then(|c| c.as_str()))
}

/// A single append-only entry in a tape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapeEntry {
    pub id: i64,
    pub kind: TapeEntryKind,
    pub payload: Value,
    pub meta: Value,
    pub date: String,
}

impl AsRef<TapeEntry> for TapeEntry {
    fn as_ref(&self) -> &TapeEntry {
        self
    }
}

impl TapeEntry {
    /// Create a new TapeEntry with the given fields.
    pub fn new(id: i64, kind: TapeEntryKind, payload: Value, meta: Value, date: String) -> Self {
        Self {
            id,
            kind,
            payload,
            meta,
            date,
        }
    }

    /// Deep-copy this entry (clone payload and meta).
    pub fn copy(&self) -> Self {
        Self {
            id: self.id,
            kind: self.kind,
            payload: self.payload.clone(),
            meta: self.meta.clone(),
            date: self.date.clone(),
        }
    }

    /// Create a message entry.
    pub fn message(message: Value, meta: Value) -> Self {
        Self {
            id: 0,
            kind: TapeEntryKind::Message,
            payload: normalize_message_tool_calls(&message),
            meta,
            date: utc_now(),
        }
    }

    /// Create a system entry.
    pub fn system(content: &str, meta: Value) -> Self {
        let payload = serde_json::json!({ "content": content });
        Self {
            id: 0,
            kind: TapeEntryKind::System,
            payload,
            meta,
            date: utc_now(),
        }
    }

    /// Create an anchor entry.
    pub fn anchor(name: &str, state: Option<Value>, meta: Value) -> Self {
        let mut map = Map::new();
        map.insert("name".into(), Value::String(name.into()));
        if let Some(s) = state {
            map.insert("state".into(), s);
        }
        Self {
            id: 0,
            kind: TapeEntryKind::Anchor,
            payload: Value::Object(map),
            meta,
            date: utc_now(),
        }
    }

    /// Create a tool_call entry.
    pub fn tool_call(calls: Vec<Value>, meta: Value) -> Self {
        Self::tool_call_with_content(calls, None, meta)
    }

    /// Create a tool_call entry, optionally preserving assistant text that
    /// accompanied the tool call.
    pub fn tool_call_with_content(calls: Vec<Value>, content: Option<String>, meta: Value) -> Self {
        Self::tool_call_with_assistant_fields(calls, content, None, meta)
    }

    /// Create a tool_call entry while preserving provider-specific assistant
    /// fields needed when the conversation is replayed.
    pub fn tool_call_with_assistant_fields(
        calls: Vec<Value>,
        content: Option<String>,
        reasoning_content: Option<String>,
        meta: Value,
    ) -> Self {
        let mut payload = Map::new();
        payload.insert("calls".into(), Value::Array(normalize_tool_calls(&calls)));
        insert_payload_string(&mut payload, "content", content);
        insert_payload_string(&mut payload, "reasoning_content", reasoning_content);
        Self {
            id: 0,
            kind: TapeEntryKind::ToolCall,
            payload: Value::Object(payload),
            meta,
            date: utc_now(),
        }
    }

    /// Create a tool_result entry.
    pub fn tool_result(results: Vec<Value>, meta: Value) -> Self {
        let payload = serde_json::json!({ "results": results });
        Self {
            id: 0,
            kind: TapeEntryKind::ToolResult,
            payload,
            meta,
            date: utc_now(),
        }
    }

    /// Create an error entry from an ErrorPayload (matching Python's `ErrorPayload.as_dict()`).
    pub fn error(error: &ErrorPayload, meta: Value) -> Self {
        let payload = Value::Object(error.as_map());
        Self {
            id: 0,
            kind: TapeEntryKind::Error,
            payload,
            meta,
            date: utc_now(),
        }
    }

    /// Create a decision entry.
    ///
    /// Decisions are persistent commitments that survive anchor slicing and
    /// context trimming. They are injected into the system prompt on every turn.
    pub fn decision(text: &str, meta: Value) -> Self {
        let payload = serde_json::json!({ "text": text });
        Self {
            id: 0,
            kind: TapeEntryKind::Decision,
            payload,
            meta,
            date: utc_now(),
        }
    }

    /// Create a decision revocation (tombstone) entry.
    ///
    /// Marks a prior decision as revoked by matching its text content.
    /// The original decision entry remains in the tape (append-only);
    /// this tombstone causes it to be excluded from context building.
    pub fn decision_revoked(text: &str, meta: Value) -> Self {
        let payload = serde_json::json!({ "text": text });
        Self {
            id: 0,
            kind: TapeEntryKind::DecisionRevoked,
            payload,
            meta,
            date: utc_now(),
        }
    }

    /// Create an event entry.
    pub fn event(name: &str, data: Option<Value>, meta: Value) -> Self {
        let mut map = Map::new();
        map.insert("name".into(), Value::String(name.into()));
        if let Some(d) = data {
            map.insert("data".into(), d);
        }
        Self {
            id: 0,
            kind: TapeEntryKind::Event,
            payload: Value::Object(map),
            meta,
            date: utc_now(),
        }
    }
}

fn insert_payload_string(payload: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(text) = value.filter(|text| !text.is_empty()) {
        payload.insert(key.into(), Value::String(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decision_entry_creation() {
        let meta = serde_json::json!({ "turn": 42 });
        let entry = TapeEntry::decision("Use PostgreSQL for storage", meta);
        assert_eq!(entry.kind, TapeEntryKind::Decision);
        assert_eq!(
            entry.payload.get("text").and_then(|v| v.as_str()),
            Some("Use PostgreSQL for storage")
        );
        assert_eq!(entry.id, 0);
        assert!(!entry.date.is_empty());
    }

    #[test]
    fn test_decision_revoked_entry() {
        let meta = serde_json::json!({});
        let entry = TapeEntry::decision_revoked("Use PostgreSQL for storage", meta);
        assert_eq!(entry.kind, TapeEntryKind::DecisionRevoked);
        assert_eq!(
            entry.payload.get("text").and_then(|v| v.as_str()),
            Some("Use PostgreSQL for storage")
        );
    }

    #[test]
    fn test_latest_system_content_empty_entries() {
        assert_eq!(latest_system_content(&[]), None);
    }

    #[test]
    fn test_latest_system_content_no_system_entries() {
        let meta = serde_json::json!({});
        let entries = vec![
            TapeEntry::message(
                serde_json::json!({"role": "user", "content": "hi"}),
                meta.clone(),
            ),
            TapeEntry::message(
                serde_json::json!({"role": "assistant", "content": "hello"}),
                meta,
            ),
        ];
        assert_eq!(latest_system_content(&entries), None);
    }

    #[test]
    fn test_latest_system_content_single() {
        let meta = serde_json::json!({});
        let entries = vec![
            TapeEntry::system("you are helpful", meta.clone()),
            TapeEntry::message(serde_json::json!({"role": "user", "content": "hi"}), meta),
        ];
        assert_eq!(latest_system_content(&entries), Some("you are helpful"));
    }

    #[test]
    fn test_latest_system_content_returns_last() {
        let meta = serde_json::json!({});
        let entries = vec![
            TapeEntry::system("prompt v1", meta.clone()),
            TapeEntry::message(
                serde_json::json!({"role": "user", "content": "hi"}),
                meta.clone(),
            ),
            TapeEntry::system("prompt v2", meta.clone()),
            TapeEntry::message(serde_json::json!({"role": "user", "content": "bye"}), meta),
        ];
        assert_eq!(latest_system_content(&entries), Some("prompt v2"));
    }

    #[test]
    fn test_decision_empty_and_long_text() {
        let meta = serde_json::json!({});
        let empty = TapeEntry::decision("", meta.clone());
        assert_eq!(empty.payload.get("text").and_then(|v| v.as_str()), Some(""));

        let long_text = "x".repeat(5000);
        let long_entry = TapeEntry::decision(&long_text, meta);
        assert_eq!(
            long_entry
                .payload
                .get("text")
                .and_then(|v| v.as_str())
                .map(|s| s.len()),
            Some(5000)
        );
    }
}
