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

        let (system_parts, mut messages) =
            anthropic_messages::split_system_and_conversation(&request.messages_payload);

        if let Some(system_val) = build_system_value(
            &system_parts,
            request.is_anthropic_oauth,
            request.prompt_cache,
        ) {
            body.insert("system".to_owned(), system_val);
        }

        // Rolling history cache breakpoint: anchor the *second-to-last*
        // conversation message. The last message is treated as volatile — in the
        // agentic tool loop it carries the ephemeral tail reminder (merged into the
        // trailing user message on the first round, pushed as a fresh user message
        // afterwards) or the current turn's not-yet-persisted content — so caching
        // up to the message *before* it keeps the cached prefix byte-stable
        // turn-to-turn (a mis-placed breakpoint on volatile content would self-
        // invalidate: 1.25x write, 0 read). system + last tool already use 2 of
        // Anthropic's 4 breakpoints; this is the 3rd. Below the model's
        // min-cacheable size Anthropic silently ignores it; with <2 messages there
        // is nothing stable to anchor yet.
        if request.prompt_cache && messages.len() >= 2 {
            let anchor = messages.len() - 2;
            mark_message_block_cached(&mut messages[anchor]);
        }

        body.insert("messages".to_owned(), Value::Array(messages));
        insert_streaming_options(&mut body, request.is_anthropic_oauth, request.stream);

        if let Some(ref tools) = request.tools_payload
            && !tools.is_empty()
        {
            let mut anthropic_tools: Vec<Value> =
                tools.iter().map(convert_to_anthropic_tool).collect();
            // Cache the (stable) tool-schema block: marking the last tool caches
            // the whole tools array as a prefix, so repeat turns re-read it cheaply.
            if request.prompt_cache
                && let Some(last) = anthropic_tools.last_mut()
            {
                mark_cached(last);
            }
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

/// Attach an ephemeral `cache_control` marker to a content block / tool object,
/// making it (and everything before it) a cacheable prefix for Anthropic.
fn mark_cached(block: &mut Value) {
    if let Value::Object(map) = block {
        map.insert(
            "cache_control".to_owned(),
            serde_json::json!({"type": "ephemeral"}),
        );
    }
}

/// Mark a whole conversation message as a cache anchor. `cache_control` attaches
/// to a content *block*, so for block-array content we mark the final block, and
/// for string content we lower it to a single text block carrying the marker.
/// Anthropic treats `"s"` and `[{"type":"text","text":"s"}]` as equivalent for
/// prefix matching, so the same message stays cache-stable when it later appears
/// unmarked (string form) deeper in the prefix. Empty content is left untouched.
fn mark_message_block_cached(message: &mut Value) {
    let Value::Object(map) = message else { return };
    match map.get_mut("content") {
        Some(Value::Array(blocks)) => {
            if let Some(last) = blocks.last_mut() {
                mark_cached(last);
            }
        }
        Some(content @ Value::String(_)) => {
            let text = content.as_str().unwrap_or_default().to_owned();
            if text.is_empty() {
                return;
            }
            *content = serde_json::json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        _ => {}
    }
}

/// Build the Anthropic `system` value from the collected system parts.
///
/// When `cache` is set, the system is emitted in block-array form with a
/// `cache_control` marker on the final block so the stable system prefix is
/// cached. Without caching, a plain non-OAuth system stays in compact string
/// form (unchanged wire shape).
fn build_system_value(
    system_parts: &[String],
    is_anthropic_oauth: bool,
    cache: bool,
) -> Option<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    if is_anthropic_oauth {
        blocks.push(serde_json::json!({"type": "text", "text": CLAUDE_CODE_SYSTEM}));
    }
    if !system_parts.is_empty() {
        blocks.push(serde_json::json!({"type": "text", "text": system_parts.join("\n\n")}));
    }
    if blocks.is_empty() {
        return None;
    }
    // Compact string form for the simple non-cached, non-OAuth case.
    if !cache && !is_anthropic_oauth {
        return Some(Value::String(system_parts.join("\n\n")));
    }
    if cache && let Some(last) = blocks.last_mut() {
        mark_cached(last);
    }
    Some(Value::Array(blocks))
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
            prompt_cache: false,
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

    fn cache_request(prompt_cache: bool) -> TransportCallRequest {
        let mut req = make_request(None);
        req.prompt_cache = prompt_cache;
        req.messages_payload = vec![
            serde_json::json!({"role": "system", "content": "stable system rules"}),
            serde_json::json!({"role": "user", "content": "hi"}),
        ];
        req.tools_payload = Some(vec![
            serde_json::json!({"function": {"name": "a", "description": "tool a", "parameters": {}}}),
            serde_json::json!({"function": {"name": "b", "description": "tool b", "parameters": {}}}),
        ]);
        req
    }

    #[test]
    fn prompt_cache_marks_last_system_block_and_last_tool() {
        let body = ANTHROPIC_ADAPTER
            .build_request_body(&cache_request(true), TransportKind::Messages)
            .unwrap();
        // System is emitted as a block array with cache_control on the final block.
        let system = body["system"]
            .as_array()
            .expect("cached system should be a block array");
        assert!(
            system.last().unwrap().get("cache_control").is_some(),
            "last system block must carry cache_control"
        );
        // Only the LAST tool gets the breakpoint, which caches the whole prefix.
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools[0].get("cache_control").is_none());
        assert!(tools[1].get("cache_control").is_some());
    }

    #[test]
    fn no_prompt_cache_emits_no_cache_control() {
        let body = ANTHROPIC_ADAPTER
            .build_request_body(&cache_request(false), TransportKind::Messages)
            .unwrap();
        // Without caching the system stays a compact string and tools are bare.
        assert!(body["system"].is_string());
        let tools = body["tools"].as_array().unwrap();
        assert!(tools.iter().all(|t| t.get("cache_control").is_none()));
    }

    /// A multi-message conversation: system + 3 conversation turns. After the
    /// system split, the conversation is [user, assistant, user]; the rolling
    /// history breakpoint must land on the second-to-last (the assistant), never
    /// the last (volatile) message.
    fn history_request(prompt_cache: bool) -> TransportCallRequest {
        let mut req = make_request(None);
        req.prompt_cache = prompt_cache;
        req.messages_payload = vec![
            serde_json::json!({"role": "system", "content": "stable system rules"}),
            serde_json::json!({"role": "user", "content": "first question"}),
            serde_json::json!({"role": "assistant", "content": "stable prior answer"}),
            serde_json::json!({"role": "user", "content": "current volatile turn"}),
        ];
        req
    }

    #[test]
    fn prompt_cache_marks_second_to_last_conversation_message() {
        let body = ANTHROPIC_ADAPTER
            .build_request_body(&history_request(true), TransportKind::Messages)
            .unwrap();
        let messages = body["messages"].as_array().unwrap();
        // Conversation is [user, assistant, user] (system split out; the adapter
        // always emits block-array content). The anchor is the assistant at index
        // 1; the marker lands on its final block.
        let anchor_blocks = messages[1]["content"].as_array().unwrap();
        assert!(
            anchor_blocks.last().unwrap().get("cache_control").is_some(),
            "second-to-last message must carry the history cache breakpoint"
        );
        // The last (volatile) message must NOT be cached — that would self-invalidate.
        assert!(
            blocks_uncached(&messages[2]),
            "last message stays volatile and uncached"
        );
    }

    /// True if no content block of `msg` carries a `cache_control` marker.
    fn blocks_uncached(msg: &Value) -> bool {
        msg["content"]
            .as_array()
            .is_none_or(|blocks| blocks.iter().all(|b| b.get("cache_control").is_none()))
    }

    #[test]
    fn prompt_cache_marks_last_block_of_array_content_message() {
        let mut req = history_request(true);
        // Make the anchor (second-to-last) already block-form; the marker should
        // land on its final block, not convert anything.
        req.messages_payload[2] = serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b"},
            ],
        });
        let body = ANTHROPIC_ADAPTER
            .build_request_body(&req, TransportKind::Messages)
            .unwrap();
        let blocks = body["messages"][1]["content"].as_array().unwrap();
        assert!(blocks[0].get("cache_control").is_none());
        assert!(blocks[1].get("cache_control").is_some());
    }

    #[test]
    fn no_prompt_cache_leaves_history_unmarked() {
        let body = ANTHROPIC_ADAPTER
            .build_request_body(&history_request(false), TransportKind::Messages)
            .unwrap();
        let messages = body["messages"].as_array().unwrap();
        // With caching off, no message carries a cache_control marker.
        assert!(messages.iter().all(blocks_uncached));
    }

    #[test]
    fn prompt_cache_single_message_has_no_history_breakpoint() {
        // cache_request has only one conversation message ([user "hi"]); the
        // history breakpoint needs >=2, so it must be a no-op there.
        let body = ANTHROPIC_ADAPTER
            .build_request_body(&cache_request(true), TransportKind::Messages)
            .unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert!(blocks_uncached(&messages[0]));
    }
}
