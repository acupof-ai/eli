//! OpenAI chat-completions shape parsing.

use serde_json::Value;

use super::common::{expand_tool_calls, field, field_str};
use super::types::{BaseTransportParser, ToolCallDelta};
use crate::core::tool_calls::{parse_dsml_tool_calls, strip_dsml_tool_call_block};

/// Parser for the OpenAI chat-completions response format.
pub struct CompletionTransportParser;

fn first_choice(response: &Value) -> Option<&Value> {
    field(response, "choices")
        .and_then(|c| c.as_array())
        .filter(|c| !c.is_empty())
        .map(|c| &c[0])
}

fn first_choice_delta(chunk: &Value) -> Option<&Value> {
    first_choice(chunk).and_then(|c| field(c, "delta"))
}

fn first_choice_message(response: &Value) -> Option<&Value> {
    first_choice(response).and_then(|c| field(c, "message"))
}

fn normalize_completion_tool_call(tool_call: &Value) -> Option<Value> {
    let function = field(tool_call, "function").filter(|f| f.is_object())?;
    let name = field(function, "name").cloned().unwrap_or(Value::Null);
    let arguments = field(function, "arguments").cloned().unwrap_or(Value::Null);

    let mut entry = serde_json::Map::new();
    entry.insert(
        "function".to_owned(),
        serde_json::json!({"name": name, "arguments": arguments}),
    );

    for key in ["id", "type"] {
        if let Some(v) = field(tool_call, key).filter(|v| !v.is_null()) {
            entry.insert(key.to_owned(), v.clone());
        }
    }

    Some(Value::Object(entry))
}

fn delta_to_tool_call_delta(tc: &Value) -> ToolCallDelta {
    let func = tc.get("function");
    ToolCallDelta {
        id: tc.get("id").and_then(|v| v.as_str()).map(|s| s.to_owned()),
        index: tc.get("index").cloned(),
        call_type: tc
            .get("type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned()),
        name: func
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_owned(),
        arguments: func
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
            .unwrap_or("")
            .to_owned(),
        arguments_complete: false,
    }
}

const USAGE_KEY_MAPPINGS: &[(&[&str], &str)] = &[
    (&["input_tokens", "prompt_tokens"], "input_tokens"),
    (&["output_tokens", "completion_tokens"], "output_tokens"),
    (&["total_tokens"], "total_tokens"),
    (&["requests"], "requests"),
    // Pass through cache fields so UsageEvent::from_raw can read them.
    (&["prompt_cache_hit_tokens"], "prompt_cache_hit_tokens"),
    (&["prompt_tokens_details"], "prompt_tokens_details"),
];

fn normalize_usage_fields(
    usage_obj: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    USAGE_KEY_MAPPINGS
        .iter()
        .filter_map(|(source_keys, target_key)| {
            let value = source_keys.iter().find_map(|key| usage_obj.get(*key))?;
            Some(((*target_key).to_owned(), value.clone()))
        })
        .collect()
}

impl BaseTransportParser for CompletionTransportParser {
    fn is_non_stream_response(&self, response: &Value) -> bool {
        response.is_string() || response.get("choices").is_some()
    }

    fn extract_chunk_tool_call_deltas(&self, chunk: &Value) -> Vec<ToolCallDelta> {
        let delta = match first_choice_delta(chunk) {
            Some(d) => d,
            None => return Vec::new(),
        };
        let tool_calls = match field(delta, "tool_calls").and_then(|tc| tc.as_array()) {
            Some(tc) => tc,
            None => return Vec::new(),
        };

        tool_calls.iter().map(delta_to_tool_call_delta).collect()
    }

    fn extract_chunk_text(&self, chunk: &Value) -> String {
        first_choice_delta(chunk)
            .map(|d| field_str(d, "content").to_owned())
            .unwrap_or_default()
    }

    fn extract_chunk_reasoning(&self, chunk: &Value) -> String {
        first_choice_delta(chunk)
            .map(|d| field_str(d, "reasoning_content").to_owned())
            .unwrap_or_default()
    }

    fn extract_text(&self, response: &Value) -> String {
        if let Some(s) = response.as_str() {
            return s.to_owned();
        }
        first_choice_message(response)
            .map(|m| {
                strip_dsml_tool_call_block(field_str(m, "content"))
                    .trim()
                    .to_owned()
            })
            .unwrap_or_default()
    }

    fn extract_tool_calls(&self, response: &Value) -> Vec<Value> {
        let Some(message) = first_choice_message(response) else {
            return Vec::new();
        };
        let Some(tool_calls) = field(message, "tool_calls").and_then(|tc| tc.as_array()) else {
            return parse_dsml_tool_calls(field_str(message, "content"));
        };
        let calls = tool_calls
            .iter()
            .filter_map(normalize_completion_tool_call)
            .collect();
        expand_tool_calls(calls)
    }

    fn extract_usage(&self, response: &Value) -> Option<Value> {
        let usage_obj = field(response, "usage")?.as_object()?;
        let normalized = normalize_usage_fields(usage_obj);
        (!normalized.is_empty()).then_some(Value::Object(normalized))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_text() {
        let parser = CompletionTransportParser;
        let response = json!({
            "choices": [{
                "message": {
                    "content": "Hello world"
                }
            }]
        });
        assert_eq!(parser.extract_text(&response), "Hello world");
    }

    #[test]
    fn test_extract_text_string() {
        let parser = CompletionTransportParser;
        let response = Value::String("raw text".into());
        assert_eq!(parser.extract_text(&response), "raw text");
    }

    #[test]
    fn test_extract_chunk_text() {
        let parser = CompletionTransportParser;
        let chunk = json!({
            "choices": [{
                "delta": {
                    "content": "chunk"
                }
            }]
        });
        assert_eq!(parser.extract_chunk_text(&chunk), "chunk");
    }

    #[test]
    fn test_extract_chunk_reasoning() {
        let parser = CompletionTransportParser;
        let chunk = json!({
            "choices": [{
                "delta": {
                    "reasoning_content": "thinking",
                    "content": "chunk"
                }
            }]
        });
        assert_eq!(parser.extract_chunk_reasoning(&chunk), "thinking");
        // Chunks without reasoning_content (or without a delta) yield empty.
        assert_eq!(parser.extract_chunk_reasoning(&json!({"choices": [{}]})), "");
    }

    #[test]
    fn test_is_non_stream_response() {
        let parser = CompletionTransportParser;
        assert!(parser.is_non_stream_response(&json!({"choices": []})));
        assert!(!parser.is_non_stream_response(&json!({"other": true})));
    }

    #[test]
    fn test_extract_usage_prompt_tokens() {
        let parser = CompletionTransportParser;
        let response = json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            }
        });
        let usage = parser.extract_usage(&response).unwrap();
        assert_eq!(usage["input_tokens"], 10);
        assert_eq!(usage["output_tokens"], 20);
        assert_eq!(usage["total_tokens"], 30);
    }

    #[test]
    fn test_extract_tool_calls() {
        let parser = CompletionTransportParser;
        let response = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\": \"London\"}"
                        }
                    }]
                }
            }]
        });
        let calls = parser.extract_tool_calls(&response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "get_weather");
    }

    #[test]
    fn test_extract_tool_calls_dsml_fallback() {
        let parser = CompletionTransportParser;
        let response = json!({
            "choices": [{
                "message": {
                    "content": "<｜DSML｜tool_calls><｜DSML｜invoke name=\"echo\"></｜DSML｜invoke></｜DSML｜tool_calls>"
                }
            }]
        });

        let calls = parser.extract_tool_calls(&response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "echo");
        assert_eq!(parser.extract_text(&response), "");
    }
}
