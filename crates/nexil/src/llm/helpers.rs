//! Free helper functions for message building, response parsing, and tape
//! entry conversion.

use serde_json::Value;

use crate::core::errors::{ConduitError, ErrorKind};
use crate::core::tool_calls::{
    normalize_message_tool_calls, normalize_tool_calls, parse_dsml_tool_calls,
};
use crate::tape::AnchorSelector;
use crate::tape::entries::{TapeEntry, TapeEntryKind};

// ---------------------------------------------------------------------------
// Message building
// ---------------------------------------------------------------------------

pub(super) fn build_messages(
    prompt: Option<&str>,
    user_content: Option<&[Value]>,
    system_prompt: Option<&str>,
    messages: Option<&[Value]>,
) -> Vec<Value> {
    let mut msgs = Vec::new();
    if let Some(sys) = system_prompt {
        msgs.push(serde_json::json!({"role": "system", "content": sys}));
    }
    if let Some(existing) = messages {
        msgs.extend_from_slice(existing);
    }
    if let Some(parts) = user_content {
        msgs.push(serde_json::json!({"role": "user", "content": parts}));
    } else if let Some(p) = prompt {
        msgs.push(serde_json::json!({"role": "user", "content": p}));
    }
    msgs
}

pub(super) fn prepend_tape_history(msgs: &mut Vec<Value>, tape_messages: Vec<Value>) {
    if tape_messages.is_empty() {
        return;
    }
    let system_count = msgs
        .iter()
        .take_while(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
        .count();
    let mut combined = msgs[..system_count].to_vec();
    combined.extend(tape_messages);
    combined.extend_from_slice(&msgs[system_count..]);
    *msgs = combined;
}

/// Restore the original multimodal content on the last user message in context.
/// This ensures the LLM sees images on the current turn even though tape has
/// placeholders. On the next turn, user_content is None and tape's placeholders
/// are used.
pub(super) fn restore_last_user_content(msgs: &mut [Value], user_content: &[Value]) {
    if let Some(msg) = msgs
        .iter_mut()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    {
        msg["content"] = Value::Array(user_content.to_vec());
    }
}

// ---------------------------------------------------------------------------
// Tape entry conversion
// ---------------------------------------------------------------------------

pub(super) fn slice_entries_by_anchor(
    entries: &[TapeEntry],
    anchor: &AnchorSelector,
) -> Vec<TapeEntry> {
    let anchor_pos = match anchor {
        AnchorSelector::None => return entries.to_vec(),
        AnchorSelector::LastAnchor => entries
            .iter()
            .rposition(|e| e.kind == TapeEntryKind::Anchor),
        AnchorSelector::Named(name) => entries.iter().rposition(|e| {
            e.kind == TapeEntryKind::Anchor
                && e.payload.get("name").and_then(|v| v.as_str()) == Some(name.as_str())
        }),
    };
    match anchor_pos {
        Some(idx) => entries[idx + 1..].to_vec(),
        // Bug E: anchor not found — log an error so operators can detect stale
        // handoff state (e.g. tape was trimmed after the anchor was written),
        // then fall back to the full history so the user still gets context.
        None => {
            match anchor {
                AnchorSelector::Named(name) => tracing::error!(
                    anchor = %name,
                    "tape: named anchor not found, falling back to full history"
                ),
                AnchorSelector::LastAnchor => {
                    tracing::error!("tape: no anchor found in tape, falling back to full history")
                }
                AnchorSelector::None => unreachable!(),
            }
            entries.to_vec()
        }
    }
}

pub(super) fn build_full_context_from_entries(entries: &[TapeEntry]) -> Vec<Value> {
    let messages: Vec<Value> = entries.iter().flat_map(entry_to_messages).collect();
    dedup_system_messages(messages)
}

fn entry_to_messages(entry: &TapeEntry) -> Vec<Value> {
    match entry.kind {
        TapeEntryKind::Message if entry.payload.is_object() => {
            vec![normalize_message_tool_calls(&entry.payload)]
        }
        TapeEntryKind::System => entry
            .payload
            .get("content")
            .and_then(|c| c.as_str())
            .map(|content| vec![serde_json::json!({"role": "system", "content": content})])
            .unwrap_or_default(),
        TapeEntryKind::ToolCall => entry
            .payload
            .get("calls")
            .and_then(|c| c.as_array())
            .map(|calls| normalize_tool_calls(calls))
            .filter(|nc| !nc.is_empty())
            .map(|normalized_calls| {
                let content = entry.payload.get("content").cloned().unwrap_or(Value::Null);
                let mut message = serde_json::json!({
                    "role": "assistant",
                    "content": content,
                    "tool_calls": normalized_calls
                });
                if let Some(reasoning) = entry.payload.get("reasoning_content") {
                    message["reasoning_content"] = reasoning.clone();
                }
                vec![message]
            })
            .unwrap_or_default(),
        TapeEntryKind::ToolResult => entry
            .payload
            .get("results")
            .and_then(|r| r.as_array())
            .map(|results| results.iter().map(tool_result_to_message).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn tool_result_to_message(result: &Value) -> Value {
    let tool_call_id = result
        .get("call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let content = result
        .get("output")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        })
        .unwrap_or_default();
    serde_json::json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": content
    })
}

fn dedup_system_messages(messages: Vec<Value>) -> Vec<Value> {
    let last_system_idx = messages
        .iter()
        .rposition(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("system"));

    match last_system_idx {
        Some(last_idx) => messages
            .into_iter()
            .enumerate()
            .filter(|(i, msg)| {
                msg.get("role").and_then(|r| r.as_str()) != Some("system") || *i == last_idx
            })
            .map(|(_, msg)| msg)
            .collect(),
        None => messages,
    }
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

pub(super) fn extract_content(response: &Value) -> Result<String, ConduitError> {
    extract_completion_content(response)
        .or_else(|| extract_anthropic_content(response))
        .or_else(|| extract_responses_content(response))
        .ok_or_else(|| {
            // Truncate to avoid flooding logs with large payloads
            let snippet = {
                let s = response.to_string();
                if s.len() > 500 {
                    format!("{}…", &s[..500])
                } else {
                    s
                }
            };
            tracing::warn!(response = %snippet, "extract_content: no content found in response");
            ConduitError::new(ErrorKind::Provider, "Response missing content")
        })
}

fn extract_completion_content(response: &Value) -> Option<String> {
    let content = response
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?;
    // OpenAI returns `"content": null` when the response has only tool calls.
    // Treat null as empty string so we don't fall through to "Response missing content".
    if content.is_null() {
        return Some(String::new());
    }
    content.as_str().map(str::to_owned)
}

fn extract_anthropic_content(response: &Value) -> Option<String> {
    if response.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return None;
    }
    let content_arr = response.get("content")?.as_array()?;
    let text: String = content_arr
        .iter()
        .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
        .collect();
    // An empty content array or all-tool_use blocks is valid — return empty string
    // so we don't fall through to "Response missing content".
    Some(text)
}

fn extract_responses_content(response: &Value) -> Option<String> {
    let output = response.get("output")?.as_array()?;
    // Responses API can return an empty output array when the model completes
    // but produces no content (e.g., content filter). Treat as empty string.
    if output.is_empty() {
        tracing::warn!("Responses API returned empty output array — possible content filter");
        return Some(String::new());
    }
    output
        .iter()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("message"))
        .and_then(|item| {
            item.get("content")?
                .get(0)?
                .get("text")?
                .as_str()
                .map(str::to_owned)
        })
}

pub(super) fn extract_tool_calls(response: &Value) -> Result<Vec<Value>, ConduitError> {
    if let Some(calls) = extract_completion_tool_calls(response) {
        return Ok(calls);
    }
    if let Some(calls) = extract_typed_blocks(response.get("content"), "tool_use") {
        return Ok(normalize_tool_calls(&calls));
    }
    if let Some(calls) = extract_typed_blocks(response.get("output"), "function_call") {
        return Ok(normalize_tool_calls(&calls));
    }
    Ok(Vec::new())
}

fn extract_completion_tool_calls(response: &Value) -> Option<Vec<Value>> {
    let message = completion_message(response)?;
    if let Some(calls) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
        return Some(normalize_tool_calls(calls));
    }
    let calls = parse_dsml_tool_calls(message.get("content")?.as_str()?);
    (!calls.is_empty()).then_some(calls)
}

fn completion_message(response: &Value) -> Option<&Value> {
    response
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
}

fn extract_typed_blocks(field: Option<&Value>, type_name: &str) -> Option<Vec<Value>> {
    let arr = field?.as_array()?;
    let calls: Vec<Value> = arr
        .iter()
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some(type_name))
        .cloned()
        .collect();
    if calls.is_empty() { None } else { Some(calls) }
}

// ---------------------------------------------------------------------------
// Assistant message construction
// ---------------------------------------------------------------------------

pub(super) fn build_assistant_tool_call_message(response: &Value) -> Value {
    if let Some(msg) = response
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
    {
        return normalize_message_tool_calls(msg);
    }

    if let Some(content) = response.get("content").and_then(|c| c.as_array()) {
        return build_anthropic_assistant_message(content);
    }

    if let Some(output) = response.get("output").and_then(|o| o.as_array()) {
        return build_responses_assistant_message(output);
    }

    serde_json::json!({"role": "assistant", "content": null})
}

fn build_anthropic_assistant_message(content: &[Value]) -> Value {
    let tool_calls: Vec<Value> = content
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .map(|block| {
            serde_json::json!({
                "id": block.get("id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": block.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": serde_json::to_string(
                        block.get("input").unwrap_or(&Value::Null)
                    ).unwrap_or_default(),
                }
            })
        })
        .collect();

    let text: String = content
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect();

    let content_val = if text.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    };
    normalize_message_tool_calls(&serde_json::json!({
        "role": "assistant",
        "content": content_val,
        "tool_calls": tool_calls,
    }))
}

fn build_responses_assistant_message(output: &[Value]) -> Value {
    let tool_calls: Vec<Value> = output
        .iter()
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("function_call"))
        .map(|item| {
            serde_json::json!({
                "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": item.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}"),
                }
            })
        })
        .collect();

    normalize_message_tool_calls(&serde_json::json!({
        "role": "assistant",
        "content": null,
        "tool_calls": tool_calls,
    }))
}

// ---------------------------------------------------------------------------
// Image stripping for tape persistence
// ---------------------------------------------------------------------------

/// Strip image content blocks from a user message before tape persistence.
/// Replaces `image_base64`, `image`, and `image_url` blocks with a text
/// placeholder `[image: filename]`. Non-user messages and string-content
/// messages pass through unchanged.
pub(super) fn strip_image_blocks_for_persistence(message: &Value) -> Value {
    let role = message.get("role").and_then(|v| v.as_str());
    if role != Some("user") {
        return message.clone();
    }

    let Some(content) = message.get("content").and_then(|v| v.as_array()) else {
        return message.clone();
    };

    let mut img_index = 0u32;
    let replaced: Vec<Value> = content
        .iter()
        .map(|block| {
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match block_type {
                "image_base64" | "image" | "image_url" => {
                    let filename = extract_image_filename(block, img_index);
                    img_index += 1;
                    serde_json::json!({"type": "text", "text": format!("[image: {filename}]")})
                }
                _ => block.clone(),
            }
        })
        .collect();

    let mut msg = message.clone();
    msg["content"] = Value::Array(replaced);
    msg
}

/// Try to derive a filename from an image content block.
fn extract_image_filename(block: &Value, index: u32) -> String {
    // Try common locations for mime_type
    let mime = block
        .get("mime_type")
        .or_else(|| block.get("source").and_then(|s| s.get("media_type")))
        .and_then(|v| v.as_str())
        .unwrap_or("image/png");
    let ext = mime.rsplit('/').next().unwrap_or("png");
    format!("image_{index}.{ext}")
}
