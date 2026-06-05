//! Message normalization, orphan pruning, and message rule enforcement.

use std::collections::HashSet;

use serde_json::Value;

use super::tool_calls::normalize_message_tool_calls;
use crate::clients::parsing::TransportKind;

/// Normalize messages to ensure protocol compliance before sending to any LLM API.
///
/// - Removes orphan tool_use blocks (no matching tool_result follows)
/// - Removes orphan tool_result messages (no matching tool_use precedes)
pub fn normalize_messages_for_api(messages: Vec<Value>, transport: TransportKind) -> Vec<Value> {
    let normalized_messages: Vec<Value> = messages
        .into_iter()
        .map(|message| normalize_message_tool_calls(&message))
        .collect();
    let mut result = prune_orphan_tool_messages(normalized_messages);

    normalize_image_content_blocks(&mut result, transport);

    // Why: role merging deferred to `build_messages_body` — earlier merging drops call IDs.
    result
}

fn normalize_image_content_blocks(messages: &mut [Value], transport: TransportKind) {
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for block in content.iter_mut() {
            if block.get("type").and_then(|t| t.as_str()) != Some("image_base64") {
                continue;
            }
            let mime = block
                .get("mime_type")
                .and_then(|v| v.as_str())
                .unwrap_or("image/jpeg");
            let data = block.get("data").and_then(|v| v.as_str()).unwrap_or("");
            *block = match transport {
                TransportKind::Messages => {
                    serde_json::json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": mime,
                            "data": data,
                        }
                    })
                }
                TransportKind::Completion | TransportKind::Responses => {
                    serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{mime};base64,{data}"),
                        }
                    })
                }
            };
        }
    }
}

pub(crate) fn prune_orphan_tool_messages(messages: Vec<Value>) -> Vec<Value> {
    let tool_call_ids = collect_tool_call_ids(&messages);
    let tool_result_ids = collect_tool_result_ids(&messages);

    messages
        .into_iter()
        .filter_map(|msg| prune_single_message(msg, &tool_call_ids, &tool_result_ids))
        .collect()
}

fn collect_tool_call_ids(messages: &[Value]) -> HashSet<String> {
    messages
        .iter()
        .filter_map(|msg| msg.get("tool_calls").and_then(|c| c.as_array()))
        .flatten()
        .filter_map(|call| call.get("id").and_then(|v| v.as_str()))
        .map(ToOwned::to_owned)
        .collect()
}

fn collect_tool_result_ids(messages: &[Value]) -> HashSet<String> {
    messages
        .iter()
        .filter(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .filter_map(|msg| msg.get("tool_call_id").and_then(|v| v.as_str()))
        .map(ToOwned::to_owned)
        .collect()
}

fn prune_single_message(
    msg: Value,
    tool_call_ids: &HashSet<String>,
    tool_result_ids: &HashSet<String>,
) -> Option<Value> {
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

    if role == "tool" {
        let call_id = msg
            .get("tool_call_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return if !call_id.is_empty() && tool_call_ids.contains(call_id) {
            Some(msg)
        } else {
            None
        };
    }

    if role == "assistant" && msg.get("tool_calls").and_then(|c| c.as_array()).is_some() {
        return prune_assistant_tool_calls(msg, tool_result_ids);
    }

    Some(msg)
}

fn prune_assistant_tool_calls(mut msg: Value, tool_result_ids: &HashSet<String>) -> Option<Value> {
    let Some(obj) = msg.as_object_mut() else {
        return Some(msg);
    };

    let calls = obj
        .get("tool_calls")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let valid_calls: Vec<Value> = calls
        .into_iter()
        .filter(|call| {
            call.get("id")
                .and_then(|v| v.as_str())
                .is_some_and(|id| tool_result_ids.contains(id))
        })
        .collect();

    let has_text = obj.get("content").is_some_and(|c| {
        c.as_str().is_some_and(|s| !s.is_empty()) || c.as_array().is_some_and(|a| !a.is_empty())
    });

    if valid_calls.is_empty() && !has_text {
        return None;
    } else if valid_calls.is_empty() {
        obj.remove("tool_calls");
    } else {
        obj.insert("tool_calls".to_owned(), Value::Array(valid_calls));
    }

    Some(msg)
}

/// Enforce Anthropic-specific message ordering rules.
///
/// - Merges consecutive same-role messages (except system).
/// - Inserts a synthetic "user" message at the start if needed.
/// - Appends a synthetic "user" message at the end if the last message is "assistant".
#[cfg(test)]
pub(crate) fn enforce_anthropic_message_rules(messages: Vec<Value>) -> Vec<Value> {
    if messages.is_empty() {
        return messages;
    }

    let mut result: Vec<Value> = Vec::new();

    for msg in messages {
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_owned();

        // Skip system messages in this pass (they go separately in Anthropic API)
        if role == "system" {
            result.push(msg);
            continue;
        }

        // Merge consecutive same-role messages
        if let Some(last) = result.last() {
            let last_role = last.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if last_role == role && role != "system" {
                // Merge: append content to previous message
                let prev_content = last.get("content").and_then(|c| c.as_str()).unwrap_or("");
                let new_content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if !new_content.is_empty() {
                    let merged = format!("{prev_content}\n\n{new_content}");
                    if let Some(last_mut) = result.last_mut()
                        && let Some(obj) = last_mut.as_object_mut()
                    {
                        obj.insert("content".to_owned(), Value::String(merged));
                    }
                }
                continue;
            }
        }

        result.push(msg);
    }

    // Ensure first non-system message is "user"
    let first_non_system = result
        .iter()
        .position(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"));
    if let Some(idx) = first_non_system
        && result[idx].get("role").and_then(|r| r.as_str()) != Some("user")
    {
        result.insert(
            idx,
            serde_json::json!({"role": "user", "content": "Continue."}),
        );
    }

    // Ensure last message is "user" (but NOT if it ends with tool results, which is valid)
    if let Some(last) = result.last() {
        let last_role = last.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if last_role == "assistant" {
            result.push(serde_json::json!({"role": "user", "content": "Continue."}));
        }
    }

    result
}

#[cfg(test)]
mod image_norm_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_format() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "What is this?"},
                {"type": "image_base64", "mime_type": "image/png", "data": "iVBOR"}
            ]
        })];
        normalize_image_content_blocks(&mut msgs, TransportKind::Messages);

        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "iVBOR");
    }

    #[test]
    fn openai_format() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "describe"},
                {"type": "image_base64", "mime_type": "image/jpeg", "data": "/9j/4A"}
            ]
        })];
        normalize_image_content_blocks(&mut msgs, TransportKind::Completion);

        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/jpeg;base64,/9j/4A"
        );
    }

    #[test]
    fn skips_non_user_messages() {
        let mut msgs = vec![json!({
            "role": "assistant",
            "content": [
                {"type": "image_base64", "mime_type": "image/png", "data": "abc"}
            ]
        })];
        normalize_image_content_blocks(&mut msgs, TransportKind::Messages);
        assert_eq!(msgs[0]["content"][0]["type"], "image_base64");
    }

    #[test]
    fn skips_string_content() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": "just text"
        })];
        normalize_image_content_blocks(&mut msgs, TransportKind::Messages);
        assert_eq!(msgs[0]["content"], "just text");
    }

    #[test]
    fn multiple_images_in_single_message() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "compare these"},
                {"type": "image_base64", "mime_type": "image/png", "data": "AAA"},
                {"type": "image_base64", "mime_type": "image/jpeg", "data": "BBB"}
            ]
        })];
        normalize_image_content_blocks(&mut msgs, TransportKind::Messages);

        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[2]["source"]["media_type"], "image/jpeg");
    }

    #[test]
    fn image_only_no_text() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "image_base64", "mime_type": "image/png", "data": "ONLY"}
            ]
        })];
        normalize_image_content_blocks(&mut msgs, TransportKind::Messages);

        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["data"], "ONLY");
    }

    #[test]
    fn missing_mime_type_defaults_to_jpeg() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "image_base64", "data": "NOMINE"}
            ]
        })];
        normalize_image_content_blocks(&mut msgs, TransportKind::Messages);

        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["source"]["media_type"], "image/jpeg");
    }

    #[test]
    fn missing_data_defaults_to_empty() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "image_base64", "mime_type": "image/png"}
            ]
        })];
        normalize_image_content_blocks(&mut msgs, TransportKind::Messages);

        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["source"]["data"], "");
    }

    #[test]
    fn responses_transport_uses_openai_format() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "image_base64", "mime_type": "image/webp", "data": "WEBP"}
            ]
        })];
        normalize_image_content_blocks(&mut msgs, TransportKind::Responses);

        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(
            content[0]["image_url"]["url"],
            "data:image/webp;base64,WEBP"
        );
    }

    #[test]
    fn leaves_non_image_blocks_untouched() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image_base64", "mime_type": "image/png", "data": "IMG"},
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]
        })];
        normalize_image_content_blocks(&mut msgs, TransportKind::Messages);

        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[2]["type"], "tool_result");
    }

    #[test]
    fn multiple_user_messages_all_normalized() {
        let mut msgs = vec![
            json!({
                "role": "user",
                "content": [
                    {"type": "image_base64", "mime_type": "image/png", "data": "A"}
                ]
            }),
            json!({
                "role": "assistant",
                "content": "I see an image"
            }),
            json!({
                "role": "user",
                "content": [
                    {"type": "image_base64", "mime_type": "image/jpeg", "data": "B"}
                ]
            }),
        ];
        normalize_image_content_blocks(&mut msgs, TransportKind::Completion);

        assert_eq!(msgs[0]["content"][0]["type"], "image_url");
        assert_eq!(msgs[1]["content"], "I see an image");
        assert_eq!(msgs[2]["content"][0]["type"], "image_url");
    }

    #[test]
    fn image_survives_full_normalize_pipeline() {
        let msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "look"},
                {"type": "image_base64", "mime_type": "image/png", "data": "XYZ"}
            ]
        })];
        let normalized = normalize_messages_for_api(msgs, TransportKind::Messages);

        let content = normalized[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["data"], "XYZ");
    }
}
