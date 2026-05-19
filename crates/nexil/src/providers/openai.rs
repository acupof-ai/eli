use serde_json::{Map, Value};

use crate::adapter::ProviderAdapter;
use crate::clients::parsing::TransportKind;
use crate::core::errors::{ConduitError, ErrorKind};
use crate::core::execution::LLMCore;
use crate::core::request_builder::TransportCallRequest;

pub static OPENAI_ADAPTER: OpenAIAdapter = OpenAIAdapter;

pub struct OpenAIAdapter;

impl ProviderAdapter for OpenAIAdapter {
    fn build_request_url(&self, api_base: &str, transport: TransportKind) -> String {
        let base = api_base.trim_end_matches('/');
        match transport {
            TransportKind::Completion => format!("{}/chat/completions", base),
            TransportKind::Responses => format!("{}/responses", base),
            TransportKind::Messages => format!("{}/messages", base),
        }
    }

    fn build_request_body(
        &self,
        request: &TransportCallRequest,
        transport: TransportKind,
    ) -> Result<Value, ConduitError> {
        match transport {
            TransportKind::Completion => Ok(self.build_completion_body(request)),
            TransportKind::Responses => Ok(self.build_responses_body(request)),
            TransportKind::Messages => Err(ConduitError::new(
                ErrorKind::Config,
                "openai adapter does not support messages transport",
            )),
        }
    }
}

impl OpenAIAdapter {
    fn build_completion_body(&self, request: &TransportCallRequest) -> Value {
        let mut body = base_completion_body(request);
        insert_completion_options(&mut body, request);
        insert_openai_kwargs(&mut body, completion_kwargs(request));
        Value::Object(body)
    }

    fn build_responses_body(&self, request: &TransportCallRequest) -> Value {
        let mut body = base_responses_body(request);
        insert_responses_options(&mut body, request);
        insert_openai_kwargs(&mut body, responses_kwargs(request));
        apply_chatgpt_backend_options(&mut body, request);
        Value::Object(body)
    }
}

fn base_completion_body(request: &TransportCallRequest) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(request.model_id.clone()));
    body.insert(
        "messages".to_owned(),
        Value::Array(request.messages_payload.clone()),
    );
    body
}

fn insert_completion_options(body: &mut Map<String, Value>, request: &TransportCallRequest) {
    if request.stream {
        body.insert("stream".to_owned(), Value::Bool(true));
    }
    if let Some(ref tools) = request.tools_payload
        && !tools.is_empty()
    {
        body.insert("tools".to_owned(), Value::Array(tools.clone()));
    }
    insert_reasoning_effort(body, &request.reasoning_effort);
}

fn insert_reasoning_effort(body: &mut Map<String, Value>, effort: &Option<Value>) {
    if let Some(effort) = effort
        && !effort.is_null()
    {
        body.insert("reasoning_effort".to_owned(), effort.clone());
    }
}

fn completion_kwargs(request: &TransportCallRequest) -> Map<String, Value> {
    let kwargs = LLMCore::decide_kwargs_for_provider(
        &request.provider_name,
        request.max_tokens,
        &request.kwargs,
    );
    LLMCore::with_default_completion_stream_options(&request.provider_name, request.stream, &kwargs)
}

fn base_responses_body(request: &TransportCallRequest) -> Map<String, Value> {
    let (instructions, input_items) =
        LLMCore::split_messages_for_responses(&request.messages_payload);
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(request.model_id.clone()));
    body.insert("input".to_owned(), Value::Array(input_items));
    body.insert(
        "instructions".to_owned(),
        Value::String(instructions.unwrap_or_default()),
    );
    body
}

fn insert_responses_options(body: &mut Map<String, Value>, request: &TransportCallRequest) {
    if request.stream {
        body.insert("stream".to_owned(), Value::Bool(true));
    }
    if let Some(ref tools) = request.tools_payload
        && let Some(converted) = LLMCore::convert_tools_for_responses(Some(tools))
    {
        body.insert("tools".to_owned(), Value::Array(converted));
    }
}

fn responses_kwargs(request: &TransportCallRequest) -> Map<String, Value> {
    let effective_reasoning = request
        .reasoning_effort
        .clone()
        .or_else(|| default_gpt5_reasoning(&request.model_id));
    let kwargs = LLMCore::with_responses_reasoning(&request.kwargs, effective_reasoning.as_ref());
    LLMCore::decide_responses_kwargs(request.max_tokens, &kwargs, true)
}

fn default_gpt5_reasoning(model_id: &str) -> Option<Value> {
    model_id
        .starts_with("gpt-5")
        .then(|| Value::String("low".to_owned()))
}

fn insert_openai_kwargs(body: &mut Map<String, Value>, kwargs: Map<String, Value>) {
    for (key, value) in kwargs {
        if key != "session_id" {
            body.entry(key).or_insert(value);
        }
    }
}

fn apply_chatgpt_backend_options(body: &mut Map<String, Value>, request: &TransportCallRequest) {
    if request
        .api_base
        .as_deref()
        .is_some_and(|b| b.contains("chatgpt.com"))
    {
        body.insert("store".to_owned(), Value::Bool(false));
        body.insert("stream".to_owned(), Value::Bool(true));
        body.remove("max_output_tokens");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_request(session_id: Option<String>) -> TransportCallRequest {
        TransportCallRequest {
            client: Arc::new(reqwest::Client::new()),
            provider_name: "openai".to_owned(),
            model_id: "gpt-4o".to_owned(),
            api_base: Some("https://api.openai.com/v1".to_owned()),
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
    fn test_openai_completion_body_omits_session_id_when_set() {
        let req = make_request(Some("sess-42".to_owned()));
        let body = OPENAI_ADAPTER.build_completion_body(&req);
        assert!(body.get("session_id").is_none());
    }

    #[test]
    fn test_openai_completion_body_omits_session_id_when_none() {
        let req = make_request(None);
        let body = OPENAI_ADAPTER.build_completion_body(&req);
        assert!(body.get("session_id").is_none());
    }

    #[test]
    fn test_openai_responses_body_omits_session_id_when_set() {
        let req = make_request(Some("sess-42".to_owned()));
        let body = OPENAI_ADAPTER.build_responses_body(&req);
        assert!(body.get("session_id").is_none());
    }

    #[test]
    fn test_openai_kwargs_session_id_is_filtered() {
        let mut req = make_request(Some("sess-42".to_owned()));
        req.kwargs.insert(
            "session_id".to_owned(),
            Value::String("from-kwargs".to_owned()),
        );
        let body = OPENAI_ADAPTER.build_completion_body(&req);
        assert!(body.get("session_id").is_none());
    }
}
