use serde_json::Value;

use crate::adapter::ProviderAdapter;
use crate::clients::parsing::TransportKind;
use crate::core::anthropic_messages;
use crate::core::errors::{ConduitError, ErrorKind};
use crate::core::request_builder::TransportCallRequest;

pub static ANTHROPIC_ADAPTER: AnthropicAdapter = AnthropicAdapter;

pub struct AnthropicAdapter;

impl ProviderAdapter for AnthropicAdapter {
    fn build_request_url(&self, api_base: &str, transport: TransportKind) -> String {
        // build_request_url doesn't return Result, so keep the debug_assert
        // as a development-time guard. Callers should not reach here with a
        // wrong transport because build_request_body validates first.
        debug_assert_eq!(transport, TransportKind::Messages);
        format!("{}/messages", api_base.trim_end_matches('/'))
    }

    fn build_request_body(
        &self,
        request: &TransportCallRequest,
        transport: TransportKind,
    ) -> Result<Value, ConduitError> {
        if transport != TransportKind::Messages {
            return Err(ConduitError::new(
                ErrorKind::Config,
                "anthropic adapter only supports messages transport",
            ));
        }

        let mut body = serde_json::Map::new();
        body.insert("model".to_owned(), Value::String(request.model_id.clone()));

        let max_tokens = request.max_tokens.unwrap_or(4096);
        body.insert("max_tokens".to_owned(), Value::Number(max_tokens.into()));

        let (system_parts, messages) =
            anthropic_messages::split_system_and_conversation(&request.messages_payload);

        if let Some(system_val) = build_system_value(&system_parts, request.is_anthropic_oauth) {
            body.insert("system".to_owned(), system_val);
        }

        body.insert("messages".to_owned(), Value::Array(messages));
        insert_streaming_options(&mut body, request.is_anthropic_oauth, request.stream);

        if let Some(ref tools) = request.tools_payload
            && !tools.is_empty()
        {
            let anthropic_tools: Vec<Value> = tools.iter().map(convert_to_anthropic_tool).collect();
            body.insert("tools".to_owned(), Value::Array(anthropic_tools));
        }

        for (key, value) in &request.kwargs {
            if request.is_anthropic_oauth && key == "temperature" {
                continue;
            }
            body.entry(key.clone()).or_insert(value.clone());
        }

        if let Some(ref sid) = request.session_id
            && !request.is_anthropic_oauth
            && !body.contains_key("metadata")
        {
            body.insert("metadata".to_owned(), serde_json::json!({ "user_id": sid }));
        }

        Ok(Value::Object(body))
    }
}

const CLAUDE_CODE_SYSTEM: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

fn build_system_value(system_parts: &[String], is_anthropic_oauth: bool) -> Option<Value> {
    let claude_code_block = serde_json::json!({"type": "text", "text": CLAUDE_CODE_SYSTEM});
    if !system_parts.is_empty() {
        if is_anthropic_oauth {
            let system_text = system_parts.join("\n\n");
            Some(Value::Array(vec![
                claude_code_block,
                serde_json::json!({"type": "text", "text": system_text}),
            ]))
        } else {
            Some(Value::String(system_parts.join("\n\n")))
        }
    } else if is_anthropic_oauth {
        Some(Value::Array(vec![claude_code_block]))
    } else {
        None
    }
}

fn insert_streaming_options(
    body: &mut serde_json::Map<String, Value>,
    is_oauth: bool,
    stream: bool,
) {
    if is_oauth {
        body.insert("stream".to_owned(), Value::Bool(true));
        body.insert(
            "thinking".to_owned(),
            serde_json::json!({"type": "adaptive"}),
        );
        body.insert(
            "output_config".to_owned(),
            serde_json::json!({"effort": "medium"}),
        );
    } else if stream {
        body.insert("stream".to_owned(), Value::Bool(true));
    }
}

fn convert_to_anthropic_tool(tool: &Value) -> Value {
    match tool.get("function").and_then(|f| f.as_object()) {
        Some(function) => serde_json::json!({
            "name": function.get("name").and_then(|n| n.as_str()).unwrap_or(""),
            "description": function.get("description").and_then(|d| d.as_str()).unwrap_or(""),
            "input_schema": function.get("parameters").cloned().unwrap_or(serde_json::json!({}))
        }),
        None => tool.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::parsing::TransportKind;
    use std::sync::Arc;

    fn make_request(session_id: Option<String>) -> TransportCallRequest {
        TransportCallRequest {
            client: Arc::new(reqwest::Client::new()),
            provider_name: "anthropic".to_owned(),
            model_id: "claude-3-5-sonnet".to_owned(),
            api_base: Some("https://api.anthropic.com/v1".to_owned()),
            messages_payload: vec![serde_json::json!({"role": "user", "content": "hi"})],
            tools_payload: None,
            max_tokens: Some(64),
            stream: false,
            reasoning_effort: None,
            kwargs: serde_json::Map::new(),
            is_anthropic_oauth: false,
            session_id,
        }
    }

    #[test]
    fn test_anthropic_messages_body_maps_session_id_to_metadata_user_id() {
        let req = make_request(Some("sess-42".to_owned()));
        let body = ANTHROPIC_ADAPTER
            .build_request_body(&req, TransportKind::Messages)
            .unwrap();
        assert!(body.get("session_id").is_none());
        assert_eq!(body["metadata"]["user_id"], "sess-42");
    }

    #[test]
    fn test_anthropic_messages_body_omits_metadata_when_session_id_none() {
        let req = make_request(None);
        let body = ANTHROPIC_ADAPTER
            .build_request_body(&req, TransportKind::Messages)
            .unwrap();
        assert!(body.get("metadata").is_none());
        assert!(body.get("session_id").is_none());
    }

    #[test]
    fn test_anthropic_kwargs_metadata_takes_precedence_over_session_id() {
        let mut req = make_request(Some("sess-42".to_owned()));
        req.kwargs.insert(
            "metadata".to_owned(),
            serde_json::json!({"user_id": "from-kwargs"}),
        );
        let body = ANTHROPIC_ADAPTER
            .build_request_body(&req, TransportKind::Messages)
            .unwrap();
        assert_eq!(body["metadata"]["user_id"], "from-kwargs");
    }

    #[test]
    fn test_anthropic_oauth_skips_session_id_metadata() {
        let mut req = make_request(Some("sess-42".to_owned()));
        req.is_anthropic_oauth = true;
        let body = ANTHROPIC_ADAPTER
            .build_request_body(&req, TransportKind::Messages)
            .unwrap();
        assert!(body.get("metadata").is_none());
    }
}
