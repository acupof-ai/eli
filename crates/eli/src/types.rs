//! Framework-neutral data aliases and core types.

use std::collections::HashMap;

use serde_json::Value;

/// An envelope is any JSON value representing a message flowing through the framework.
pub type Envelope = Value;

/// Session state is a string-keyed map of arbitrary JSON values.
pub type State = HashMap<String, Value>;

/// Internal state key used to carry the resolved system prompt through one turn.
pub const RUNTIME_SYSTEM_PROMPT_KEY: &str = "_runtime_system_prompt";

/// Internal state key for the workspace path.
pub const RUNTIME_WORKSPACE_KEY: &str = "_runtime_workspace";

/// Internal state key for the tapes directory path.
pub const RUNTIME_TAPES_DIR_KEY: &str = "_runtime_tapes_dir";

/// Token usage from a single turn.
#[derive(Debug, Clone, Default)]
pub struct TurnUsageInfo {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Result of one complete message turn through the framework.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub session_id: String,
    pub prompt: PromptValue,
    pub model_output: String,
    pub outbounds: Vec<Envelope>,
    pub usage: TurnUsageInfo,
}

/// A prompt can be plain text or a list of multimodal content parts.
#[derive(Debug, Clone)]
pub enum PromptValue {
    Text(String),
    Parts(Vec<Value>),
}

impl PromptValue {
    /// Return a plain-text representation regardless of variant.
    pub fn as_text(&self) -> String {
        match self {
            PromptValue::Text(s) => s.clone(),
            PromptValue::Parts(parts) => parts
                .iter()
                .filter_map(|part| {
                    part.as_str().map(str::to_owned).or_else(|| {
                        part.as_object()
                            .and_then(|obj| obj.get("text"))
                            .and_then(|v| v.as_str())
                            .map(str::to_owned)
                    })
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// Return `true` if the prompt is empty or blank.
    pub fn is_empty(&self) -> bool {
        match self {
            PromptValue::Text(s) => s.is_empty(),
            PromptValue::Parts(parts) => parts.is_empty(),
        }
    }

    /// Return `true` if the prompt is empty after trimming whitespace.
    pub fn is_blank(&self) -> bool {
        match self {
            PromptValue::Text(s) => s.trim().is_empty(),
            PromptValue::Parts(parts) => parts.is_empty(),
        }
    }

    /// Extract text content strictly — only parts with `"type": "text"` are
    /// included. For the Text variant, returns the string as-is.
    pub fn strict_text(&self) -> String {
        match self {
            PromptValue::Text(s) => s.clone(),
            PromptValue::Parts(parts) => parts
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(|v| v.as_str()) == Some("text") {
                        p.get("text").and_then(|v| v.as_str()).map(|s| s.to_owned())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- TurnResult tests -----------------------------------------------------

    #[test]
    fn test_turn_result_creation() {
        let result = TurnResult {
            session_id: "cli:default".into(),
            prompt: PromptValue::Text("hello".into()),
            model_output: "world".into(),
            outbounds: vec![json!({"content": "world"})],
            usage: TurnUsageInfo::default(),
        };
        assert_eq!(result.session_id, "cli:default");
        assert_eq!(result.model_output, "world");
        assert_eq!(result.outbounds.len(), 1);
    }

    #[test]
    fn test_turn_result_clone() {
        let result = TurnResult {
            session_id: "s1".into(),
            prompt: PromptValue::Text("p".into()),
            model_output: "o".into(),
            outbounds: vec![],
            usage: TurnUsageInfo::default(),
        };
        let cloned = result.clone();
        assert_eq!(cloned.session_id, "s1");
        assert_eq!(cloned.model_output, "o");
    }

    // -- PromptValue tests ----------------------------------------------------

    #[test]
    fn test_prompt_value_text_as_text() {
        let pv = PromptValue::Text("hello world".into());
        assert_eq!(pv.as_text(), "hello world");
    }

    #[test]
    fn test_prompt_value_parts_as_text_with_strings() {
        let pv = PromptValue::Parts(vec![json!("line1"), json!("line2")]);
        assert_eq!(pv.as_text(), "line1\nline2");
    }

    #[test]
    fn test_prompt_value_parts_as_text_with_text_objects() {
        let pv = PromptValue::Parts(vec![json!({"text": "part1"}), json!({"text": "part2"})]);
        assert_eq!(pv.as_text(), "part1\npart2");
    }

    #[test]
    fn test_prompt_value_parts_as_text_mixed() {
        let pv = PromptValue::Parts(vec![
            json!("plain"),
            json!({"text": "object"}),
            json!({"image": "data"}), // no text field, should be skipped
        ]);
        assert_eq!(pv.as_text(), "plain\nobject");
    }

    #[test]
    fn test_prompt_value_is_empty_text() {
        assert!(PromptValue::Text("".into()).is_empty());
        assert!(!PromptValue::Text("hello".into()).is_empty());
    }

    #[test]
    fn test_prompt_value_is_empty_parts() {
        assert!(PromptValue::Parts(vec![]).is_empty());
        assert!(!PromptValue::Parts(vec![json!("a")]).is_empty());
    }

    // -- is_blank (trim-aware) ------------------------------------------------

    #[test]
    fn test_prompt_value_is_blank_text() {
        assert!(PromptValue::Text("".into()).is_blank());
        assert!(PromptValue::Text("   ".into()).is_blank());
        assert!(PromptValue::Text(" \n\t ".into()).is_blank());
        assert!(!PromptValue::Text("hello".into()).is_blank());
        assert!(!PromptValue::Text("  hello  ".into()).is_blank());
    }

    #[test]
    fn test_prompt_value_is_blank_parts() {
        assert!(PromptValue::Parts(vec![]).is_blank());
        assert!(!PromptValue::Parts(vec![json!("a")]).is_blank());
    }

    // -- strict_text (requires "type": "text") --------------------------------

    #[test]
    fn test_prompt_value_strict_text_from_text() {
        let pv = PromptValue::Text("hello world".into());
        assert_eq!(pv.strict_text(), "hello world");
    }

    #[test]
    fn test_prompt_value_strict_text_from_parts() {
        let pv = PromptValue::Parts(vec![
            json!({"type": "text", "text": "line1"}),
            json!({"type": "image", "url": "http://example.com"}),
            json!({"type": "text", "text": "line2"}),
        ]);
        assert_eq!(pv.strict_text(), "line1\nline2");
    }

    #[test]
    fn test_prompt_value_strict_text_empty_parts() {
        let pv = PromptValue::Parts(vec![]);
        assert_eq!(pv.strict_text(), "");
    }

    #[test]
    fn test_prompt_value_strict_text_no_text_type() {
        // Parts without "type": "text" should be excluded
        let pv = PromptValue::Parts(vec![
            json!({"text": "bare"}), // no type field
            json!("plain string"),   // not an object
            json!({"type": "image", "url": "x"}),
        ]);
        assert_eq!(pv.strict_text(), "");
    }

    // -- Multimodal vision: PromptValue with image_base64 blocks ---------------

    #[test]
    fn test_prompt_value_as_text_with_image_base64_blocks() {
        let pv = PromptValue::Parts(vec![
            json!({"type": "text", "text": "describe this"}),
            json!({"type": "image_base64", "mime_type": "image/png", "data": "ABC"}),
        ]);
        // as_text extracts text from objects with "text" field — image blocks have no "text".
        assert_eq!(pv.as_text(), "describe this");
    }

    #[test]
    fn test_prompt_value_strict_text_filters_image_base64() {
        let pv = PromptValue::Parts(vec![
            json!({"type": "text", "text": "line1"}),
            json!({"type": "image_base64", "mime_type": "image/jpeg", "data": "XYZ"}),
            json!({"type": "text", "text": "line2"}),
        ]);
        // strict_text only includes "type": "text" parts.
        assert_eq!(pv.strict_text(), "line1\nline2");
    }

    #[test]
    fn test_prompt_value_image_only_parts_as_text_empty() {
        let pv = PromptValue::Parts(vec![
            json!({"type": "image_base64", "mime_type": "image/png", "data": "IMG"}),
        ]);
        assert_eq!(pv.as_text(), "");
        assert_eq!(pv.strict_text(), "");
    }

    #[test]
    fn test_prompt_value_parts_is_not_empty_with_image() {
        let pv = PromptValue::Parts(vec![
            json!({"type": "image_base64", "mime_type": "image/png", "data": "X"}),
        ]);
        assert!(!pv.is_empty());
        assert!(!pv.is_blank());
    }

    #[test]
    fn test_prompt_value_multimodal_multiple_images() {
        let pv = PromptValue::Parts(vec![
            json!({"type": "text", "text": "compare"}),
            json!({"type": "image_base64", "mime_type": "image/png", "data": "A"}),
            json!({"type": "image_base64", "mime_type": "image/jpeg", "data": "B"}),
        ]);
        assert_eq!(pv.strict_text(), "compare");
        assert_eq!(pv.as_text(), "compare");
    }
}
