//! Request building for each transport kind (completion, messages, responses).

use std::sync::Arc;

use reqwest::Client;
use serde_json::Value;

use super::errors::ConduitError;
use super::execution::LLMCore;
use super::provider_runtime::ProviderRuntime;
use super::request_adapters::normalize_responses_kwargs;
use super::tool_calls::{
    normalize_tool_calls, tool_call_arguments_string, tool_call_id, tool_call_name,
};
use crate::clients::parsing::TransportKind;
use crate::providers;

fn convert_message_to_responses_items(message: &Value) -> Vec<Value> {
    let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
    match role {
        "user" | "assistant" => convert_user_or_assistant_items(message, role),
        "tool" => convert_tool_result_item(message).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn convert_user_or_assistant_items(message: &Value, role: &str) -> Vec<Value> {
    let content_item = responses_message_content(message, role)
        .map(|content| serde_json::json!({"role": role, "content": content, "type": "message"}));

    let tool_items = (role == "assistant")
        .then(|| message.get("tool_calls").and_then(|tc| tc.as_array()))
        .flatten()
        .into_iter()
        .flat_map(|calls| {
            normalize_tool_calls(calls)
                .into_iter()
                .enumerate()
                .filter_map(|(index, tc)| tool_call_to_function_call(&tc, index))
        });

    content_item.into_iter().chain(tool_items).collect()
}

fn responses_message_content(message: &Value, role: &str) -> Option<Value> {
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => Some(Value::String(text.clone())),
        Some(Value::Array(parts)) => {
            let normalized: Vec<Value> = parts
                .iter()
                .filter_map(|part| normalize_responses_content_part(part, role))
                .collect();
            (!normalized.is_empty()).then_some(Value::Array(normalized))
        }
        _ => None,
    }
}

fn normalize_responses_content_part(part: &Value, role: &str) -> Option<Value> {
    match part.get("type").and_then(|v| v.as_str()) {
        Some("text") => normalize_responses_text_part(part, role),
        Some("image_url") => normalize_responses_image_url_part(part),
        Some("image_base64") => normalize_responses_base64_image_part(part),
        _ => Some(part.clone()),
    }
}

fn normalize_responses_text_part(part: &Value, role: &str) -> Option<Value> {
    let text = part.get("text").and_then(|v| v.as_str())?;
    let part_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    Some(serde_json::json!({"type": part_type, "text": text}))
}

fn normalize_responses_image_url_part(part: &Value) -> Option<Value> {
    let image_url = part.get("image_url")?;
    let url = image_url
        .as_str()
        .or_else(|| image_url.get("url").and_then(|value| value.as_str()))?;
    Some(serde_json::json!({"type": "input_image", "image_url": url}))
}

fn normalize_responses_base64_image_part(part: &Value) -> Option<Value> {
    let data = part.get("data").and_then(|v| v.as_str())?;
    let mime = part
        .get("mime_type")
        .and_then(|v| v.as_str())
        .unwrap_or("image/jpeg");
    let url = format!("data:{mime};base64,{data}");
    Some(serde_json::json!({"type": "input_image", "image_url": url}))
}

fn tool_call_to_function_call(tc: &Value, index: usize) -> Option<Value> {
    let name = tool_call_name(tc).filter(|n| !n.is_empty())?;
    let call_id = tool_call_id(tc)
        .map(|s| s.to_owned())
        .unwrap_or_else(|| format!("call_{}", index + 1));
    Some(serde_json::json!({
        "type": "function_call",
        "name": name,
        "arguments": tool_call_arguments_string(tc),
        "call_id": call_id,
    }))
}

fn convert_tool_result_item(message: &Value) -> Option<Value> {
    let call_id = message
        .get("tool_call_id")
        .or_else(|| message.get("call_id"))
        .and_then(|v| v.as_str())?;
    let output = message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    Some(serde_json::json!({
        "type": "function_call_output", "call_id": call_id, "output": output,
    }))
}

fn extract_system_instructions(messages: &[Value]) -> Option<String> {
    let joined: String = messages
        .iter()
        .filter(|m| {
            matches!(
                m.get("role").and_then(|r| r.as_str()),
                Some("system" | "developer")
            )
        })
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!joined.is_empty()).then_some(joined)
}

fn convert_single_tool(tool: &Value) -> Value {
    let Some(function) = tool.get("function").and_then(|f| f.as_object()) else {
        return tool.clone();
    };

    let mut entry = serde_json::Map::new();
    entry.insert(
        "type".to_owned(),
        tool.get("type")
            .cloned()
            .unwrap_or(Value::String("function".to_owned())),
    );
    for (key, default) in [
        ("name", None),
        ("description", Some(Value::String(String::new()))),
        ("parameters", Some(serde_json::json!({}))),
        ("strict", None),
    ] {
        if let Some(val) = function.get(key).cloned().or(default) {
            entry.insert(key.to_owned(), val);
        }
    }
    Value::Object(entry)
}

/// All the parameters needed for a single provider call.
#[derive(Debug, Clone)]
pub struct TransportCallRequest {
    pub client: Arc<Client>,
    pub provider_name: String,
    pub model_id: String,
    pub api_base: Option<String>,
    pub messages_payload: Vec<Value>,
    pub tools_payload: Option<Vec<Value>>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
    pub reasoning_effort: Option<Value>,
    pub kwargs: serde_json::Map<String, Value>,
    pub is_anthropic_oauth: bool,
    /// Routing hint forwarded to provider-aware adapters. OpenAI keeps this
    /// local because the public API rejects a top-level `session_id`; Anthropic
    /// maps it to `metadata.user_id`.
    pub session_id: Option<String>,
}

impl LLMCore {
    /// Resolve provider-specific max_tokens kwargs.
    pub fn decide_kwargs_for_provider(
        provider: &str,
        max_tokens: Option<u32>,
        kwargs: &serde_json::Map<String, Value>,
    ) -> serde_json::Map<String, Value> {
        let max_tokens_arg = ProviderRuntime::completion_max_tokens_arg(provider);
        let mut clean = kwargs.clone();
        if clean.contains_key(&max_tokens_arg) {
            return clean;
        }
        if let Some(mt) = max_tokens {
            clean.insert(max_tokens_arg, Value::Number(mt.into()));
        }
        clean
    }

    /// Resolve kwargs for the responses transport format.
    pub fn decide_responses_kwargs(
        max_tokens: Option<u32>,
        kwargs: &serde_json::Map<String, Value>,
        drop_extra_headers: bool,
    ) -> serde_json::Map<String, Value> {
        let mut clean = kwargs.clone();
        if drop_extra_headers {
            clean.remove("extra_headers");
        }
        normalize_responses_kwargs(&mut clean);
        if clean.contains_key("max_output_tokens") || max_tokens.is_none() {
            return clean;
        }
        if let Some(mt) = max_tokens {
            clean.insert("max_output_tokens".to_owned(), Value::Number(mt.into()));
        }
        clean
    }

    pub fn should_default_completion_stream_usage(provider_name: &str) -> bool {
        ProviderRuntime::should_include_completion_stream_usage(provider_name)
    }

    /// Inject `stream_options` for providers that support usage reporting.
    pub fn with_default_completion_stream_options(
        provider_name: &str,
        stream: bool,
        kwargs: &serde_json::Map<String, Value>,
    ) -> serde_json::Map<String, Value> {
        let should_add = stream
            && Self::should_default_completion_stream_usage(provider_name)
            && !kwargs.contains_key("stream_options");

        let mut result = kwargs.clone();
        if should_add {
            result.insert(
                "stream_options".to_owned(),
                serde_json::json!({"include_usage": true}),
            );
        }
        result
    }

    /// Inject reasoning effort into kwargs when applicable.
    pub fn with_responses_reasoning(
        kwargs: &serde_json::Map<String, Value>,
        reasoning_effort: Option<&Value>,
    ) -> serde_json::Map<String, Value> {
        let should_add =
            reasoning_effort.is_some_and(|e| !e.is_null()) && !kwargs.contains_key("reasoning");

        let mut result = kwargs.clone();
        if should_add {
            result.insert(
                "reasoning".to_owned(),
                serde_json::json!({"effort": reasoning_effort.expect("SAFETY: checked above")}),
            );
        }
        result
    }

    /// Re-key completion tool schemas into responses format.
    pub fn convert_tools_for_responses(tools_payload: Option<&[Value]>) -> Option<Vec<Value>> {
        let tools = tools_payload.filter(|t| !t.is_empty())?;
        Some(tools.iter().map(convert_single_tool).collect())
    }

    /// Determine which transport to use for a given request.
    pub fn selected_transport(
        &self,
        provider_name: &str,
        model_id: &str,
        tools_payload: Option<&[Value]>,
        supports_responses: bool,
        preferred_transport: Option<TransportKind>,
    ) -> Result<TransportKind, ConduitError> {
        ProviderRuntime::new(provider_name, model_id, None, None, self.api_format())
            .selected_transport(tools_payload, supports_responses, preferred_transport)
    }

    /// Split messages into `(instructions, input_items)` for the responses format.
    pub fn split_messages_for_responses(messages: &[Value]) -> (Option<String>, Vec<Value>) {
        let instructions = extract_system_instructions(messages);
        let input_items = messages
            .iter()
            .filter(|m| {
                !matches!(
                    m.get("role").and_then(|r| r.as_str()),
                    Some("system" | "developer")
                )
            })
            .flat_map(convert_message_to_responses_items)
            .collect();
        (instructions, input_items)
    }

    /// Build the request URL for a given provider and transport.
    pub fn build_request_url(api_base: &str, transport: TransportKind) -> String {
        providers::adapter_for_transport(transport).build_request_url(api_base, transport)
    }

    /// Build the JSON body for a completion-format request.
    pub fn build_completion_body(
        request: &TransportCallRequest,
        provider_name: &str,
    ) -> Result<Value, ConduitError> {
        let mut adapter_request = request.clone();
        adapter_request.provider_name = provider_name.to_owned();
        providers::adapter_for_transport(TransportKind::Completion)
            .build_request_body(&adapter_request, TransportKind::Completion)
    }

    /// Build the JSON body for an Anthropic Messages-format request.
    pub fn build_messages_body(request: &TransportCallRequest) -> Result<Value, ConduitError> {
        providers::adapter_for_transport(TransportKind::Messages)
            .build_request_body(request, TransportKind::Messages)
    }

    /// Build the JSON body for a responses-format request.
    pub fn build_responses_body(request: &TransportCallRequest) -> Result<Value, ConduitError> {
        providers::adapter_for_transport(TransportKind::Responses)
            .build_request_body(request, TransportKind::Responses)
    }
}
