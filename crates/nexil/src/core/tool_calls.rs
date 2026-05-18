//! Canonical tool-call helpers shared across transports and tape history.

use serde_json::{Map, Value};

use crate::clients::parsing::common::expand_tool_calls;

const DSML_TOOL_CALLS_OPEN: &str = "<｜DSML｜tool_calls>";
const DSML_TOOL_CALLS_CLOSE: &str = "</｜DSML｜tool_calls>";
const DSML_INVOKE_OPEN: &str = "<｜DSML｜invoke";
const DSML_INVOKE_CLOSE: &str = "</｜DSML｜invoke>";
const DSML_PARAMETER_OPEN: &str = "<｜DSML｜parameter";
const DSML_PARAMETER_CLOSE: &str = "</｜DSML｜parameter>";

pub(crate) fn normalize_tool_calls(calls: &[Value]) -> Vec<Value> {
    let normalized: Vec<Value> = calls
        .iter()
        .enumerate()
        .filter_map(|(index, call)| {
            normalize_tool_call_with_fallback(call, Some(format!("call_{}", index + 1)))
        })
        .collect();
    expand_tool_calls(normalized)
}

pub(crate) fn normalize_message_tool_calls(message: &Value) -> Value {
    let Some(obj) = message.as_object() else {
        return message.clone();
    };
    if let Some(raw_calls) = obj.get("tool_calls").and_then(|value| value.as_array()) {
        return message_with_tool_calls(obj, normalize_tool_calls(raw_calls));
    }
    let calls = parse_dsml_tool_calls(
        obj.get("content")
            .and_then(|value| value.as_str())
            .unwrap_or(""),
    );
    if calls.is_empty() {
        return message.clone();
    }
    message_with_dsml_tool_calls(obj, calls)
}

pub(crate) fn parse_dsml_tool_calls(text: &str) -> Vec<Value> {
    let Some(body) = extract_between(text, DSML_TOOL_CALLS_OPEN, DSML_TOOL_CALLS_CLOSE) else {
        return Vec::new();
    };
    let calls: Vec<Value> = extract_blocks(body, DSML_INVOKE_OPEN, DSML_INVOKE_CLOSE)
        .into_iter()
        .enumerate()
        .filter_map(|(index, block)| parse_dsml_invoke(block, index))
        .collect();
    normalize_tool_calls(&calls)
}

pub(crate) fn strip_dsml_tool_call_block(text: &str) -> String {
    let Some(start) = text.find(DSML_TOOL_CALLS_OPEN) else {
        return text.to_owned();
    };
    let tail = &text[start + DSML_TOOL_CALLS_OPEN.len()..];
    let Some(end) = tail.find(DSML_TOOL_CALLS_CLOSE) else {
        return text.to_owned();
    };
    let after = start + DSML_TOOL_CALLS_OPEN.len() + end + DSML_TOOL_CALLS_CLOSE.len();
    format!("{}{}", &text[..start], &text[after..])
}

fn message_with_tool_calls(obj: &Map<String, Value>, calls: Vec<Value>) -> Value {
    let mut normalized = obj.clone();
    normalized.insert("tool_calls".to_owned(), Value::Array(calls));
    Value::Object(normalized)
}

fn message_with_dsml_tool_calls(obj: &Map<String, Value>, calls: Vec<Value>) -> Value {
    let mut normalized = obj.clone();
    normalized.insert("tool_calls".to_owned(), Value::Array(calls));
    normalized.insert("content".to_owned(), dsml_message_content(obj));
    Value::Object(normalized)
}

fn dsml_message_content(obj: &Map<String, Value>) -> Value {
    let text = obj
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let stripped = strip_dsml_tool_call_block(text);
    match stripped.trim() {
        "" => Value::Null,
        content => Value::String(content.to_owned()),
    }
}

pub(crate) fn tool_call_id(call: &Value) -> Option<&str> {
    call.get("id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            call.get("call_id")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
        })
}

pub(crate) fn tool_call_name(call: &Value) -> Option<&str> {
    call.get("function")
        .and_then(|value| value.get("name"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            call.get("name")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
        })
}

pub(crate) fn tool_call_arguments_string(call: &Value) -> String {
    call.get("function")
        .and_then(|value| value.get("arguments"))
        .map(json_field_to_string)
        .or_else(|| call.get("arguments").map(json_field_to_string))
        .or_else(|| call.get("input").map(json_field_to_string))
        .unwrap_or_else(|| "{}".to_owned())
}

fn normalize_tool_call_with_fallback(call: &Value, fallback_id: Option<String>) -> Option<Value> {
    let name = tool_call_name(call)?;
    let arguments = tool_call_arguments_string(call);

    let mut function = Map::new();
    function.insert("name".to_owned(), Value::String(name.to_owned()));
    function.insert("arguments".to_owned(), Value::String(arguments));

    let mut entry = Map::new();
    if let Some(id) = tool_call_id(call)
        .map(ToOwned::to_owned)
        .or(fallback_id)
        .filter(|value| !value.is_empty())
    {
        entry.insert("id".to_owned(), Value::String(id));
    }
    entry.insert("type".to_owned(), Value::String("function".to_owned()));
    entry.insert("function".to_owned(), Value::Object(function));

    Some(Value::Object(entry))
}

fn json_field_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_owned()),
    }
}

fn extract_between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let tail = &text[start..];
    let end = tail.find(close)?;
    Some(&tail[..end])
}

fn extract_blocks<'a>(mut text: &'a str, open: &str, close: &str) -> Vec<&'a str> {
    let mut blocks = Vec::new();
    while let Some(start) = text.find(open) {
        let block_start = &text[start..];
        let Some(end) = block_start.find(close) else {
            break;
        };
        blocks.push(&block_start[..end + close.len()]);
        text = &block_start[end + close.len()..];
    }
    blocks
}

fn parse_dsml_invoke(block: &str, index: usize) -> Option<Value> {
    let tag = opening_tag(block)?;
    let name = attr_value(tag, "name")?;
    let params = parse_dsml_parameters(element_body(block, DSML_INVOKE_CLOSE)?);
    Some(serde_json::json!({
        "type": "function_call",
        "call_id": format!("call_{}", index + 1),
        "name": name,
        "arguments": Value::Object(params),
    }))
}

fn parse_dsml_parameters(body: &str) -> Map<String, Value> {
    let mut params = Map::new();
    for block in extract_blocks(body, DSML_PARAMETER_OPEN, DSML_PARAMETER_CLOSE) {
        if let Some((name, value)) = parse_dsml_parameter(block) {
            params.insert(name, value);
        }
    }
    params
}

fn parse_dsml_parameter(block: &str) -> Option<(String, Value)> {
    let tag = opening_tag(block)?;
    let name = attr_value(tag, "name")?;
    let string_attr = attr_value(tag, "string").unwrap_or_else(|| "true".to_owned());
    let raw = element_body(block, DSML_PARAMETER_CLOSE)?;
    Some((name, parse_dsml_parameter_value(raw, &string_attr)))
}

fn parse_dsml_parameter_value(raw: &str, string_attr: &str) -> Value {
    if string_attr.eq_ignore_ascii_case("false") {
        return serde_json::from_str(raw.trim()).unwrap_or_else(|_| Value::String(raw.to_owned()));
    }
    Value::String(raw.to_owned())
}

fn opening_tag(block: &str) -> Option<&str> {
    block.find('>').map(|end| &block[..=end])
}

fn element_body<'a>(block: &'a str, close: &str) -> Option<&'a str> {
    let start = block.find('>')? + 1;
    let tail = &block[start..];
    let end = tail.find(close)?;
    Some(&tail[..end])
}

fn attr_value(tag: &str, name: &str) -> Option<String> {
    let marker = format!(r#"{name}=""#);
    let value = tag.split_once(&marker)?.1.split_once('"')?.0;
    Some(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_normalize_tool_calls_canonicalizes_responses_shape() {
        let calls = normalize_tool_calls(&[json!({
            "type": "function_call",
            "call_id": "call_123",
            "name": "tape_info",
            "arguments": "{}"
        })]);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_123");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "tape_info");
        assert_eq!(calls[0]["function"]["arguments"], "{}");
    }

    #[test]
    fn test_normalize_tool_calls_canonicalizes_anthropic_shape() {
        let calls = normalize_tool_calls(&[json!({
            "type": "tool_use",
            "id": "toolu_123",
            "name": "fs.read",
            "input": {"path": "AGENTS.md"}
        })]);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "toolu_123");
        assert_eq!(calls[0]["function"]["name"], "fs.read");
        assert_eq!(
            calls[0]["function"]["arguments"].as_str().unwrap(),
            r#"{"path":"AGENTS.md"}"#
        );
    }

    #[test]
    fn test_normalize_message_tool_calls_rewrites_message_payload() {
        let message = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "echo",
                "arguments": "{\"msg\":\"hi\"}"
            }]
        });

        let normalized = normalize_message_tool_calls(&message);

        assert_eq!(normalized["tool_calls"][0]["id"], "call_1");
        assert_eq!(normalized["tool_calls"][0]["function"]["name"], "echo");
    }

    #[test]
    fn test_parse_dsml_tool_calls_extracts_v4_tags() {
        let calls = parse_dsml_tool_calls(
            r#"<｜DSML｜tool_calls>
<｜DSML｜invoke name="get_weather">
<｜DSML｜parameter name="location" string="true">杭州</｜DSML｜parameter>
<｜DSML｜parameter name="days" string="false">3</｜DSML｜parameter>
</｜DSML｜invoke>
</｜DSML｜tool_calls>"#,
        );

        let args: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(args, json!({"location": "杭州", "days": 3}));
    }

    #[test]
    fn test_parse_dsml_tool_calls_handles_multiple_invokes() {
        let calls = parse_dsml_tool_calls(
            r#"<｜DSML｜tool_calls>
<｜DSML｜invoke name="first"></｜DSML｜invoke>
<｜DSML｜invoke name="second"></｜DSML｜invoke>
</｜DSML｜tool_calls>"#,
        );

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["name"], "first");
        assert_eq!(calls[1]["id"], "call_2");
    }

    #[test]
    fn test_normalize_message_tool_calls_rewrites_dsml_content() {
        let message = json!({
            "role": "assistant",
            "reasoning_content": "Need a tool.",
            "content": "Checking\n<｜DSML｜tool_calls><｜DSML｜invoke name=\"echo\"></｜DSML｜invoke></｜DSML｜tool_calls>"
        });

        let normalized = normalize_message_tool_calls(&message);

        assert_eq!(normalized["content"], "Checking");
        assert_eq!(normalized["reasoning_content"], "Need a tool.");
        assert_eq!(normalized["tool_calls"][0]["function"]["name"], "echo");
    }
}
