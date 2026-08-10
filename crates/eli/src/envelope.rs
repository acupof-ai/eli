//! Utilities for reading and normalizing envelopes (serde_json::Value).

use anyhow::anyhow;
use serde_json::Value;

use crate::types::Envelope;

/// Project-specific convenience methods for JSON envelopes.
pub trait ValueExt {
    /// Read a field from an envelope (must be a JSON object).
    fn field(&self, key: &str, default: Option<&Value>) -> Option<Value>;

    /// Read a field as a string, with an optional default.
    fn field_str(&self, key: &str, default: &str) -> String;

    /// Read a field as a string slice.
    fn get_str_field(&self, key: &str) -> Option<&str>;

    /// Read a required string field.
    fn require_str_field(&self, key: &str) -> Result<&str, anyhow::Error>;

    /// Read a field as an integer.
    fn get_i64_field(&self, key: &str) -> Option<i64>;

    /// Read a field as a boolean.
    fn get_bool_field(&self, key: &str) -> Option<bool>;

    /// Get textual content from any envelope shape.
    fn content_text(&self) -> String;
}

impl ValueExt for Value {
    fn field(&self, key: &str, default: Option<&Value>) -> Option<Value> {
        self.as_object()
            .and_then(|obj| obj.get(key).cloned())
            .or_else(|| default.cloned())
    }

    fn field_str(&self, key: &str, default: &str) -> String {
        self.as_object()
            .and_then(|obj| obj.get(key))
            .map(value_to_string)
            .unwrap_or_else(|| default.to_owned())
    }

    fn get_str_field(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    fn require_str_field(&self, key: &str) -> Result<&str, anyhow::Error> {
        self.get(key)
            .ok_or_else(|| anyhow!("missing required argument '{key}'"))
            .and_then(|value| {
                value
                    .as_str()
                    .ok_or_else(|| anyhow!("argument '{key}' must be a string, got {value}"))
            })
    }

    fn get_i64_field(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(Value::as_i64)
    }

    fn get_bool_field(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(Value::as_bool)
    }

    fn content_text(&self) -> String {
        match self.as_object() {
            Some(obj) => obj.get("content").map(value_to_string).unwrap_or_default(),
            None => value_to_string(self),
        }
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
fn field_of(message: &Envelope, key: &str, default: Option<&Value>) -> Option<Value> {
    message.field(key, default)
}

#[cfg(test)]
fn field_of_str(message: &Envelope, key: &str, default: &str) -> String {
    message.field_str(key, default)
}

#[cfg(test)]
fn str_field_of<'a>(message: &'a Envelope, key: &'a str) -> Option<&'a str> {
    message.get_str_field(key)
}

#[cfg(test)]
fn required_str_field_of<'a>(
    message: &'a Envelope,
    key: &'a str,
) -> Result<&'a str, anyhow::Error> {
    message.require_str_field(key)
}

#[cfg(test)]
fn i64_field_of(message: &Envelope, key: &str) -> Option<i64> {
    message.get_i64_field(key)
}

#[cfg(test)]
fn bool_field_of(message: &Envelope, key: &str) -> Option<bool> {
    message.get_bool_field(key)
}

#[cfg(test)]
fn content_of(message: &Envelope) -> String {
    message.content_text()
}

/// Flatten nested batches into a single list of envelopes.
pub fn unpack_batch_vec(batches: Vec<Vec<Envelope>>) -> Vec<Envelope> {
    batches.into_iter().flatten().collect()
}

/// A validated outbound message extracted from a raw Envelope.
#[derive(Debug, Clone)]
pub struct OutboundMessage {
    pub channel: String,
    pub session_id: String,
    pub chat_id: String,
    pub content: String,
    pub context: serde_json::Map<String, Value>,
    pub raw: Value,
}

impl OutboundMessage {
    /// Extract and validate an outbound message from a raw envelope.
    /// Returns the validated message, logging warnings for any missing fields that fall back to defaults.
    pub fn from_envelope(envelope: &Value, default_channel: &str, default_session: &str) -> Self {
        let channel = envelope
            .get("output_channel")
            .and_then(|v| v.as_str())
            .or_else(|| envelope.get("channel").and_then(|v| v.as_str()))
            .unwrap_or(default_channel);

        let session_id = envelope
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or(default_session);

        let chat_id = envelope
            .get("chat_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default");

        let content = envelope.content_text();

        let context = envelope
            .get("context")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        // Log warnings for missing routing fields
        if envelope.get("output_channel").is_none() && envelope.get("channel").is_none() {
            tracing::debug!(
                default = default_channel,
                "outbound envelope missing 'output_channel' / 'channel', using default"
            );
        }

        Self {
            channel: channel.to_string(),
            session_id: session_id.to_string(),
            chat_id: chat_id.to_string(),
            content,
            context,
            raw: envelope.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- field_of tests -------------------------------------------------------

    #[test]
    fn test_field_of_object() {
        let msg = json!({"channel": "telegram", "content": "hello"});
        assert_eq!(
            field_of(&msg, "channel", None),
            Some(Value::String("telegram".into()))
        );
        assert_eq!(field_of(&msg, "missing", None), None);
        assert_eq!(
            field_of(&msg, "missing", Some(&Value::String("fallback".into()))),
            Some(Value::String("fallback".into()))
        );
    }

    #[test]
    fn test_field_of_non_object_returns_default() {
        let msg = json!("just a string");
        assert_eq!(field_of(&msg, "content", None), None);
        assert_eq!(
            field_of(&msg, "content", Some(&json!("fb"))),
            Some(json!("fb"))
        );
    }

    #[test]
    fn test_field_of_numeric_value() {
        let msg = json!({"count": 3});
        assert_eq!(field_of(&msg, "count", None), Some(json!(3)));
    }

    // -- field_of_str tests ---------------------------------------------------

    #[test]
    fn test_field_of_str() {
        let msg = json!({"channel": "telegram"});
        assert_eq!(field_of_str(&msg, "channel", "default"), "telegram");
        assert_eq!(field_of_str(&msg, "missing", "default"), "default");
    }

    #[test]
    fn test_field_of_str_non_string_value_stringified() {
        let msg = json!({"count": 123});
        assert_eq!(field_of_str(&msg, "count", "0"), "123");
    }

    #[test]
    fn test_field_of_str_non_object_returns_default() {
        let msg = json!(42);
        assert_eq!(field_of_str(&msg, "anything", "fallback"), "fallback");
    }

    // -- get_*_field tests ---------------------------------------------------

    #[test]
    fn test_get_str_field() {
        let msg = json!({"channel": "telegram", "count": 3});
        assert_eq!(str_field_of(&msg, "channel"), Some("telegram"));
        assert_eq!(str_field_of(&msg, "count"), None);
    }

    #[test]
    fn test_require_str_field() {
        let msg = json!({"channel": "telegram", "count": 3});
        assert_eq!(required_str_field_of(&msg, "channel").unwrap(), "telegram");
        assert_eq!(
            required_str_field_of(&msg, "missing")
                .unwrap_err()
                .to_string(),
            "missing required argument 'missing'"
        );
        assert_eq!(
            required_str_field_of(&msg, "count")
                .unwrap_err()
                .to_string(),
            "argument 'count' must be a string, got 3"
        );
    }

    #[test]
    fn test_get_i64_and_bool_field() {
        let msg = json!({"count": 3, "background": true, "name": "eli"});
        assert_eq!(i64_field_of(&msg, "count"), Some(3));
        assert_eq!(i64_field_of(&msg, "name"), None);
        assert_eq!(bool_field_of(&msg, "background"), Some(true));
        assert_eq!(bool_field_of(&msg, "name"), None);
    }

    // -- content_of tests -----------------------------------------------------

    #[test]
    fn test_content_of_object() {
        let msg = json!({"content": "hello world"});
        assert_eq!(content_of(&msg), "hello world");
    }

    #[test]
    fn test_content_of_string() {
        let msg = json!("raw text");
        assert_eq!(content_of(&msg), "raw text");
    }

    #[test]
    fn test_content_of_no_content() {
        let msg = json!({"channel": "test"});
        assert_eq!(content_of(&msg), "");
    }

    #[test]
    fn test_content_of_numeric_content_stringified() {
        let msg = json!({"content": 123});
        assert_eq!(content_of(&msg), "123");
    }

    #[test]
    fn test_content_of_numeric_non_object() {
        let msg = json!(42);
        assert_eq!(content_of(&msg), "42");
    }

    #[test]
    fn test_content_of_bool_non_object() {
        let msg = json!(true);
        assert_eq!(content_of(&msg), "true");
    }

    // -- unpack_batch_vec tests -----------------------------------------------

    #[test]
    fn test_unpack_batch_vec_flattens() {
        let batches = vec![
            vec![json!({"content": "a"})],
            vec![json!({"content": "b"}), json!({"content": "c"})],
        ];
        let out = unpack_batch_vec(batches);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], json!({"content": "a"}));
        assert_eq!(out[2], json!({"content": "c"}));
    }

    #[test]
    fn test_unpack_batch_vec_empty() {
        let out = unpack_batch_vec(Vec::new());
        assert!(out.is_empty());
    }

    // -- OutboundMessage tests ------------------------------------------------

    #[test]
    fn test_outbound_message_full_envelope() {
        let env = json!({
            "output_channel": "telegram",
            "session_id": "tg:123",
            "chat_id": "456",
            "content": "hello",
            "context": {"reply_to": 789}
        });
        let msg = OutboundMessage::from_envelope(&env, "cli", "cli:default");
        assert_eq!(msg.channel, "telegram");
        assert_eq!(msg.session_id, "tg:123");
        assert_eq!(msg.chat_id, "456");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.context.get("reply_to").unwrap(), &json!(789));
    }

    #[test]
    fn test_outbound_message_falls_back_to_channel_field() {
        let env = json!({"channel": "cli", "content": "hi"});
        let msg = OutboundMessage::from_envelope(&env, "default_ch", "default_sess");
        assert_eq!(msg.channel, "cli");
    }

    #[test]
    fn test_outbound_message_defaults_on_missing_fields() {
        let env = json!({"content": "hi"});
        let msg = OutboundMessage::from_envelope(&env, "fallback_ch", "fallback_sess");
        assert_eq!(msg.channel, "fallback_ch");
        assert_eq!(msg.session_id, "fallback_sess");
        assert_eq!(msg.chat_id, "default");
        assert_eq!(msg.content, "hi");
        assert!(msg.context.is_empty());
    }

    #[test]
    fn test_outbound_message_empty_envelope() {
        let env = json!({});
        let msg = OutboundMessage::from_envelope(&env, "", "");
        assert_eq!(msg.channel, "");
        assert_eq!(msg.session_id, "");
        assert_eq!(msg.chat_id, "default");
        assert_eq!(msg.content, "");
    }

    #[test]
    fn test_outbound_message_preserves_raw() {
        let env = json!({"output_channel": "tg", "content": "x", "extra": true});
        let msg = OutboundMessage::from_envelope(&env, "", "");
        assert_eq!(msg.raw, env);
    }
}
